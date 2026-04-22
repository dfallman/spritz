use crate::{DLNA_CONTENT_FEATURES, DlnaConfig, description::xml_escape, soap};
use axum::{http::HeaderMap, response::Response};
use std::path::Path;
use std::sync::Arc;

const CD_SERVICE: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
const CM_SERVICE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

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
		"Browse" => browse(&body, &config),
		"GetSystemUpdateID" => soap::ok(soap::response(
			"GetSystemUpdateID",
			CD_SERVICE,
			"<Id>1</Id>",
		)),
		"GetSearchCapabilities" => soap::ok(soap::response(
			"GetSearchCapabilities",
			CD_SERVICE,
			"<SearchCaps></SearchCaps>",
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
			let flags = DLNA_CONTENT_FEATURES;
			let source = spritz_core::ALL_MIMES
				.iter()
				.map(|m| format!("http-get:*:{m}:{flags}"))
				.collect::<Vec<_>>()
				.join(",");
			let inner = format!("<Source>{source}</Source><Sink></Sink>");
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

fn browse(body: &str, config: &DlnaConfig) -> Response {
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

	// Index membership lists per category. Cheap on small libraries; the
	// whole media list is already in memory.
	let video_idx: Vec<usize> = config
		.media_files
		.iter()
		.enumerate()
		.filter(|(_, p)| !is_audio(p))
		.map(|(i, _)| i)
		.collect();
	let audio_idx: Vec<usize> = config
		.media_files
		.iter()
		.enumerate()
		.filter(|(_, p)| is_audio(p))
		.map(|(i, _)| i)
		.collect();

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
			let slice_start = start.min(total);
			let slice_end = if count == 0 {
				total
			} else {
				(start + count).min(total)
			};
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
		(VIDEO_ID, _) => category_children(&video_idx, VIDEO_ID, start, count, config),
		(AUDIO_ID, _) => category_children(&audio_idx, AUDIO_ID, start, count, config),
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
				folder_children(idx, node, start, count, config)
			}
		}
		(id, _) if id.starts_with("m:") => {
			let idx: usize = id[2..].parse().unwrap_or(usize::MAX);
			match config.media_files.get(idx) {
				Some(p) => {
					let parent = if is_audio(p) { AUDIO_ID } else { VIDEO_ID };
					match item_xml(idx, p, parent, config) {
						Some(item) => (didl_wrap(&[item]), 1, 1),
						None => return soap::err(soap::fault(701, "No Such Object")),
					}
				}
				None => return soap::err(soap::fault(701, "No Such Object")),
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
) -> (String, usize, usize) {
	let total = indices.len();
	let slice_start = start.min(total);
	let slice_end = if count == 0 {
		total
	} else {
		(start + count).min(total)
	};
	let items: Vec<String> = indices[slice_start..slice_end]
		.iter()
		.filter_map(|&i| {
			config
				.media_files
				.get(i)
				.and_then(|p| item_xml(i, p, parent, config))
		})
		.collect();
	let returned = items.len();
	(didl_wrap(&items), returned, total)
}

fn paginate(entries: Vec<String>, start: usize, count: usize) -> (String, usize, usize) {
	let total = entries.len();
	let slice_start = start.min(total);
	let slice_end = if count == 0 {
		total
	} else {
		(start + count).min(total)
	};
	let slice = &entries[slice_start..slice_end];
	(didl_wrap(slice), slice.len(), total)
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
			&& let Some(item) = item_xml(media_i, path, &self_id, config)
		{
			entries.push(item);
		}
	}

	paginate(entries, start, count)
}

fn is_audio(path: &Path) -> bool {
	path.extension()
		.and_then(|e| e.to_str())
		.and_then(spritz_core::mime_for_ext)
		.map(|m| m.starts_with("audio/"))
		.unwrap_or(false)
}

fn didl_wrap(entries: &[String]) -> String {
	format!(
		r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
  {}
</DIDL-Lite>"#,
		entries.join("\n  ")
	)
}

fn item_xml(index: usize, path: &Path, parent_id: &str, config: &DlnaConfig) -> Option<String> {
	let (dir_idx, url_path) = spritz_core::media_url_path(path, &config.media_dirs)?;
	let url = format!(
		"http://{}:{}/m/{dir_idx}/{url_path}",
		config.local_ip, config.http_port
	);
	let title = path.file_name()?.to_string_lossy();
	let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
	let mime = spritz_core::mime_for_ext(ext).unwrap_or("application/octet-stream");
	let class = upnp_class_for_mime(mime);
	let flags = DLNA_CONTENT_FEATURES;

	// Emit size= only when we know it — Infuse treats size="0" as "empty file".
	let size_attr = match config.media_sizes.get(index).copied().unwrap_or(0) {
		0 => String::new(),
		n => format!(r#" size="{n}""#),
	};

	Some(format!(
		r#"<item id="m:{index}" parentID="{parent_id}" restricted="1">
    <dc:title>{}</dc:title>
    <upnp:class>{class}</upnp:class>
    <dc:date>2000-01-01</dc:date>
    <res protocolInfo="http-get:*:{mime}:{flags}"{size_attr}>{}</res>
  </item>"#,
		xml_escape(&title),
		xml_escape(&url),
	))
}

fn upnp_class_for_mime(mime: &str) -> &'static str {
	if mime.starts_with("audio/") {
		"object.item.audioItem.musicTrack"
	} else {
		"object.item.videoItem"
	}
}
