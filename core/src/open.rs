use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Component, Path};

use crate::mime_for_ext;

/// Open `relative` under `root` and return the file plus its MIME type.
///
/// On Unix every component is opened with `openat` and `O_NOFOLLOW`, so a
/// symlink swapped in after a path check cannot retarget the file. The
/// returned handle is that inode.
pub fn open_media_file(root: &Path, relative: &Path) -> Option<(File, &'static str)> {
	let mut dirs = Vec::new();
	for component in relative.components() {
		match component {
			Component::Normal(name) => dirs.push(name),
			Component::CurDir => {}
			_ => return None,
		}
	}
	let file_name = dirs.pop()?;
	if file_name.is_empty() || file_name.as_encoded_bytes().contains(&0) {
		return None;
	}
	let ext = Path::new(file_name)
		.extension()
		.and_then(|ext| ext.to_str())?;
	let mime = mime_for_ext(ext)?;
	let file = open_nofollow(root, &dirs, file_name).ok()?;
	Some((file, mime))
}

#[cfg(unix)]
fn open_nofollow(root: &Path, dirs: &[&OsStr], file_name: &OsStr) -> io::Result<File> {
	use rustix::fs::{FileType, Mode, OFlags, fstat, open, openat};
	use std::os::fd::OwnedFd;

	let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
	let mut dir: OwnedFd = open(root, flags | OFlags::DIRECTORY, Mode::empty())?;
	for name in dirs {
		dir = openat(&dir, *name, flags | OFlags::DIRECTORY, Mode::empty())?;
	}
	// NONBLOCK so a fifo named like a media file cannot stall the open.
	let file = openat(&dir, file_name, flags | OFlags::NONBLOCK, Mode::empty())?;
	let stat = fstat(&file)?;
	if !FileType::from_raw_mode(stat.st_mode).is_file() {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			"not a regular file",
		));
	}
	Ok(File::from(file))
}

#[cfg(windows)]
fn open_nofollow(root: &Path, dirs: &[&OsStr], file_name: &OsStr) -> io::Result<File> {
	use std::fs::OpenOptions;
	use std::os::windows::fs::OpenOptionsExt;

	// Last-component reparse points (symlinks) are opened as themselves and
	// rejected. Intermediate directory symlinks are still followed by the
	// Win32 path walk; Unix uses openat instead.
	const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
	let mut path = root.to_path_buf();
	for name in dirs {
		path.push(name);
	}
	path.push(file_name);
	let file = OpenOptions::new()
		.read(true)
		.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
		.open(&path)?;
	let meta = file.metadata()?;
	if meta.file_type().is_symlink() || !meta.is_file() {
		return Err(io::Error::new(
			io::ErrorKind::InvalidInput,
			"not a regular file",
		));
	}
	Ok(file)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Read;

	#[test]
	fn open_media_file_reads_the_file_that_was_opened() {
		let tmp = tempfile::tempdir().unwrap();
		std::fs::create_dir(tmp.path().join("sub")).unwrap();
		std::fs::write(tmp.path().join("sub").join("clip.mp4"), b"hello").unwrap();
		let (mut file, mime) = open_media_file(tmp.path(), Path::new("sub/clip.mp4")).unwrap();
		assert_eq!(mime, "video/mp4");
		let mut buf = String::new();
		file.read_to_string(&mut buf).unwrap();
		assert_eq!(buf, "hello");
	}

	#[test]
	fn open_media_file_rejects_parent_dir_and_non_media() {
		let tmp = tempfile::tempdir().unwrap();
		std::fs::write(tmp.path().join("notes.txt"), b"x").unwrap();
		assert!(open_media_file(tmp.path(), Path::new("../clip.mp4")).is_none());
		assert!(open_media_file(tmp.path(), Path::new("notes.txt")).is_none());
	}

	#[cfg(unix)]
	#[test]
	fn open_media_file_does_not_follow_file_or_directory_symlinks() {
		let tmp = tempfile::tempdir().unwrap();
		let root = tmp.path().join("root");
		std::fs::create_dir(&root).unwrap();
		let outside = tmp.path().join("secret.mp4");
		std::fs::write(&outside, b"secret").unwrap();
		std::os::unix::fs::symlink(&outside, root.join("link.mp4")).unwrap();
		std::os::unix::fs::symlink(tmp.path(), root.join("dirlink")).unwrap();

		assert!(open_media_file(&root, Path::new("link.mp4")).is_none());
		assert!(open_media_file(&root, Path::new("dirlink/secret.mp4")).is_none());
	}
}
