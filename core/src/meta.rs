use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ART_NAMES: &[&str] = &[
	"cover.jpg",
	"cover.png",
	"folder.jpg",
	"folder.png",
	"album.jpg",
	"poster.jpg",
];

/// DIDL `duration` attribute: `H:MM:SS.mmm`.
pub fn format_dlna_duration(d: Duration) -> String {
	let millis = d.as_millis();
	let hours = millis / 3_600_000;
	let mins = (millis / 60_000) % 60;
	let secs = (millis / 1000) % 60;
	let ms = millis % 1000;
	format!("{hours}:{mins:02}:{secs:02}.{ms:03}")
}

/// Conservative DLNA profile names. Wrong PNs make TVs refuse a file,
/// so video mappings are the common H.264 profiles ReadyMedia uses as defaults.
pub fn dlna_org_pn(ext: &str) -> Option<&'static str> {
	Some(match ext.to_ascii_lowercase().as_str() {
		"mp3" => "MP3",
		"flac" => "FLAC",
		"wav" => "LPCM",
		"aac" => "AAC_ADTS_320",
		"m4a" => "AAC_ISO",
		"mp4" | "m4v" => "AVC_MP4_MP_SD_AAC_MULT5",
		"mov" => "AVC_MP4_MP_SD_AAC_MULT5",
		"mkv" => "AVC_MKV_MP_HD_AC3",
		"jpg" | "jpeg" => "JPEG_LRG",
		"png" => "PNG_LRG",
		_ => return None,
	})
}

pub fn protocol_info(mime: &str, ext: &str, flags: &str) -> String {
	match dlna_org_pn(ext) {
		Some(pn) => format!("http-get:*:{mime}:DLNA.ORG_PN={pn};{flags}"),
		None => format!("http-get:*:{mime}:{flags}"),
	}
}

/// Sidecar cover next to a media file (same stem, or cover/folder/album/poster).
pub fn album_art_sidecar(media: &Path) -> Option<PathBuf> {
	let parent = media.parent()?;
	let stem = media.file_stem()?.to_string_lossy();
	let mut candidates: Vec<PathBuf> = ["jpg", "jpeg", "png"]
		.into_iter()
		.map(|ext| parent.join(format!("{stem}.{ext}")))
		.collect();
	candidates.extend(ART_NAMES.iter().map(|n| parent.join(n)));
	for path in candidates {
		let Ok(meta) = std::fs::symlink_metadata(&path) else {
			continue;
		};
		if !meta.file_type().is_symlink() && meta.is_file() {
			return Some(path);
		}
	}
	None
}

pub fn probe_duration(path: &Path) -> Option<Duration> {
	let ext = path
		.extension()
		.and_then(|e| e.to_str())
		.unwrap_or("")
		.to_ascii_lowercase();
	match ext.as_str() {
		"mp4" | "m4v" | "m4a" | "mov" => duration_from_mp4(path).ok().flatten(),
		"mkv" | "webm" => duration_from_mkv(path).ok().flatten(),
		"wav" => duration_from_wav(path).ok().flatten(),
		"flac" => duration_from_flac(path).ok().flatten(),
		_ => None,
	}
}

pub fn has_embedded_art(_path: &Path) -> bool {
	// Embedded pictures need a tag parser; sidecars cover the common case.
	false
}

fn duration_from_wav(path: &Path) -> std::io::Result<Option<Duration>> {
	let mut f = std::fs::File::open(path)?;
	let mut hdr = [0u8; 12];
	f.read_exact(&mut hdr)?;
	if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
		return Ok(None);
	}
	let mut byte_rate = 0u32;
	loop {
		let mut chunk = [0u8; 8];
		if f.read_exact(&mut chunk).is_err() {
			return Ok(None);
		}
		let size = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
		if &chunk[0..4] == b"fmt " {
			let mut fmt = vec![0u8; size as usize];
			f.read_exact(&mut fmt)?;
			if fmt.len() >= 16 {
				byte_rate = u32::from_le_bytes(fmt[8..12].try_into().unwrap());
			}
			if size % 2 == 1 {
				let _ = f.seek(SeekFrom::Current(1));
			}
		} else if &chunk[0..4] == b"data" {
			if byte_rate == 0 {
				return Ok(None);
			}
			let secs = size as f64 / byte_rate as f64;
			return Ok(Some(Duration::from_secs_f64(secs)));
		} else {
			f.seek(SeekFrom::Current(i64::from(size) + i64::from(size % 2)))?;
		}
	}
}

