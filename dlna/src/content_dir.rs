use crate::{DLNA_CONTENT_FEATURES, DlnaConfig, description::xml_escape, soap};
use axum::{http::HeaderMap, response::Response};
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Arc;

const CD_SERVICE: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
const CM_SERVICE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// Max DIDL entries returned when a client sends `RequestedCount=0` ("all").
/// Honest `TotalMatches` still lets well-behaved clients page.
const DEFAULT_BROWSE_PAGE: usize = 200;

pub async fn handle_contentdirectory(
	headers: HeaderMap,
	body: String,
	config: Arc<DlnaConfig>,
) -> Response {
	let action = headers
		.get("soapaction")
		.and_then(|v| v.to_str().ok())
		.map(soap::parse_action)
		.unwrap_or_default();

	match action.as_str() {
		"Browse" => browse(&headers, &body, &config),
		"Search" => search(&headers, &body, &config),
		"GetSystemUpdateID" => soap::ok(soap::response(
			"GetSystemUpdateID",
			CD_SERVICE,
			"<Id>1</Id>",
		)),
		"GetSearchCapabilities" => soap::ok(soap::response(
			"GetSearchCapabilities",
			CD_SERVICE,
			"<SearchCaps>dc:title,upnp:class</SearchCaps>",
		)),
		"GetSortCapabilities" => soap::ok(soap::response(
			"GetSortCapabilities",
			CD_SERVICE,
			"<SortCaps></SortCaps>",
		)),
		_ => soap::err(soap::fault(401, "Invalid Action")),
	}
}

pub async fn handle_connectionmanager(
	headers: HeaderMap,
	_body: String,
	_config: Arc<DlnaConfig>,
) -> Response {
	let action = headers
		.get("soapaction")
		.and_then(|v| v.to_str().ok())
		.map(soap::parse_action)
		.unwrap_or_default();

	match action.as_str() {
		"GetProtocolInfo" => {
			let inner = format!("<Source>{}</Source><Sink></Sink>", source_protocol_info());
			soap::ok(soap::response("GetProtocolInfo", CM_SERVICE, &inner))
		}
		"GetCurrentConnectionIDs" => soap::ok(soap::response(
			"GetCurrentConnectionIDs",
			CM_SERVICE,
			"<ConnectionIDs>0</ConnectionIDs>",
		)),
		"GetCurrentConnectionInfo" => {
			let inner = "<RcsID>-1</RcsID>\
				<AVTransportID>-1</AVTransportID>\
				<ProtocolInfo></ProtocolInfo>\
				<PeerConnectionManager></PeerConnectionManager>\
				<PeerConnectionID>-1</PeerConnectionID>\
				<Direction>Output</Direction>\
				<Status>OK</Status>";
			soap::ok(soap::response(
				"GetCurrentConnectionInfo",
				CM_SERVICE,
				inner,
			))
		}
		_ => soap::err(soap::fault(401, "Invalid Action")),
	}
}

pub fn source_protocol_info() -> String {
	spritz_core::ALL_MIMES
		.iter()
		.map(|m| {
			let ext = mime_ext(m);
			spritz_core::protocol_info(m, ext, DLNA_CONTENT_FEATURES)
		})
		.collect::<Vec<_>>()
		.join(",")
}

fn mime_ext(mime: &str) -> &'static str {
	match mime {
		"video/mp4" => "mp4",
		"video/x-matroska" => "mkv",
		"video/x-msvideo" => "avi",
		"video/quicktime" => "mov",
		"video/webm" => "webm",
		"video/x-flv" => "flv",
		"audio/mpeg" => "mp3",
		"audio/mp4" => "m4a",
		"audio/aac" => "aac",
		"audio/flac" => "flac",
		"audio/ogg" => "ogg",
		"audio/wav" => "wav",
		"audio/x-ms-wma" => "wma",
		"audio/aiff" => "aiff",
		_ => "",
	}
}

