use std::path::{Path, PathBuf};

pub fn find_videos(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
	let mut videos = Vec::new();
	if dir.is_dir() {
		for entry in std::fs::read_dir(dir)? {
			let entry = entry?;
			let path = entry.path();
			if path.is_dir() {
				videos.append(&mut find_videos(&path)?);
			} else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
				let ext = ext.to_lowercase();
				if matches!(ext.as_str(), "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "m4v") {
					videos.push(path);
				}
			}
		}
	}
	Ok(videos)
}

/// Given a video's absolute path and the list of root directories being served,
/// returns `(dir_index, url-encoded relative path)` for building `/v/{i}/{path}` URLs.
pub fn video_url_path(video: &Path, dirs: &[PathBuf]) -> Option<(usize, String)> {
	for (i, dir) in dirs.iter().enumerate() {
		if let Ok(relative) = video.strip_prefix(dir) {
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