fn duration_from_flac(path: &Path) -> std::io::Result<Option<Duration>> {
	let mut f = std::fs::File::open(path)?;
	let mut mag = [0u8; 4];
	f.read_exact(&mut mag)?;
	if &mag != b"fLaC" {
		return Ok(None);
	}
	loop {
		let mut head = [0u8; 4];
		f.read_exact(&mut head)?;
		let is_last = head[0] & 0x80 != 0;
		let block_type = head[0] & 0x7f;
		let len = u32::from_be_bytes([0, head[1], head[2], head[3]]);
		if block_type == 0 {
			let mut info = vec![0u8; len as usize];
			f.read_exact(&mut info)?;
			if info.len() < 18 {
				return Ok(None);
			}
			let sr = (u32::from(info[10]) << 12)
				| (u32::from(info[11]) << 4)
				| (u32::from(info[12]) >> 4);
			let total = ((u64::from(info[13]) & 0x0f) << 32)
				| (u64::from(info[14]) << 24)
				| (u64::from(info[15]) << 16)
				| (u64::from(info[16]) << 8)
				| u64::from(info[17]);
			if sr == 0 || total == 0 {
				return Ok(None);
			}
			return Ok(Some(Duration::from_secs_f64(total as f64 / f64::from(sr))));
		}
		f.seek(SeekFrom::Current(i64::from(len)))?;
		if is_last {
			return Ok(None);
		}
	}
}

fn duration_from_mp4(path: &Path) -> std::io::Result<Option<Duration>> {
	let mut data = Vec::new();
	std::fs::File::open(path)?.read_to_end(&mut data)?;
	Ok(mp4_duration_from_bytes(&data))
}

pub(crate) fn mp4_duration_from_bytes(data: &[u8]) -> Option<Duration> {
	fn walk(data: &[u8], mut pos: usize, end: usize, looking_moov: bool) -> Option<Duration> {
		while pos + 8 <= end {
			let size = u32::from_be_bytes(data[pos..pos + 4].try_into().ok()?) as usize;
			let kind = &data[pos + 4..pos + 8];
			let (hdr, payload_size) = if size == 1 {
				if pos + 16 > end {
					return None;
				}
				let large = u64::from_be_bytes(data[pos + 8..pos + 16].try_into().ok()?) as usize;
				(16usize, large.saturating_sub(16))
			} else if size == 0 {
				(8usize, end.saturating_sub(pos + 8))
			} else {
				(8usize, size.saturating_sub(8))
			};
			let payload_start = pos + hdr;
			let payload_end = (payload_start + payload_size).min(end);
			if looking_moov && kind == b"mvhd" {
				return parse_mvhd(&data[payload_start..payload_end]);
			}
			if kind == b"moov"
				&& let Some(d) = walk(data, payload_start, payload_end, true)
			{
				return Some(d);
			}
			if size == 0 {
				break;
			}
			pos = if size == 1 {
				payload_end
			} else {
				pos + size.max(8)
			};
			if pos < payload_start {
				break;
			}
		}
		None
	}
	walk(data, 0, data.len(), false)
}

fn parse_mvhd(payload: &[u8]) -> Option<Duration> {
	if payload.is_empty() {
		return None;
	}
	let version = payload[0];
	if version == 1 {
		if payload.len() < 32 {
			return None;
		}
		let timescale = u32::from_be_bytes(payload[20..24].try_into().ok()?);
		let duration = u64::from_be_bytes(payload[24..32].try_into().ok()?);
		if timescale == 0 {
			return None;
		}
		Some(Duration::from_secs_f64(
			duration as f64 / f64::from(timescale),
		))
	} else {
		if payload.len() < 20 {
			return None;
		}
		let timescale = u32::from_be_bytes(payload[12..16].try_into().ok()?);
		let duration = u32::from_be_bytes(payload[16..20].try_into().ok()?);
		if timescale == 0 {
			return None;
		}
		Some(Duration::from_secs_f64(
			f64::from(duration) / f64::from(timescale),
		))
	}
}

fn duration_from_mkv(path: &Path) -> std::io::Result<Option<Duration>> {
	let mut data = Vec::new();
	std::fs::File::open(path)?.read_to_end(&mut data)?;
	Ok(mkv_duration_from_bytes(&data))
}

