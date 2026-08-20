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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodec {
	Avc,
	Hevc,
	Vp8,
	Vp9,
	Av1,
	Mpeg2,
	Mpeg4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
	Aac,
	Mp3,
	Ac3,
	Eac3,
	Flac,
	Pcm,
	Vorbis,
	Opus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MediaInfo {
	pub duration: Option<Duration>,
	pub width: Option<u32>,
	pub height: Option<u32>,
	pub video_codec: Option<VideoCodec>,
	pub audio_codec: Option<AudioCodec>,
}

impl MediaInfo {
	/// DIDL `resolution` value, e.g. `1920x1080`.
	pub fn resolution_attr(&self) -> Option<String> {
		let w = self.width.filter(|n| *n > 0)?;
		let h = self.height.filter(|n| *n > 0)?;
		Some(format!("{w}x{h}"))
	}
}

/// Audio (and image) profile names from the file extension. Video PNs are
/// not claimed here — a `.mp4` may be HEVC, and a wrong PN makes TVs refuse
/// the file. Use [`dlna_org_pn_for`] after probing.
pub fn dlna_org_pn(ext: &str) -> Option<&'static str> {
	Some(match ext.to_ascii_lowercase().as_str() {
		"mp3" => "MP3",
		"flac" => "FLAC",
		"wav" => "LPCM",
		"aac" => "AAC_ADTS_320",
		"m4a" => "AAC_ISO",
		"jpg" | "jpeg" => "JPEG_LRG",
		"png" => "PNG_LRG",
		_ => return None,
	})
}

/// Profile from a probed file. Omits the PN rather than advertising H.264
/// for HEVC/VP9/AV1/unknown video.
pub fn dlna_org_pn_for(ext: &str, info: &MediaInfo) -> Option<&'static str> {
	let ext = ext.to_ascii_lowercase();
	match info.video_codec {
		Some(VideoCodec::Avc) => {
			let h = info.height.unwrap_or(0);
			if matches!(ext.as_str(), "mkv" | "webm") {
				if h > 720 {
					Some("AVC_MKV_HP_HD_AC3")
				} else {
					Some("AVC_MKV_MP_HD_AC3")
				}
			} else if matches!(ext.as_str(), "mp4" | "m4v" | "mov") {
				if h > 720 {
					Some("AVC_MP4_HP_HD_AAC")
				} else if h > 576 {
					Some("AVC_MP4_MP_HD_AAC_MULT5")
				} else {
					Some("AVC_MP4_MP_SD_AAC_MULT5")
				}
			} else {
				None
			}
		}
		Some(_) => None,
		None => dlna_org_pn(&ext),
	}
}

pub fn protocol_info_pn(mime: &str, pn: Option<&str>, flags: &str) -> String {
	match pn {
		Some(pn) => format!("http-get:*:{mime}:DLNA.ORG_PN={pn};{flags}"),
		None => format!("http-get:*:{mime}:{flags}"),
	}
}