// Container IDs. Infuse (and tvOS DLNA clients in general) expect the root
// to contain child containers, not items. Plex/minidlna/Jellyfin all use this
// shape, so it's what clients are tuned for.
const ROOT_ID: &str = "0";
const VIDEO_ID: &str = "V";
const AUDIO_ID: &str = "A";
const FOLDER_ID: &str = "F";

/// Count of source-root FolderNodes that actually contain media (directly or
/// transitively). Determines whether "By folder" shows up at the root, and
/// is the childCount for the F container.
fn active_root_folder_count(config: &DlnaConfig) -> usize {
	let n = config.media_dirs.len();
	config
		.folder_nodes
		.iter()
		.take(n)
		.filter(|node| !node.subfolder_indices.is_empty() || !node.media_indices.is_empty())
		.count()
}

fn browse(headers: &HeaderMap, body: &str, config: &DlnaConfig) -> Response {
	let object_id =
		soap::extract_tag_value(body, "ObjectID").unwrap_or_else(|| ROOT_ID.to_string());
	let browse_flag = soap::extract_tag_value(body, "BrowseFlag")
		.unwrap_or_else(|| "BrowseDirectChildren".to_string());
	let start: usize = soap::extract_tag_value(body, "StartingIndex")
		.and_then(|s| s.parse().ok())
		.unwrap_or(0);
	let count: usize = soap::extract_tag_value(body, "RequestedCount")
		.and_then(|s| s.parse().ok())
		.unwrap_or(0);

	let video_idx = &config.video_idx;
	let audio_idx = &config.audio_idx;
	let public_base = public_base(headers, config.local_ip, config.http_port);

	let folder_root_count = active_root_folder_count(config);

	let (didl, returned, total_matches) = match (object_id.as_str(), browse_flag.as_str()) {
		(ROOT_ID, "BrowseMetadata") => {
			let child_count = (!video_idx.is_empty()) as usize
				+ (!audio_idx.is_empty()) as usize
				+ (folder_root_count > 0) as usize;
			let xml = format!(
				r#"<container id="0" parentID="-1" restricted="1" childCount="{child_count}">
    <dc:title>Spritz</dc:title>
    <upnp:class>object.container</upnp:class>
  </container>"#
			);
			(didl_wrap(&[xml]), 1usize, 1usize)
		}
		(ROOT_ID, _) => {
			let mut containers = Vec::new();
			if !video_idx.is_empty() {
				containers.push(category_container_xml(VIDEO_ID, "Videos", video_idx.len()));
			}
			if !audio_idx.is_empty() {
				containers.push(category_container_xml(AUDIO_ID, "Music", audio_idx.len()));
			}
			if folder_root_count > 0 {
				containers.push(category_container_xml(
					FOLDER_ID,
					"By folder",
					folder_root_count,
				));
			}
			let total = containers.len();
			let (slice_start, slice_end) = page_slice(start, count, total);
			let slice = &containers[slice_start..slice_end];
			(didl_wrap(slice), slice.len(), total)
		}
		(VIDEO_ID, "BrowseMetadata") => {
			let xml = category_container_xml(VIDEO_ID, "Videos", video_idx.len());
			(didl_wrap(&[xml]), 1, 1)
		}
		(AUDIO_ID, "BrowseMetadata") => {
			let xml = category_container_xml(AUDIO_ID, "Music", audio_idx.len());
			(didl_wrap(&[xml]), 1, 1)
		}
		(FOLDER_ID, "BrowseMetadata") => {
			let xml = category_container_xml(FOLDER_ID, "By folder", folder_root_count);
			(didl_wrap(&[xml]), 1, 1)
		}
		(VIDEO_ID, _) => category_children(video_idx, VIDEO_ID, start, count, config, &public_base),
		(AUDIO_ID, _) => category_children(audio_idx, AUDIO_ID, start, count, config, &public_base),
		(FOLDER_ID, _) => {
			// Source roots, filtered to those with media in subtree.
			let root_count = config.media_dirs.len();
			let entries: Vec<String> = config
				.folder_nodes
				.iter()
				.enumerate()
				.take(root_count)
				.filter(|(_, node)| {
					!node.subfolder_indices.is_empty() || !node.media_indices.is_empty()
				})
				.map(|(i, node)| folder_container_xml(i, FOLDER_ID, node))
				.collect();
			paginate(entries, start, count)
		}
		(id, _) if id.starts_with("f:") => {
			let idx: usize = match id[2..].parse() {
				Ok(i) => i,
				Err(_) => return soap::err(soap::fault(701, "No Such Object")),
			};
			let node = match config.folder_nodes.get(idx) {
				Some(n) => n,
				None => return soap::err(soap::fault(701, "No Such Object")),
			};
			let parent_id = folder_parent_id(config, idx);
			if browse_flag == "BrowseMetadata" {
				let xml = folder_container_xml_with_parent(idx, &parent_id, node);
				(didl_wrap(&[xml]), 1, 1)
			} else {
				folder_children(idx, node, start, count, config, &public_base)
			}
		}
		(id, flag) if parse_item_id(id).is_some() => {
			let (view, idx) = parse_item_id(id).expect("checked");
			if flag != "BrowseMetadata" {
				(didl_wrap(&[]), 0, 0)
			} else {
				match config.media_files.get(idx) {
					Some(p) => {
						let parent = if view == "m" {
							folder_parent_for_media(config, idx)
						} else {
							view.to_string()
						};
						match item_xml(idx, p, id, &parent, config, &public_base) {
							Some(item) => (didl_wrap(&[item]), 1, 1),
							None => return soap::err(soap::fault(701, "No Such Object")),
						}
					}
					None => return soap::err(soap::fault(701, "No Such Object")),
				}
			}
		}
		_ => return soap::err(soap::fault(701, "No Such Object")),
	};

	let inner = format!(
		"<Result>{}</Result>\
		<NumberReturned>{returned}</NumberReturned>\
		<TotalMatches>{total_matches}</TotalMatches>\
		<UpdateID>1</UpdateID>",
		xml_escape(&didl),
	);
	soap::ok(soap::response("Browse", CD_SERVICE, &inner))
}