pub(crate) fn mkv_duration_from_bytes(data: &[u8]) -> Option<Duration> {
	let mut pos = 0usize;
	let mut timestamp_scale = 1_000_000.0f64;
	let mut duration_ticks: Option<f64> = None;
	// Scan a bounded prefix for Segment/Info — no need to parse Clusters.
	let limit = data.len().min(256 * 1024);
	while pos + 1 < limit {
		let Some((id, id_len)) = ebml_id(&data[pos..limit]) else {
			break;
		};
		pos += id_len;
		let Some((size, size_len)) = ebml_size(&data[pos..limit]) else {
			break;
		};
		pos += size_len;
		let end = (pos + size).min(limit);
		match id {
			0x1549_A966 => {
				// Info
				let mut ip = pos;
				while ip + 1 < end {
					let Some((iid, ilen)) = ebml_id(&data[ip..end]) else {
						break;
					};
					ip += ilen;
					let Some((isize, slen)) = ebml_size(&data[ip..end]) else {
						break;
					};
					ip += slen;
					let iend = (ip + isize).min(end);
					if iid == 0x2AD7B1 {
						timestamp_scale = ebml_uint(&data[ip..iend])? as f64;
					} else if iid == 0x4489 {
						duration_ticks = Some(ebml_float(&data[ip..iend])?);
					}
					ip = iend;
				}
			}
			0x1853_8067 | 0x1A45_DFA3 => {
				// Segment: walk children by continuing; EBML header: skip
				if id == 0x1A45_DFA3 {
					pos = end;
					continue;
				}
				// Stay inside segment without skipping its payload.
				continue;
			}
			_ => {
				pos = end;
				continue;
			}
		}
		pos = end;
		if duration_ticks.is_some() {
			break;
		}
	}
	let ticks = duration_ticks?;
	let nanos = ticks * timestamp_scale;
	if nanos <= 0.0 {
		return None;
	}
	Some(Duration::from_secs_f64(nanos / 1_000_000_000.0))
}

fn ebml_id(data: &[u8]) -> Option<(u32, usize)> {
	let first = *data.first()?;
	let len = match first {
		b if b & 0x80 != 0 => 1,
		b if b & 0x40 != 0 => 2,
		b if b & 0x20 != 0 => 3,
		b if b & 0x10 != 0 => 4,
		_ => return None,
	};
	if data.len() < len {
		return None;
	}
	let mut id = 0u32;
	for b in &data[..len] {
		id = (id << 8) | u32::from(*b);
	}
	Some((id, len))
}

fn ebml_size(data: &[u8]) -> Option<(usize, usize)> {
	let first = *data.first()?;
	let len = first.leading_zeros() as usize + 1;
	if len == 0 || len > 8 || data.len() < len {
		return None;
	}
	let mut val = u64::from(first) & (0xff >> len);
	for b in &data[1..len] {
		val = (val << 8) | u64::from(*b);
	}
	if val == (1u64 << (7 * len)) - 1 {
		return None; // unknown size
	}
	Some((val as usize, len))
}

fn ebml_uint(data: &[u8]) -> Option<u64> {
	if data.is_empty() || data.len() > 8 {
		return None;
	}
	let mut v = 0u64;
	for b in data {
		v = (v << 8) | u64::from(*b);
	}
	Some(v)
}

