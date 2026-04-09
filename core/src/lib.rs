use std::path::{Path, PathBuf};

pub fn find_videos(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
	let mut videos = Vec::new();
	if dir.is_dir() {
		for entry in std::fs::read_dir(dir)? {
			let entry = entry?;
			let path = entry.path();
			if path.is_dir() {
				// Recursively find videos
				if let Ok(mut sub_videos) = find_videos(&path) {
					videos.append(&mut sub_videos);
				}
			} else {
				if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
					let ext = ext.to_lowercase();
					if matches!(
						ext.as_str(),
						"mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "m4v"
					) {
						videos.push(path);
					}
				}
			}
		}
	}
	Ok(videos)
}