fn search(headers: &HeaderMap, body: &str, config: &DlnaConfig) -> Response {
	let object_id = soap::extract_tag_value(body, "ContainerID")
		.or_else(|| soap::extract_tag_value(body, "ObjectID"))
		.unwrap_or_else(|| ROOT_ID.to_string());
	let criteria =
		soap::extract_tag_value(body, "SearchCriteria").unwrap_or_else(|| "*".to_string());
	let start: usize = soap::extract_tag_value(body, "StartingIndex")
		.and_then(|s| s.parse().ok())
		.unwrap_or(0);
	let count: usize = soap::extract_tag_value(body, "RequestedCount")
		.and_then(|s| s.parse().ok())
		.unwrap_or(0);
	let public_base = public_base(headers, config.local_ip, config.http_port);

	let Some(candidates) = search_candidates(&object_id, config) else {
		return soap::err(soap::fault(701, "No Such Object"));
	};

	let matched: Vec<usize> = candidates
		.into_iter()
		.filter(|&i| {
			config.media_files.get(i).is_some_and(|p| {
				let title = p.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
				let mime = p
					.extension()
					.and_then(|e| e.to_str())
					.and_then(spritz_core::mime_for_ext)
					.unwrap_or("video/mpeg");
				matches_search(title, mime, &criteria)
			})
		})
		.collect();

	let total = matched.len();
	let (slice_start, slice_end) = page_slice(start, count, total);
	let items: Vec<String> = matched[slice_start..slice_end]
		.iter()
		.filter_map(|&i| {
			config.media_files.get(i).and_then(|p| {
				let oid = item_id(&object_id, i);
				item_xml(i, p, &oid, &object_id, config, &public_base)
			})
		})
		.collect();
	let returned = items.len();
	let didl = didl_wrap(&items);
	let inner = format!(
		"<Result>{}</Result>\
		<NumberReturned>{returned}</NumberReturned>\
		<TotalMatches>{total}</TotalMatches>\
		<UpdateID>1</UpdateID>",
		xml_escape(&didl),
	);
	soap::ok(soap::response("Search", CD_SERVICE, &inner))
}