pub fn protocol_info(mime: &str, ext: &str, flags: &str) -> String {
	protocol_info_pn(mime, dlna_org_pn(ext), flags)
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

pub fn probe_media(path: &Path) -> MediaInfo {
	let ext = path
		.extension()
		.and_then(|e| e.to_str())
		.unwrap_or("")
		.to_ascii_lowercase();
	match ext.as_str() {
		"mp4" | "m4v" | "m4a" | "mov" => read_probe(path, mp4_info_from_bytes),
		"mkv" | "webm" => read_probe(path, mkv_info_from_bytes),
		"wav" => MediaInfo {
			duration: duration_from_wav(path).ok().flatten(),
			audio_codec: Some(AudioCodec::Pcm),
			..MediaInfo::default()
		},
		"flac" => MediaInfo {
			duration: duration_from_flac(path).ok().flatten(),
			audio_codec: Some(AudioCodec::Flac),
			..MediaInfo::default()
		},
		_ => MediaInfo::default(),
	}
}

fn read_probe(path: &Path, parse: fn(&[u8]) -> MediaInfo) -> MediaInfo {
	let mut data = Vec::new();
	match std::fs::File::open(path).and_then(|mut f| f.read_to_end(&mut data)) {
		Ok(_) => parse(&data),
		Err(_) => MediaInfo::default(),
	}
}

pub fn probe_duration(path: &Path) -> Option<Duration> {
	probe_media(path).duration
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

pub(crate) fn mp4_info_from_bytes(data: &[u8]) -> MediaInfo {
	let mut info = MediaInfo::default();
	walk_mp4(data, 0, data.len(), &mut info);
	info
}

fn mp4_container(kind: &[u8]) -> bool {
	matches!(
		kind,
		b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts"
	)
}

fn walk_mp4(data: &[u8], mut pos: usize, end: usize, info: &mut MediaInfo) {
	while pos + 8 <= end {
		let Ok(size_bytes) = data[pos..pos + 4].try_into() else {
			break;
		};
		let size = u32::from_be_bytes(size_bytes) as usize;
		let kind = &data[pos + 4..pos + 8];
		let (hdr, payload_size) = if size == 1 {
			if pos + 16 > end {
				break;
			}
			let Ok(large_bytes) = data[pos + 8..pos + 16].try_into() else {
				break;
			};
			let large = u64::from_be_bytes(large_bytes) as usize;
			(16usize, large.saturating_sub(16))
		} else if size == 0 {
			(8usize, end.saturating_sub(pos + 8))
		} else {
			(8usize, size.saturating_sub(8))
		};
		let payload_start = pos + hdr;
		let payload_end = (payload_start + payload_size).min(end);
		let payload = &data[payload_start..payload_end];
		if kind == b"mvhd" && info.duration.is_none() {
			info.duration = parse_mvhd(payload);
		} else if kind == b"tkhd" {
			if let Some((w, h)) = parse_tkhd(payload)
				&& w > 0 && h > 0
				&& info.width.is_none()
			{
				info.width = Some(w);
				info.height = Some(h);
			}
		} else if kind == b"stsd" {
			parse_stsd(payload, info);
		} else if mp4_container(kind) {
			walk_mp4(data, payload_start, payload_end, info);
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
}

fn parse_tkhd(payload: &[u8]) -> Option<(u32, u32)> {
	if payload.is_empty() {
		return None;
	}
	let (w_off, h_off) = if payload[0] == 1 { (88, 92) } else { (76, 80) };
	if payload.len() < h_off + 4 {
		return None;
	}
	let w = u32::from_be_bytes(payload[w_off..w_off + 4].try_into().ok()?) >> 16;
	let h = u32::from_be_bytes(payload[h_off..h_off + 4].try_into().ok()?) >> 16;
	Some((w, h))
}

fn parse_stsd(payload: &[u8], info: &mut MediaInfo) {
	if payload.len() < 8 {
		return;
	}
	let Ok(count_bytes) = payload[4..8].try_into() else {
		return;
	};
	let count = u32::from_be_bytes(count_bytes);
	let mut pos = 8usize;
	for _ in 0..count {
		if pos + 8 > payload.len() {
			break;
		}
		let size = u32::from_be_bytes(payload[pos..pos + 4].try_into().unwrap_or([0; 4])) as usize;
		let fourcc = &payload[pos + 4..pos + 8];
		if let Some(codec) = video_fourcc(fourcc)
			&& info.video_codec.is_none()
		{
			info.video_codec = Some(codec);
			if info.width.is_none() && payload.len() >= pos + 36 {
				let w = u16::from_be_bytes(payload[pos + 32..pos + 34].try_into().unwrap_or([0; 2]))
					as u32;
				let h = u16::from_be_bytes(payload[pos + 34..pos + 36].try_into().unwrap_or([0; 2]))
					as u32;
				if w > 0 && h > 0 {
					info.width = Some(w);
					info.height = Some(h);
				}
			}
		} else if let Some(codec) = audio_fourcc(fourcc)
			&& info.audio_codec.is_none()
		{
			info.audio_codec = Some(codec);
		}
		pos = if size < 8 { pos + 8 } else { pos + size };
	}
}

fn video_fourcc(fourcc: &[u8]) -> Option<VideoCodec> {
	Some(match fourcc {
		b"avc1" | b"avc3" | b"dvav" => VideoCodec::Avc,
		b"hvc1" | b"hev1" | b"dvh1" | b"dvhe" => VideoCodec::Hevc,
		b"vp09" => VideoCodec::Vp9,
		b"vp08" => VideoCodec::Vp8,
		b"av01" => VideoCodec::Av1,
		b"mp4v" => VideoCodec::Mpeg4,
		b"mp2v" | b"m2v1" => VideoCodec::Mpeg2,
		_ => return None,
	})
}

fn audio_fourcc(fourcc: &[u8]) -> Option<AudioCodec> {
	Some(match fourcc {
		b"mp4a" => AudioCodec::Aac,
		b"ac-3" | b"AC-3" => AudioCodec::Ac3,
		b"ec-3" => AudioCodec::Eac3,
		b"fLaC" | b"flac" => AudioCodec::Flac,
		b"Opus" | b"opus" => AudioCodec::Opus,
		b".mp3" | b"mp3 " => AudioCodec::Mp3,
		b"lpcm" | b"sowt" | b"twos" => AudioCodec::Pcm,
		_ => return None,
	})
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

pub(crate) fn mkv_info_from_bytes(data: &[u8]) -> MediaInfo {
	let mut info = MediaInfo::default();
	let mut pos = 0usize;
	let mut timestamp_scale = 1_000_000.0f64;
	let mut duration_ticks: Option<f64> = None;
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
						if let Some(v) = ebml_uint(&data[ip..iend]) {
							timestamp_scale = v as f64;
						}
					} else if iid == 0x4489 {
						duration_ticks = ebml_float(&data[ip..iend]);
					}
					ip = iend;
				}
			}
			0x1654_AE6B => parse_mkv_tracks(&data[pos..end], &mut info),
			0x1853_8067 => continue,
			0x1A45_DFA3 => {
				pos = end;
				continue;
			}
			_ => {
				pos = end;
				continue;
			}
		}
		pos = end;
	}
	if let Some(ticks) = duration_ticks {
		let nanos = ticks * timestamp_scale;
		if nanos > 0.0 {
			info.duration = Some(Duration::from_secs_f64(nanos / 1_000_000_000.0));
		}
	}
	info
}

