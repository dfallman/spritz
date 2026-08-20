use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use walkdir::WalkDir;

mod meta;
pub use meta::{
	AudioCodec, MediaInfo, VideoCodec, album_art_sidecar, dlna_org_pn, dlna_org_pn_for,
	format_dlna_duration, has_embedded_art, probe_duration, probe_media, protocol_info,
	protocol_info_pn,
};

/// Directory names we never descend into while indexing. NAS thumbnail
/// stores, recycle bins, and VCS metadata are a common source of permission
/// errors and junk "media" on a share.
const JUNK_DIR_NAMES: &[&str] = &[
	".git",
	".Trash",
	".Trashes",
	"@eaDir",
	"@Recycle",
	"#recycle",
	"$RECYCLE.BIN",
	"lost+found",
];

fn is_junk_dir_name(name: &OsStr) -> bool {
	let Some(s) = name.to_str() else {
		return false;
	};
	JUNK_DIR_NAMES.iter().any(|j| s.eq_ignore_ascii_case(j))
}

pub fn find_media(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
	let mut found = Vec::new();
	if !dir.is_dir() {
		return Ok(found);
	}

	let walker = WalkDir::new(dir)
		.follow_links(false)
		.into_iter()
		.filter_entry(|e| {
			e.depth() == 0 || !(e.file_type().is_dir() && is_junk_dir_name(e.file_name()))
		});

	for entry in walker {
		let entry = match entry {
			Ok(e) => e,
			Err(e) => {
				tracing::warn!("skipping during media scan: {e}");
				continue;
			}
		};
		if !entry.file_type().is_file() {
			continue;
		}
		let path = entry.into_path();
		if let Some(ext) = path.extension().and_then(|s| s.to_str())
			&& is_indexed_media(ext)
		{
			found.push(path);
		}
	}
	sort_media_paths(&mut found);
	Ok(found)
}

pub fn sort_media_paths(files: &mut [PathBuf]) {
	files.sort_by(|a, b| {
		let an = a
			.file_name()
			.map(|n| n.to_string_lossy().to_lowercase())
			.unwrap_or_default();
		let bn = b
			.file_name()
			.map(|n| n.to_string_lossy().to_lowercase())
			.unwrap_or_default();
		an.cmp(&bn).then_with(|| a.cmp(b))
	});
}

pub fn is_audio(path: &Path) -> bool {
	path.extension()
		.and_then(|e| e.to_str())
		.and_then(mime_for_ext)
		.map(|m| m.starts_with("audio/"))
		.unwrap_or(false)
}

/// Canonicalize source directories, drop duplicates, and drop any root that
/// sits inside another listed root so the same files are not indexed twice.
pub fn unique_canonical_roots(dirs: &[PathBuf]) -> Vec<PathBuf> {
	let mut canonical = Vec::new();
	for dir in dirs {
		match dir.canonicalize() {
			Ok(p) => {
				if !canonical.contains(&p) {
					canonical.push(p);
				}
			}
			Err(e) => tracing::warn!("could not canonicalize {}: {e}", dir.display()),
		}
	}

	let mut roots: Vec<PathBuf> = Vec::new();
	for candidate in canonical {
		if roots.iter().any(|existing| candidate.starts_with(existing)) {
			tracing::info!(
				"skipping nested source {} (already covered)",
				candidate.display()
			);
			continue;
		}
		roots.retain(|existing| {
			if existing.starts_with(&candidate) {
				tracing::info!(
					"dropping {} because {} is a parent source",
					existing.display(),
					candidate.display()
				);
				false
			} else {
				true
			}
		});
		roots.push(candidate);
	}
	roots
}

/// Given a media file's absolute path and the list of root directories being served,
/// returns `(dir_index, url-encoded relative path)` for building `/m/{i}/{path}` URLs.
pub fn media_url_path(media: &Path, dirs: &[PathBuf]) -> Option<(usize, String)> {
	for (i, dir) in dirs.iter().enumerate() {
		if let Ok(relative) = media.strip_prefix(dir) {
			return Some((i, encode_path(relative)));
		}
	}
	None
}

pub fn encode_path(path: &Path) -> String {
	#[cfg(windows)]
	let s = path.to_string_lossy().replace('\\', "/");
	#[cfg(not(windows))]
	let s = path.to_string_lossy().to_string();

	s.split('/')
		.map(|seg| urlencoding::encode(seg).into_owned())
		.collect::<Vec<_>>()
		.join("/")
}