fn search_candidates(object_id: &str, config: &DlnaConfig) -> Option<Vec<usize>> {
	Some(match object_id {
		ROOT_ID => (0..config.media_files.len()).collect(),
		VIDEO_ID => config.video_idx.clone(),
		AUDIO_ID => config.audio_idx.clone(),
		FOLDER_ID => {
			let mut set = BTreeSet::new();
			for i in 0..config.media_dirs.len().min(config.folder_nodes.len()) {
				set.extend(folder_subtree_media(config, i));
			}
			set.into_iter().collect()
		}
		id if id.starts_with("f:") => {
			let idx = id[2..].parse::<usize>().ok()?;
			if idx >= config.folder_nodes.len() {
				return None;
			}
			folder_subtree_media(config, idx)
		}
		_ if parse_item_id(object_id).is_some() => Vec::new(),
		_ => return None,
	})
}

fn folder_subtree_media(config: &DlnaConfig, folder_idx: usize) -> Vec<usize> {
	let mut out = Vec::new();
	let mut stack = vec![folder_idx];
	while let Some(i) = stack.pop() {
		let Some(node) = config.folder_nodes.get(i) else {
			continue;
		};
		out.extend_from_slice(&node.media_indices);
		stack.extend_from_slice(&node.subfolder_indices);
	}
	out.sort_unstable();
	out.dedup();
	out
}

fn category_container_xml(id: &str, title: &str, child_count: usize) -> String {
	format!(
		r#"<container id="{id}" parentID="0" restricted="1" childCount="{child_count}">
    <dc:title>{title}</dc:title>
    <upnp:class>object.container</upnp:class>
  </container>"#
	)
}

fn category_children(
	indices: &[usize],
	parent: &str,
	start: usize,
	count: usize,
	config: &DlnaConfig,
	public_base: &str,
) -> (String, usize, usize) {
	let total = indices.len();
	let (slice_start, slice_end) = page_slice(start, count, total);
	let items: Vec<String> = indices[slice_start..slice_end]
		.iter()
		.filter_map(|&i| {
			config.media_files.get(i).and_then(|p| {
				let oid = item_id(parent, i);
				item_xml(i, p, &oid, parent, config, public_base)
			})
		})
		.collect();
	let returned = items.len();
	(didl_wrap(&items), returned, total)
}

fn paginate(entries: Vec<String>, start: usize, count: usize) -> (String, usize, usize) {
	let total = entries.len();
	let (slice_start, slice_end) = page_slice(start, count, total);
	let slice = &entries[slice_start..slice_end];
	(didl_wrap(slice), slice.len(), total)
}

fn page_slice(start: usize, count: usize, total: usize) -> (usize, usize) {
	let start = start.min(total);
	let limit = if count == 0 {
		DEFAULT_BROWSE_PAGE
	} else {
		count
	};
	let end = start.saturating_add(limit).min(total);
	(start, end)
}

/// Host:port advertised in DIDL `<res>` URLs and device description URLBase.
/// Prefer the request Host so VPN/Docker `local_ip()` mistakes don't poison playback.
pub(crate) fn public_base(headers: &HeaderMap, fallback_ip: IpAddr, port: u16) -> String {
	if let Some(host) = headers
		.get(axum::http::header::HOST)
		.and_then(|h| h.to_str().ok())
	{
		let host = host.trim();
		if spritz_core::valid_http_host(host) {
			return host.to_string();
		}
	}
	spritz_core::format_http_authority(fallback_ip, port)
}

fn folder_child_count(node: &crate::FolderNode) -> usize {
	node.subfolder_indices.len() + node.media_indices.len()
}

fn folder_container_xml(idx: usize, parent_id: &str, node: &crate::FolderNode) -> String {
	folder_container_xml_with_parent(idx, parent_id, node)
}