fn parse_mkv_tracks(data: &[u8], info: &mut MediaInfo) {
	let mut pos = 0usize;
	while pos + 1 < data.len() {
		let Some((id, id_len)) = ebml_id(&data[pos..]) else {
			break;
		};
		pos += id_len;
		let Some((size, size_len)) = ebml_size(&data[pos..]) else {
			break;
		};
		pos += size_len;
		let end = (pos + size).min(data.len());
		if id == 0xAE {
			parse_mkv_track_entry(&data[pos..end], info);
		}
		pos = end;
	}
}

fn parse_mkv_track_entry(data: &[u8], info: &mut MediaInfo) {
	let mut pos = 0usize;
	let mut codec = None;
	let mut track_type = None;
	let mut width = None;
	let mut height = None;
	while pos + 1 < data.len() {
		let Some((id, id_len)) = ebml_id(&data[pos..]) else {
			break;
		};
		pos += id_len;
		let Some((size, size_len)) = ebml_size(&data[pos..]) else {
			break;
		};
		pos += size_len;
		let end = (pos + size).min(data.len());
		match id {
			0x83 => track_type = ebml_uint(&data[pos..end]),
			0x86 => codec = std::str::from_utf8(&data[pos..end]).ok(),
			0xE0 => {
				let mut vp = pos;
				while vp + 1 < end {
					let Some((vid, vlen)) = ebml_id(&data[vp..end]) else {
						break;
					};
					vp += vlen;
					let Some((vsize, slen)) = ebml_size(&data[vp..end]) else {
						break;
					};
					vp += slen;
					let vend = (vp + vsize).min(end);
					if vid == 0xB0 {
						width = ebml_uint(&data[vp..vend]).map(|n| n as u32);
					} else if vid == 0xBA {
						height = ebml_uint(&data[vp..vend]).map(|n| n as u32);
					}
					vp = vend;
				}
			}
			_ => {}
		}
		pos = end;
	}
	let codec = codec.unwrap_or("");
	match track_type {
		Some(1) => {
			if info.video_codec.is_none() {
				info.video_codec = mkv_video_codec(codec);
			}
			if info.width.is_none()
				&& let (Some(w), Some(h)) = (width, height)
				&& w > 0 && h > 0
			{
				info.width = Some(w);
				info.height = Some(h);
			}
		}
		Some(2) if info.audio_codec.is_none() => {
			info.audio_codec = mkv_audio_codec(codec);
		}
		_ => {}
	}
}