/// `dc:date` for DIDL. Date-only ISO-8601; Samsung requires the element to exist.
pub fn dc_date(mtime: std::time::SystemTime) -> String {
	let secs = mtime
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0);
	let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
	format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant civil_from_days: `z` is days since 1970-01-01.
fn civil_from_days(z: i64) -> (i32, u8, u8) {
	let z = z + 719_468;
	let era = z.div_euclid(146_097);
	let doe = z.rem_euclid(146_097) as u64;
	let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
	let y = yoe as i64 + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
	let mp = (5 * doy + 2) / 153;
	let d = doy - (153 * mp + 2) / 5 + 1;
	let m = if mp < 10 { mp + 3 } else { mp - 9 };
	let y = if m <= 2 { y + 1 } else { y };
	(y as i32, m as u8, d as u8)
}

/// Host header used in M3U / DIDL URLs: hostname or host:port, no slashes or spaces.
pub fn valid_http_host(host: &str) -> bool {
	let host = host.trim();
	!host.is_empty()
		&& host
			.chars()
			.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']' | '_'))
}

/// `host:port` for IPv4, `[host]:port` for IPv6. Port 80 omits the port.
pub fn format_http_authority(ip: std::net::IpAddr, port: u16) -> String {
	match ip {
		std::net::IpAddr::V4(v) if port == 80 => v.to_string(),
		std::net::IpAddr::V4(v) => format!("{v}:{port}"),
		std::net::IpAddr::V6(v) if port == 80 => format!("[{v}]"),
		std::net::IpAddr::V6(v) => format!("[{v}]:{port}"),
	}
}

/// Resolve `relative` under `root` without following symlinks or leaving the tree.
/// Only extensions with a known MIME type are allowed (media and sidecar subtitles).
pub fn safe_media_path(root: &Path, relative: &Path) -> Option<PathBuf> {
	let mut cur = root.to_path_buf();
	for c in relative.components() {
		match c {
			Component::Normal(name) => {
				cur.push(name);
				let meta = std::fs::symlink_metadata(&cur).ok()?;
				if meta.file_type().is_symlink() {
					return None;
				}
			}
			Component::CurDir => {}
			_ => return None,
		}
	}
	if !cur.starts_with(root) {
		return None;
	}
	let ext = cur.extension().and_then(|e| e.to_str())?;
	mime_for_ext(ext)?;
	cur.is_file().then_some(cur)
}

/// Returns the MIME type for a file extension, or None if Spritz will not
/// serve it. Sidecar subtitles are servable but not indexed as library items
/// (`is_indexed_media`).
pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
	Some(match ext.to_ascii_lowercase().as_str() {
		// Video
		"mp4" | "m4v" => "video/mp4",
		"mkv" => "video/x-matroska",
		"avi" => "video/x-msvideo",
		"mov" => "video/quicktime",
		"webm" => "video/webm",
		"flv" => "video/x-flv",
		// Audio
		"mp3" => "audio/mpeg",
		"m4a" => "audio/mp4",
		"aac" => "audio/aac",
		"flac" => "audio/flac",
		"ogg" | "oga" => "audio/ogg",
		"opus" => "audio/ogg",
		"wav" => "audio/wav",
		"wma" => "audio/x-ms-wma",
		"aiff" | "aif" => "audio/aiff",
		"srt" => "text/srt",
		"vtt" => "text/vtt",
		"ass" | "ssa" => "text/x-ssa",
		"jpg" | "jpeg" => "image/jpeg",
		"png" => "image/png",
		_ => return None,
	})
}

/// True for playable video/audio that belongs in the library. Sidecar
/// subtitles are servable over HTTP but are not listed as items.
pub fn is_indexed_media(ext: &str) -> bool {
	mime_for_ext(ext).is_some_and(|m| m.starts_with("video/") || m.starts_with("audio/"))
}