fn folder_container_xml_with_parent(
	idx: usize,
	parent_id: &str,
	node: &crate::FolderNode,
) -> String {
	let child_count = folder_child_count(node);
	let title = xml_escape(&node.display_name);
	format!(
		r#"<container id="f:{idx}" parentID="{parent_id}" restricted="1" childCount="{child_count}">
    <dc:title>{title}</dc:title>
    <upnp:class>object.container.storageFolder</upnp:class>
  </container>"#
	)
}

/// Find the parent id for a given folder node. Source roots live under the
/// "By folder" container (F); nested folders under their containing folder (f:N).
fn folder_parent_id(config: &DlnaConfig, idx: usize) -> String {
	let root_count = config.media_dirs.len();
	if idx < root_count {
		return FOLDER_ID.to_string();
	}
	for (parent_idx, node) in config.folder_nodes.iter().enumerate() {
		if node.subfolder_indices.contains(&idx) {
			return format!("f:{parent_idx}");
		}
	}
	FOLDER_ID.to_string()
}

fn folder_children(
	idx: usize,
	node: &crate::FolderNode,
	start: usize,
	count: usize,
	config: &DlnaConfig,
	public_base: &str,
) -> (String, usize, usize) {
	let self_id = format!("f:{idx}");
	let mut entries: Vec<String> = Vec::new();

	for &sub_i in &node.subfolder_indices {
		if let Some(sub) = config.folder_nodes.get(sub_i) {
			entries.push(folder_container_xml(sub_i, &self_id, sub));
		}
	}
	for &media_i in &node.media_indices {
		if let Some(path) = config.media_files.get(media_i)
			&& let Some(item) = {
				let oid = item_id(&self_id, media_i);
				item_xml(media_i, path, &oid, &self_id, config, public_base)
			} {
			entries.push(item);
		}
	}

	paginate(entries, start, count)
}

fn didl_wrap(entries: &[String]) -> String {
	format!(
		r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
  {}
</DIDL-Lite>"#,
		entries.join("\n  ")
	)
}

fn item_id(parent_id: &str, index: usize) -> String {
	match parent_id {
		VIDEO_ID => format!("v:{index}"),
		AUDIO_ID => format!("a:{index}"),
		_ => format!("m:{index}"),
	}
}

fn parse_item_id(id: &str) -> Option<(&'static str, usize)> {
	let (prefix, rest) = id.split_once(':')?;
	let index = rest.parse().ok()?;
	match prefix {
		"v" => Some((VIDEO_ID, index)),
		"a" => Some((AUDIO_ID, index)),
		"m" => Some(("m", index)),
		_ => None,
	}
}

fn folder_parent_for_media(config: &DlnaConfig, media_idx: usize) -> String {
	for (i, node) in config.folder_nodes.iter().enumerate() {
		if node.media_indices.contains(&media_idx) {
			return format!("f:{i}");
		}
	}
	FOLDER_ID.to_string()
}

fn matches_search(title: &str, mime: &str, criteria: &str) -> bool {
	let c = criteria.trim();
	if c.is_empty() || c == "*" {
		return true;
	}
	let cl = c.to_ascii_lowercase();
	if cl.contains("videoitem") && !mime.starts_with("video/") {
		return false;
	}
	if (cl.contains("audioitem") || cl.contains("musictrack")) && !mime.starts_with("audio/") {
		return false;
	}
	for term in title_search_terms(c) {
		if !title
			.to_ascii_lowercase()
			.contains(&term.to_ascii_lowercase())
		{
			return false;
		}
	}
	true
}

fn title_search_terms(criteria: &str) -> Vec<String> {
	let mut terms = Vec::new();
	let lower = criteria.to_ascii_lowercase();
	let mut search_from = 0;
	while let Some(rel) = lower[search_from..].find("contains") {
		let after = search_from + rel + "contains".len();
		let rest = &criteria[after..];
		let Some(q1) = rest.find('"') else {
			break;
		};
		let inner = &rest[q1 + 1..];
		let Some(q2) = inner.find('"') else {
			break;
		};
		let t = inner[..q2].trim();
		if !t.is_empty() {
			terms.push(t.to_string());
		}
		search_from = after + q1 + 1 + q2 + 1;
	}
	terms
}

