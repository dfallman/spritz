use std::path::{Path, PathBuf};

pub fn find_media(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
	let mut found = Vec::new();
	if dir.is_dir() {
		for entry in std::fs::read_dir(dir)? {
			let entry = entry?;
			let path = entry.path();
			if path.is_dir() {
				found.append(&mut find_media(&path)?);
			} else if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
				if mime_for_ext(ext).is_some() {
					found.push(path);
				}
			}
		}
	}
	Ok(found)
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

/// Returns the MIME type for a file extension, or None if the extension isn't
/// a supported media format. Also doubles as the "is this a media file?" test
/// used by `find_media`.
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
		_ => return None,
	})
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
