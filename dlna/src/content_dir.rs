use crate::{DlnaConfig, description::xml_escape, soap};
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
			let inner = "<Source>http-get:*:video/mp4:*,\
				http-get:*:video/x-matroska:*,\
				http-get:*:video/x-msvideo:*,\
				http-get:*:video/quicktime:*,\
				http-get:*:video/webm:*,\
				http-get:*:video/x-flv:*</Source>\
				<Sink></Sink>";
			soap::ok(soap::response("GetProtocolInfo", CM_SERVICE, inner))
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
			soap::ok(soap::response("GetCurrentConnectionInfo", CM_SERVICE, inner))
		}
		_ => soap::err(soap::fault(401, "Invalid Action")),
	}
}

fn browse(body: &str, config: &DlnaConfig) -> Response {
	let object_id = soap::extract_tag_value(body, "ObjectID").unwrap_or_else(|| "0".to_string());
	let browse_flag = soap::extract_tag_value(body, "BrowseFlag")
		.unwrap_or_else(|| "BrowseDirectChildren".to_string());
	let start: usize = soap::extract_tag_value(body, "StartingIndex")
		.and_then(|s| s.parse().ok())
		.unwrap_or(0);
	let count: usize = soap::extract_tag_value(body, "RequestedCount")
		.and_then(|s| s.parse().ok())
		.unwrap_or(0);

	let total = config.videos.len();

	let (didl, returned, total_matches) = match (object_id.as_str(), browse_flag.as_str()) {
		("0", "BrowseMetadata") => {
			let xml = format!(
				r#"<container id="0" parentID="-1" restricted="1" childCount="{total}">
    <dc:title>Videos</dc:title>
    <upnp:class>object.container</upnp:class>
  </container>"#
			);
			(didl_wrap(&[xml]), 1usize, 1usize)
		}
		("0", _) => {
			let slice_start = start.min(total);
			let slice_end = if count == 0 { total } else { (start + count).min(total) };
			let items: Vec<String> = config.videos[slice_start..slice_end]
				.iter()
				.enumerate()
				.filter_map(|(i, path)| item_xml(slice_start + i, path, config))
				.collect();
			let returned = items.len();
			(didl_wrap(&items), returned, total)
		}
		(id, _) if id.starts_with("v:") => {
			let idx: usize = id[2..].parse().unwrap_or(usize::MAX);
			match config.videos.get(idx).and_then(|p| item_xml(idx, p, config)) {
				Some(item) => (didl_wrap(&[item]), 1, 1),
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

fn didl_wrap(entries: &[String]) -> String {
	format!(
		r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
  {}
</DIDL-Lite>"#,
		entries.join("\n  ")
	)
}

fn item_xml(index: usize, path: &Path, config: &DlnaConfig) -> Option<String> {
	let (dir_idx, url_path) = spritz_core::video_url_path(path, &config.video_dirs)?;
	let url = format!(
		"http://{}:{}/v/{dir_idx}/{url_path}",
		config.local_ip, config.http_port
	);
	let title = path.file_name()?.to_string_lossy();
	let ext = path
		.extension()
		.and_then(|e| e.to_str())
		.unwrap_or("")
		.to_lowercase();
	let mime = mime_for_ext(&ext);

	Some(format!(
		r#"<item id="v:{index}" parentID="0" restricted="1">
    <dc:title>{}</dc:title>
    <upnp:class>object.item.videoItem</upnp:class>
    <dc:date>2000-01-01</dc:date>
    <res protocolInfo="http-get:*:{mime}:*">{}</res>
  </item>"#,
		xml_escape(&title),
		xml_escape(&url),
	))
}

fn mime_for_ext(ext: &str) -> &'static str {
	match ext {
		"mp4" | "m4v" => "video/mp4",
		"mkv" => "video/x-matroska",
		"avi" => "video/x-msvideo",
		"mov" => "video/quicktime",
		"webm" => "video/webm",
		"flv" => "video/x-flv",
		_ => "application/octet-stream",
	}
}