fn item_xml(
	index: usize,
	path: &Path,
	object_id: &str,
	parent_id: &str,
	config: &DlnaConfig,
	public_base: &str,
) -> Option<String> {
	let (dir_idx, url_path) = spritz_core::media_url_path(path, &config.media_dirs)?;
	let url = format!("http://{public_base}/m/{dir_idx}/{url_path}");
	let title = path.file_name()?.to_string_lossy();
	let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
	let mime = spritz_core::mime_for_ext(ext).unwrap_or("application/octet-stream");
	let class = upnp_class_for_mime(mime);
	let protocol = spritz_core::protocol_info(mime, ext, DLNA_CONTENT_FEATURES);

	// Emit size= only when we know it — Infuse treats size="0" as "empty file".
	let size_attr = match config.media_sizes.get(index).copied().unwrap_or(0) {
		0 => String::new(),
		n => format!(r#" size="{n}""#),
	};
	let duration_attr = config
		.media_durations
		.get(index)
		.map(String::as_str)
		.filter(|s| !s.is_empty())
		.map(|s| format!(r#" duration="{s}""#))
		.unwrap_or_default();
	let date = config
		.media_dates
		.get(index)
		.map(String::as_str)
		.filter(|s| !s.is_empty())
		.unwrap_or("2000-01-01");
	let ref_attr = if object_id.starts_with("m:") {
		String::new()
	} else {
		format!(r#" refID="m:{index}""#)
	};
	let extra_res = sidecar_subtitle_res(path, public_base, &config.media_dirs);
	let art = if config.media_has_art.get(index).copied().unwrap_or(false) {
		format!("\n    <upnp:albumArtURI>http://{public_base}/art/{index}</upnp:albumArtURI>")
	} else {
		String::new()
	};

	Some(format!(
		r#"<item id="{object_id}" parentID="{parent_id}" restricted="1"{ref_attr}>
    <dc:title>{}</dc:title>
    <upnp:class>{class}</upnp:class>
    <dc:date>{date}</dc:date>{art}
    <res protocolInfo="{protocol}"{size_attr}{duration_attr}>{}</res>{extra_res}
  </item>"#,
		xml_escape(&title),
		xml_escape(&url),
	))
}

fn sidecar_subtitle_res(
	path: &Path,
	public_base: &str,
	media_dirs: &[std::path::PathBuf],
) -> String {
	let mut extra = String::new();
	for (ext, mime) in [
		("srt", "text/srt"),
		("vtt", "text/vtt"),
		("ass", "text/x-ssa"),
	] {
		let sub = path.with_extension(ext);
		let Ok(meta) = std::fs::symlink_metadata(&sub) else {
			continue;
		};
		if meta.file_type().is_symlink() || !meta.is_file() {
			continue;
		}
		let Some((dir_idx, url_path)) = spritz_core::media_url_path(&sub, media_dirs) else {
			continue;
		};
		let url = format!("http://{public_base}/m/{dir_idx}/{url_path}");
		extra.push_str(&format!(
			r#"
    <res protocolInfo="http-get:*:{mime}:*">{}</res>"#,
			xml_escape(&url),
		));
	}
	extra
}

fn upnp_class_for_mime(mime: &str) -> &'static str {
	if mime.starts_with("audio/") {
		"object.item.audioItem.musicTrack"
	} else {
		"object.item.videoItem"
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn page_slice_caps_requested_count_zero() {
		assert_eq!(page_slice(0, 0, 10), (0, 10));
		assert_eq!(page_slice(0, 0, 500), (0, DEFAULT_BROWSE_PAGE));
		assert_eq!(page_slice(180, 0, 250), (180, 250));
	}

	#[test]
	fn page_slice_honors_explicit_count_and_clamps_start() {
		assert_eq!(page_slice(0, 50, 500), (0, 50));
		assert_eq!(page_slice(999, 10, 50), (50, 50));
		assert_eq!(page_slice(40, 20, 50), (40, 50));
	}

	#[test]
	fn public_base_prefers_host_header() {
		let mut headers = HeaderMap::new();
		headers.insert(axum::http::header::HOST, "10.0.0.8:9000".parse().unwrap());
		assert_eq!(
			public_base(&headers, "127.0.0.1".parse().unwrap(), 8080),
			"10.0.0.8:9000"
		);
	}

	#[test]
	fn public_base_falls_back_to_configured_ip() {
		let headers = HeaderMap::new();
		assert_eq!(
			public_base(&headers, "192.168.1.4".parse().unwrap(), 8080),
			"192.168.1.4:8080"
		);
		assert_eq!(
			public_base(&headers, "192.168.1.4".parse().unwrap(), 80),
			"192.168.1.4"
		);
		assert_eq!(
			public_base(&headers, "2001:db8::1".parse().unwrap(), 8080),
			"[2001:db8::1]:8080"
		);
	}

	#[test]
	fn item_id_is_unique_per_browse_view() {
		assert_eq!(item_id(VIDEO_ID, 3), "v:3");
		assert_eq!(item_id(AUDIO_ID, 3), "a:3");
		assert_eq!(item_id("f:2", 3), "m:3");
	}

	#[test]
	fn parse_item_id_restores_view_and_index() {
		assert_eq!(parse_item_id("v:3"), Some((VIDEO_ID, 3)));
		assert_eq!(parse_item_id("a:7"), Some((AUDIO_ID, 7)));
		assert_eq!(parse_item_id("m:4"), Some(("m", 4)));
		assert_eq!(parse_item_id("f:1"), None);
		assert_eq!(parse_item_id("m:nope"), None);
	}

	#[test]
	fn matches_search_title_and_class() {
		assert!(matches_search("Movie.mp4", "video/mp4", "*"));
		assert!(matches_search(
			"The Office S01.mkv",
			"video/x-matroska",
			r#"dc:title contains "office""#
		));
		assert!(!matches_search(
			"The Office S01.mkv",
			"video/x-matroska",
			r#"dc:title contains "seinfeld""#
		));
		assert!(!matches_search(
			"track.mp3",
			"audio/mpeg",
			r#"upnp:class derivedfrom "object.item.videoItem""#
		));
		assert!(matches_search(
			"track.mp3",
			"audio/mpeg",
			r#"upnp:class derivedfrom "object.item.audioItem""#
		));
	}

	#[test]
	fn item_xml_adds_sidecar_subtitle_resources() {
		let tmp = tempfile::tempdir().unwrap();
		let movie = tmp.path().join("clip.mp4");
		std::fs::write(&movie, b"x").unwrap();
		std::fs::write(tmp.path().join("clip.srt"), b"1").unwrap();

		let config = DlnaConfig {
			device_uuid: "u".into(),
			friendly_name: "Spritz".into(),
			http_port: 8080,
			local_ip: "127.0.0.1".parse().unwrap(),
			media_dirs: vec![tmp.path().to_path_buf()],
			media_files: vec![movie.clone()],
			media_sizes: vec![1],
			media_dates: vec!["2020-01-01".into()],
			media_durations: vec!["0:00:02.000".into()],
			media_has_art: vec![true],
			video_idx: vec![0],
			audio_idx: vec![],
			folder_nodes: vec![],
			event_hub: crate::event::EventHub::default(),
		};
		let xml = item_xml(0, &movie, "v:0", "V", &config, "127.0.0.1:8080").unwrap();
		assert!(xml.contains("/m/0/clip.mp4"), "{xml}");
		assert!(xml.contains("/m/0/clip.srt"), "{xml}");
		assert!(xml.contains("text/srt"), "{xml}");
		assert!(xml.contains(r#"duration="0:00:02.000""#), "{xml}");
		assert!(xml.contains("DLNA.ORG_PN=AVC_MP4_MP_SD_AAC_MULT5"), "{xml}");
		assert!(xml.contains("/art/0"), "{xml}");
	}
}