fn mkv_video_codec(id: &str) -> Option<VideoCodec> {
	if id.contains("AVC") {
		Some(VideoCodec::Avc)
	} else if id.contains("HEVC") || id.contains("HVC") {
		Some(VideoCodec::Hevc)
	} else if id.contains("VP9") {
		Some(VideoCodec::Vp9)
	} else if id.contains("VP8") {
		Some(VideoCodec::Vp8)
	} else if id.contains("AV1") {
		Some(VideoCodec::Av1)
	} else if id.contains("MPEG2") {
		Some(VideoCodec::Mpeg2)
	} else if id.starts_with("V_MPEG4") {
		Some(VideoCodec::Mpeg4)
	} else {
		None
	}
}

fn mkv_audio_codec(id: &str) -> Option<AudioCodec> {
	if id.starts_with("A_AAC") {
		Some(AudioCodec::Aac)
	} else if id.contains("MPEG/L3") || id.ends_with("MP3") {
		Some(AudioCodec::Mp3)
	} else if id.contains("EAC3") || id.contains("E-AC3") {
		Some(AudioCodec::Eac3)
	} else if id.contains("AC3") {
		Some(AudioCodec::Ac3)
	} else if id.contains("FLAC") {
		Some(AudioCodec::Flac)
	} else if id.contains("VORBIS") {
		Some(AudioCodec::Vorbis)
	} else if id.contains("OPUS") {
		Some(AudioCodec::Opus)
	} else if id.contains("PCM") {
		Some(AudioCodec::Pcm)
	} else {
		None
	}
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
	fn dlna_org_pn_maps_audio_by_extension_only() {
		assert_eq!(dlna_org_pn("mp3"), Some("MP3"));
		assert_eq!(dlna_org_pn("FLAC"), Some("FLAC"));
		assert_eq!(dlna_org_pn("wav"), Some("LPCM"));
		assert_eq!(dlna_org_pn("m4a"), Some("AAC_ISO"));
		assert_eq!(dlna_org_pn("webm"), None);
		assert_eq!(dlna_org_pn("avi"), None);
	}

	#[test]
	fn video_extension_alone_does_not_claim_h264_pn() {
		assert_eq!(dlna_org_pn("mp4"), None);
		assert_eq!(dlna_org_pn("m4v"), None);
		assert_eq!(dlna_org_pn("mov"), None);
		assert_eq!(dlna_org_pn("mkv"), None);
	}

	#[test]
	fn protocol_info_inserts_pn_when_known() {
		assert_eq!(
			protocol_info("audio/mpeg", "mp3", "DLNA.ORG_OP=01"),
			"http-get:*:audio/mpeg:DLNA.ORG_PN=MP3;DLNA.ORG_OP=01"
		);
		assert_eq!(
			protocol_info("video/mp4", "mp4", "DLNA.ORG_OP=01"),
			"http-get:*:video/mp4:DLNA.ORG_OP=01"
		);
		assert_eq!(
			protocol_info("video/webm", "webm", "DLNA.ORG_OP=01"),
			"http-get:*:video/webm:DLNA.ORG_OP=01"
		);
	}

	#[test]
	fn protocol_info_pn_omits_profile_when_none() {
		assert_eq!(
			protocol_info_pn("video/mp4", None, "DLNA.ORG_OP=01"),
			"http-get:*:video/mp4:DLNA.ORG_OP=01"
		);
		assert_eq!(
			protocol_info_pn("video/mp4", Some("AVC_MP4_HP_HD_AAC"), "DLNA.ORG_OP=01"),
			"http-get:*:video/mp4:DLNA.ORG_PN=AVC_MP4_HP_HD_AAC;DLNA.ORG_OP=01"
		);
	}

	#[test]
	fn h264_mp4_pn_follows_height_bands() {
		let sd = MediaInfo {
			video_codec: Some(VideoCodec::Avc),
			height: Some(480),
			..MediaInfo::default()
		};
		let hd = MediaInfo {
			video_codec: Some(VideoCodec::Avc),
			height: Some(720),
			..MediaInfo::default()
		};
		let full = MediaInfo {
			video_codec: Some(VideoCodec::Avc),
			height: Some(1080),
			..MediaInfo::default()
		};
		assert_eq!(dlna_org_pn_for("mp4", &sd), Some("AVC_MP4_MP_SD_AAC_MULT5"));
		assert_eq!(dlna_org_pn_for("mp4", &hd), Some("AVC_MP4_MP_HD_AAC_MULT5"));
		assert_eq!(dlna_org_pn_for("mov", &full), Some("AVC_MP4_HP_HD_AAC"));
		assert_eq!(dlna_org_pn_for("mkv", &hd), Some("AVC_MKV_MP_HD_AC3"));
		assert_eq!(dlna_org_pn_for("mkv", &full), Some("AVC_MKV_HP_HD_AC3"));
	}

	#[test]
	fn hevc_and_unknown_video_omit_pn() {
		let hevc = MediaInfo {
			video_codec: Some(VideoCodec::Hevc),
			width: Some(1920),
			height: Some(1080),
			..MediaInfo::default()
		};
		let vp9 = MediaInfo {
			video_codec: Some(VideoCodec::Vp9),
			height: Some(1080),
			..MediaInfo::default()
		};
		assert_eq!(dlna_org_pn_for("mp4", &hevc), None);
		assert_eq!(dlna_org_pn_for("mkv", &hevc), None);
		assert_eq!(dlna_org_pn_for("webm", &vp9), None);
		assert_eq!(dlna_org_pn_for("mp4", &MediaInfo::default()), None);
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
		let d = mp4_info_from_bytes(&file).duration.unwrap();
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
		let d = mkv_info_from_bytes(&segment).duration.unwrap();
		// 5000 ticks * 1_000_000 ns = 5e9 ns = 5s
		assert_eq!(d, Duration::from_secs(5));
	}

	fn tkhd_v0(width: u32, height: u32) -> Vec<u8> {
		let mut p = vec![0u8; 84];
		p[76..80].copy_from_slice(&(width << 16).to_be_bytes());
		p[80..84].copy_from_slice(&(height << 16).to_be_bytes());
		p
	}

	fn hdlr(handler: &[u8; 4]) -> Vec<u8> {
		let mut p = vec![0u8; 13];
		p[8..12].copy_from_slice(handler);
		p
	}

	fn stsd_visual(fourcc: &[u8; 4], width: u16, height: u16) -> Vec<u8> {
		let mut entry = vec![0u8; 70];
		entry[7] = 1;
		entry[24..26].copy_from_slice(&width.to_be_bytes());
		entry[26..28].copy_from_slice(&height.to_be_bytes());
		let entry_box = box_bytes(fourcc, &entry);
		let mut stsd = vec![0u8; 8];
		stsd[7] = 1;
		stsd.extend(entry_box);
		box_bytes(b"stsd", &stsd)
	}

	fn h264_mp4_1080p() -> Vec<u8> {
		let mut mvhd = vec![0u8; 24];
		mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
		mvhd[16..20].copy_from_slice(&1000u32.to_be_bytes());
		let stbl = box_bytes(b"stbl", &stsd_visual(b"avc1", 1920, 1080));
		let minf = box_bytes(b"minf", &stbl);
		let mdia = box_bytes(
			b"mdia",
			&[box_bytes(b"hdlr", &hdlr(b"vide")), minf].concat(),
		);
		let trak = box_bytes(
			b"trak",
			&[box_bytes(b"tkhd", &tkhd_v0(1920, 1080)), mdia].concat(),
		);
		let moov = box_bytes(b"moov", &[box_bytes(b"mvhd", &mvhd), trak].concat());
		let mut file = box_bytes(b"ftyp", b"isom");
		file.extend(moov);
		file
	}

	fn hevc_mp4_720p() -> Vec<u8> {
		let mut mvhd = vec![0u8; 24];
		mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
		mvhd[16..20].copy_from_slice(&500u32.to_be_bytes());
		let stbl = box_bytes(b"stbl", &stsd_visual(b"hvc1", 1280, 720));
		let minf = box_bytes(b"minf", &stbl);
		let mdia = box_bytes(
			b"mdia",
			&[box_bytes(b"hdlr", &hdlr(b"vide")), minf].concat(),
		);
		let trak = box_bytes(
			b"trak",
			&[box_bytes(b"tkhd", &tkhd_v0(1280, 720)), mdia].concat(),
		);
		let moov = box_bytes(b"moov", &[box_bytes(b"mvhd", &mvhd), trak].concat());
		let mut file = box_bytes(b"ftyp", b"isom");
		file.extend(moov);
		file
	}

	#[test]
	fn mp4_probe_reads_avc1_and_tkhd_size() {
		let info = mp4_info_from_bytes(&h264_mp4_1080p());
		assert_eq!(info.video_codec, Some(VideoCodec::Avc));
		assert_eq!(info.width, Some(1920));
		assert_eq!(info.height, Some(1080));
		assert_eq!(info.duration, Some(Duration::from_secs(1)));
		assert_eq!(info.resolution_attr().as_deref(), Some("1920x1080"));
	}

	#[test]
	fn mp4_probe_reads_hevc_without_claiming_avc() {
		let info = mp4_info_from_bytes(&hevc_mp4_720p());
		assert_eq!(info.video_codec, Some(VideoCodec::Hevc));
		assert_eq!(info.width, Some(1280));
		assert_eq!(info.height, Some(720));
		assert_eq!(dlna_org_pn_for("mp4", &info), None);
	}

	fn mkv_track(codec: &str, width: u16, height: u16) -> Vec<u8> {
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
			assert!(size < 0x7f);
			v.push(0x80 | size as u8);
			v.extend(payload);
			v
		}
		let codec_id = sized(0x86, codec.as_bytes());
		let track_type = sized(0x83, &[1]);
		let pixel_w = sized(0xB0, &width.to_be_bytes());
		let pixel_h = sized(0xBA, &height.to_be_bytes());
		let mut video = pixel_w;
		video.extend(pixel_h);
		let video = sized(0xE0, &video);
		let mut entry = codec_id;
		entry.extend(track_type);
		entry.extend(video);
		let entry = sized(0xAE, &entry);
		sized(0x1654AE6B, &entry)
	}

	#[test]
	fn mkv_probe_reads_avc_track_dimensions() {
		let duration = {
			fn sized(id: u32, payload: &[u8]) -> Vec<u8> {
				let mut v = if id <= 0xff {
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
				};
				assert!(payload.len() < 0x7f);
				v.push(0x80 | payload.len() as u8);
				v.extend(payload);
				v
			}
			let dur = sized(0x4489, &1000.0f32.to_be_bytes());
			let scale = sized(0x2AD7B1, &1_000_000u32.to_be_bytes());
			let mut info_payload = dur;
			info_payload.extend(scale);
			sized(0x1549A966, &info_payload)
		};
		let tracks = mkv_track("V_MPEG4/ISO/AVC", 1920, 1080);
		let mut segment_payload = duration;
		segment_payload.extend(tracks);
		fn sized_seg(payload: &[u8]) -> Vec<u8> {
			let mut v = vec![0x18, 0x53, 0x80, 0x67];
			assert!(payload.len() < 0x7f);
			v.push(0x80 | payload.len() as u8);
			v.extend(payload);
			v
		}
		let segment = sized_seg(&segment_payload);
		let info = mkv_info_from_bytes(&segment);
		assert_eq!(info.video_codec, Some(VideoCodec::Avc));
		assert_eq!(info.width, Some(1920));
		assert_eq!(info.height, Some(1080));
		assert_eq!(info.resolution_attr().as_deref(), Some("1920x1080"));
		assert_eq!(dlna_org_pn_for("mkv", &info), Some("AVC_MKV_HP_HD_AC3"));
	}

	#[test]
	fn resolution_attr_skips_zero_dimensions() {
		assert_eq!(MediaInfo::default().resolution_attr(), None);
		assert_eq!(
			MediaInfo {
				width: Some(0),
				height: Some(1080),
				..MediaInfo::default()
			}
			.resolution_attr(),
			None
		);
	}
}