/// The full list of MIME types Spritz is willing to serve. Used by
/// `GetProtocolInfo` to advertise capability to DLNA clients.
pub const ALL_MIMES: &[&str] = &[
	"video/mp4",
	"video/x-matroska",
	"video/x-msvideo",
	"video/quicktime",
	"video/webm",
	"video/x-flv",
	"audio/mpeg",
	"audio/mp4",
	"audio/aac",
	"audio/flac",
	"audio/ogg",
	"audio/wav",
	"audio/x-ms-wma",
	"audio/aiff",
];

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;

	fn names(paths: &[PathBuf]) -> Vec<String> {
		let mut n: Vec<String> = paths
			.iter()
			.map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
			.collect();
		n.sort();
		n
	}

	#[test]
	fn find_media_indexes_supported_extensions_only() {
		let tmp = tempfile::tempdir().unwrap();
		fs::write(tmp.path().join("movie.mp4"), b"x").unwrap();
		fs::write(tmp.path().join("song.mp3"), b"x").unwrap();
		fs::write(tmp.path().join("notes.txt"), b"x").unwrap();
		fs::create_dir(tmp.path().join("sub")).unwrap();
		fs::write(tmp.path().join("sub").join("clip.mkv"), b"x").unwrap();

		let found = find_media(tmp.path()).unwrap();
		assert_eq!(names(&found), ["clip.mkv", "movie.mp4", "song.mp3"]);
	}

	#[test]
	fn find_media_does_not_index_sidecar_subtitles() {
		let tmp = tempfile::tempdir().unwrap();
		fs::write(tmp.path().join("movie.mp4"), b"x").unwrap();
		fs::write(tmp.path().join("movie.srt"), b"1").unwrap();
		fs::write(tmp.path().join("movie.vtt"), b"WEBVTT").unwrap();
		let found = find_media(tmp.path()).unwrap();
		assert_eq!(names(&found), ["movie.mp4"]);
	}

	#[test]
	fn find_media_sorts_by_file_name() {
		let tmp = tempfile::tempdir().unwrap();
		fs::write(tmp.path().join("zeta.mp4"), b"x").unwrap();
		fs::write(tmp.path().join("alpha.mp4"), b"x").unwrap();
		fs::create_dir(tmp.path().join("sub")).unwrap();
		fs::write(tmp.path().join("sub").join("middle.mp4"), b"x").unwrap();

		let found = find_media(tmp.path()).unwrap();
		let names: Vec<_> = found
			.iter()
			.map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
			.collect();
		assert_eq!(names, ["alpha.mp4", "middle.mp4", "zeta.mp4"]);
	}

	#[cfg(unix)]
	#[test]
	fn find_media_does_not_follow_directory_symlinks() {
		let tmp = tempfile::tempdir().unwrap();
		let real = tmp.path().join("real");
		fs::create_dir(&real).unwrap();
		fs::write(real.join("inside.mp4"), b"x").unwrap();
		fs::write(tmp.path().join("top.mp4"), b"x").unwrap();
		std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();

		let found = find_media(tmp.path()).unwrap();
		assert_eq!(names(&found), ["inside.mp4", "top.mp4"]);
	}

	#[cfg(unix)]
	#[test]
	fn find_media_skips_unreadable_subdirectories() {
		use std::os::unix::fs::PermissionsExt;

		let tmp = tempfile::tempdir().unwrap();
		fs::write(tmp.path().join("ok.mp4"), b"x").unwrap();
		let locked = tmp.path().join("locked");
		fs::create_dir(&locked).unwrap();
		fs::write(locked.join("secret.mp4"), b"x").unwrap();
		fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

		let found = find_media(tmp.path());
		let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
		let found = found.expect("unreadable subdir should not fail the whole walk");
		assert_eq!(names(&found), ["ok.mp4"]);
	}

	#[test]
	fn find_media_skips_junk_directories() {
		let tmp = tempfile::tempdir().unwrap();
		fs::write(tmp.path().join("keep.mp3"), b"x").unwrap();
		for junk in [".git", ".Trash", "@eaDir", "#recycle"] {
			let dir = tmp.path().join(junk);
			fs::create_dir(&dir).unwrap();
			fs::write(dir.join("skip.mp4"), b"x").unwrap();
		}

		let found = find_media(tmp.path()).unwrap();
		assert_eq!(names(&found), ["keep.mp3"]);
	}

	#[test]
	fn unique_canonical_roots_drops_nested_and_duplicate_paths() {
		let tmp = tempfile::tempdir().unwrap();
		let movies = tmp.path().join("movies");
		let kids = movies.join("kids");
		fs::create_dir_all(&kids).unwrap();

		let roots = unique_canonical_roots(&[movies.clone(), kids, movies.join(".")]);

		assert_eq!(roots.len(), 1);
		assert_eq!(roots[0], movies.canonicalize().unwrap());
	}

	#[test]
	fn unique_canonical_roots_keeps_sibling_directories() {
		let tmp = tempfile::tempdir().unwrap();
		let movies = tmp.path().join("movies");
		let music = tmp.path().join("music");
		fs::create_dir(&movies).unwrap();
		fs::create_dir(&music).unwrap();

		let roots = unique_canonical_roots(&[movies.clone(), music.clone()]);
		assert_eq!(
			roots,
			[
				movies.canonicalize().unwrap(),
				music.canonicalize().unwrap()
			]
		);
	}

	#[test]
	fn mime_for_ext_is_case_insensitive_and_rejects_unknown() {
		assert_eq!(mime_for_ext("MP4"), Some("video/mp4"));
		assert_eq!(mime_for_ext("mkv"), Some("video/x-matroska"));
		assert_eq!(mime_for_ext("mp3"), Some("audio/mpeg"));
		assert_eq!(mime_for_ext("jpg"), Some("image/jpeg"));
		assert_eq!(mime_for_ext("txt"), None);
		assert_eq!(mime_for_ext(""), None);
	}

	#[test]
	fn mime_for_ext_serves_sidecar_subtitles() {
		assert_eq!(mime_for_ext("srt"), Some("text/srt"));
		assert_eq!(mime_for_ext("vtt"), Some("text/vtt"));
		assert_eq!(mime_for_ext("ass"), Some("text/x-ssa"));
		assert!(!is_indexed_media("srt"));
		assert!(!is_indexed_media("jpg"));
		assert!(is_indexed_media("mp4"));
	}

	#[test]
	fn encode_path_percent_encodes_segments() {
		assert_eq!(encode_path(Path::new("My Movie.mp4")), "My%20Movie.mp4");
		assert_eq!(
			encode_path(Path::new("shows/S01/ep 1.mkv")),
			"shows/S01/ep%201.mkv"
		);
		assert_eq!(encode_path(Path::new("å.mp3")), "%C3%A5.mp3");
	}

	#[test]
	fn media_url_path_uses_the_first_matching_prefix() {
		let dirs = vec![PathBuf::from("/media"), PathBuf::from("/media/movies")];
		let file = Path::new("/media/movies/a.mp4");
		assert_eq!(
			media_url_path(file, &dirs),
			Some((0, "movies/a.mp4".into()))
		);
		assert_eq!(media_url_path(Path::new("/other/a.mp4"), &dirs), None);
	}

	#[test]
	fn dc_date_formats_unix_epoch() {
		assert_eq!(dc_date(std::time::SystemTime::UNIX_EPOCH), "1970-01-01");
	}

	#[test]
	fn valid_http_host_rejects_injection() {
		assert!(valid_http_host("10.0.0.8:9000"));
		assert!(valid_http_host("example.local"));
		assert!(valid_http_host("[2001:db8::1]:8080"));
		assert!(!valid_http_host(""));
		assert!(!valid_http_host("evil.com/path"));
		assert!(!valid_http_host("evil.com\\path"));
		assert!(!valid_http_host("host with space"));
		assert!(!valid_http_host("host\r\nX-Injected: 1"));
	}

	#[test]
	fn format_http_authority_brackets_ipv6() {
		assert_eq!(
			format_http_authority("192.0.2.1".parse().unwrap(), 8080),
			"192.0.2.1:8080"
		);
		assert_eq!(
			format_http_authority("192.0.2.1".parse().unwrap(), 80),
			"192.0.2.1"
		);
		assert_eq!(
			format_http_authority("2001:db8::1".parse().unwrap(), 8080),
			"[2001:db8::1]:8080"
		);
		assert_eq!(
			format_http_authority("2001:db8::1".parse().unwrap(), 80),
			"[2001:db8::1]"
		);
	}

	#[test]
	fn safe_media_path_allows_media_under_root() {
		let tmp = tempfile::tempdir().unwrap();
		fs::create_dir(tmp.path().join("sub")).unwrap();
		fs::write(tmp.path().join("sub").join("clip.mp4"), b"x").unwrap();
		let resolved = safe_media_path(tmp.path(), Path::new("sub/clip.mp4")).unwrap();
		assert_eq!(resolved, tmp.path().join("sub").join("clip.mp4"));
	}

	#[test]
	fn safe_media_path_rejects_parent_dir_and_non_media() {
		let tmp = tempfile::tempdir().unwrap();
		fs::write(tmp.path().join("notes.txt"), b"x").unwrap();
		assert!(safe_media_path(tmp.path(), Path::new("../clip.mp4")).is_none());
		assert!(safe_media_path(tmp.path(), Path::new("notes.txt")).is_none());
	}

	#[cfg(unix)]
	#[test]
	fn safe_media_path_rejects_symlinks() {
		let tmp = tempfile::tempdir().unwrap();
		let target = tmp.path().join("target.mp4");
		fs::write(&target, b"x").unwrap();
		std::os::unix::fs::symlink(&target, tmp.path().join("link.mp4")).unwrap();
		assert!(safe_media_path(tmp.path(), Path::new("link.mp4")).is_none());
	}
}