fn ebml_float(data: &[u8]) -> Option<f64> {
	match data.len() {
		4 => Some(f64::from(f32::from_be_bytes(data.try_into().ok()?))),
		8 => Some(f64::from_be_bytes(data.try_into().ok()?)),
		_ => None,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;

	#[test]
	fn format_dlna_duration_uses_h_mm_ss_mmm() {
		assert_eq!(
			format_dlna_duration(Duration::from_millis(0)),
			"0:00:00.000"
		);
		assert_eq!(
			format_dlna_duration(Duration::from_millis(125_250)),
			"0:02:05.250"
		);
		assert_eq!(
			format_dlna_duration(Duration::from_secs(3661)),
			"1:01:01.000"
		);
	}

	#[test]
	fn dlna_org_pn_maps_common_audio_and_h264_video() {
		assert_eq!(dlna_org_pn("mp3"), Some("MP3"));
		assert_eq!(dlna_org_pn("FLAC"), Some("FLAC"));
		assert_eq!(dlna_org_pn("mp4"), Some("AVC_MP4_MP_SD_AAC_MULT5"));
		assert_eq!(dlna_org_pn("webm"), None);
		assert_eq!(dlna_org_pn("avi"), None);
	}

	#[test]
	fn protocol_info_inserts_pn_when_known() {
		assert_eq!(
			protocol_info("audio/mpeg", "mp3", "DLNA.ORG_OP=01"),
			"http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01"
		);
		assert_eq!(
			protocol_info("video/webm", "webm", "DLNA.ORG_OP=01"),
			"http-get:*:video/webm:DLNA.ORG_OP=01"
		);
	}

	fn box_bytes(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
		let size = (8 + payload.len()) as u32;
		let mut v = size.to_be_bytes().to_vec();
		v.extend(kind);
		v.extend(payload);
		v
	}

	#[test]
	fn mp4_duration_reads_mvhd_timescale() {
		let mut mvhd = vec![0u8; 24];
		mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
		mvhd[16..20].copy_from_slice(&2500u32.to_be_bytes());
		let moov = box_bytes(b"moov", &box_bytes(b"mvhd", &mvhd));
		let ftyp = box_bytes(b"ftyp", b"isom");
		let mut file = ftyp;
		file.extend(moov);
		let d = mp4_duration_from_bytes(&file).unwrap();
		assert_eq!(d, Duration::from_millis(2500));
	}

	fn tiny_wav(duration_secs: u32) -> Vec<u8> {
		let sr = 8000u32;
		let data_size = sr * 2 * duration_secs;
		let mut w = Vec::new();
		w.extend(b"RIFF");
		w.extend(&(36 + data_size).to_le_bytes());
		w.extend(b"WAVEfmt ");
		w.extend(&16u32.to_le_bytes());
		w.extend(&1u16.to_le_bytes());
		w.extend(&1u16.to_le_bytes());
		w.extend(&sr.to_le_bytes());
		w.extend(&(sr * 2).to_le_bytes());
		w.extend(&2u16.to_le_bytes());
		w.extend(&16u16.to_le_bytes());
		w.extend(b"data");
		w.extend(&data_size.to_le_bytes());
		w.extend(vec![0u8; data_size as usize]);
		w
	}

	#[test]
	fn wav_duration_from_byte_rate() {
		let tmp = tempfile::tempdir().unwrap();
		let path = tmp.path().join("beep.wav");
		fs::write(&path, tiny_wav(2)).unwrap();
		assert_eq!(probe_duration(&path), Some(Duration::from_secs(2)));
	}

	#[test]
	fn album_art_sidecar_prefers_matching_stem() {
		let tmp = tempfile::tempdir().unwrap();
		let movie = tmp.path().join("clip.mp4");
		fs::write(&movie, b"x").unwrap();
		fs::write(tmp.path().join("clip.jpg"), b"jpeg").unwrap();
		fs::write(tmp.path().join("cover.jpg"), b"cover").unwrap();
		let art = album_art_sidecar(&movie).unwrap();
		assert_eq!(art.file_name().unwrap(), "clip.jpg");
	}

	#[test]
	fn album_art_sidecar_falls_back_to_cover() {
		let tmp = tempfile::tempdir().unwrap();
		let movie = tmp.path().join("clip.mp4");
		fs::write(&movie, b"x").unwrap();
		fs::write(tmp.path().join("cover.png"), b"png").unwrap();
		let art = album_art_sidecar(&movie).unwrap();
		assert_eq!(art.file_name().unwrap(), "cover.png");
	}

	#[test]
	fn mkv_duration_from_info_float() {
		// Hand-built: EBML header skipped via dummy, Segment + Info + Duration + TimestampScale.
		fn vint_id(id: u32) -> Vec<u8> {
			if id <= 0xff {
				vec![id as u8]
			} else if id <= 0xffff {
				vec![(id >> 8) as u8, id as u8]
			} else if id <= 0xff_ffff {
				vec![(id >> 16) as u8, (id >> 8) as u8, id as u8]
			} else {
				vec![
					(id >> 24) as u8,
					(id >> 16) as u8,
					(id >> 8) as u8,
					id as u8,
				]
			}
		}
		fn sized(id: u32, payload: &[u8]) -> Vec<u8> {
			let mut v = vint_id(id);
			let size = payload.len();
			// 1-byte EBML size (0x80 | len) for small payloads
			assert!(size < 0x7f);
			v.push(0x80 | size as u8);
			v.extend(payload);
			v
		}
		let duration = sized(0x4489, &5000.0f32.to_be_bytes());
		let scale = sized(0x2AD7B1, &1_000_000u32.to_be_bytes());
		let mut info_payload = duration;
		info_payload.extend(scale);
		let info = sized(0x1549A966, &info_payload);
		let segment_payload = info;
		let segment = sized(0x18538067, &segment_payload);
		let d = mkv_duration_from_bytes(&segment).unwrap();
		// 5000 ticks * 1_000_000 ns = 5e9 ns = 5s
		assert_eq!(d, Duration::from_secs(5));
	}
}
