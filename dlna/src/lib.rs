use axum::{
	Router,
	body::Body,
	extract::{DefaultBodyLimit, Request},
	http::{StatusCode, header},
	response::Response,
	routing::{any, get, post},
};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower_http::timeout::TimeoutLayer;
use uuid::Uuid;

pub mod content_dir;
pub mod description;
pub mod soap;
pub mod ssdp;

#[derive(Clone)]
pub struct DlnaConfig {
	pub device_uuid: String,
	pub friendly_name: String,
	pub http_port: u16,
	pub local_ip: IpAddr,
	pub media_dirs: Vec<PathBuf>,
	pub media_files: Vec<PathBuf>,
	/// File sizes parallel to `media_files`; 0 when stat() failed.
	pub media_sizes: Vec<u64>,
	/// Flat list of every folder reachable from a source dir that contains
	/// media (directly or transitively). The first `media_dirs.len()` entries
	/// are the source roots; subsequent entries are subfolders discovered while
	/// indexing. Referenced by DIDL ids `f:N` in the "By folder" view.
	pub folder_nodes: Vec<FolderNode>,
}

#[derive(Clone)]
pub struct FolderNode {
	pub path: PathBuf,
	pub display_name: String,
	/// Direct subfolder indices into `DlnaConfig::folder_nodes`.
	pub subfolder_indices: Vec<usize>,
	/// Indices into `DlnaConfig::media_files` for files sitting directly in this folder.
	pub media_indices: Vec<usize>,
}

/// DLNA protocolInfo 4th field: byte-seek (OP=01), original format (CI=0),
/// and flags for streaming + bg-transfer + conn-stalling + DLNA 1.5.
/// Strict clients like Infuse reject streams without these.
pub const DLNA_CONTENT_FEATURES: &str =
	"DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

const XML_UTF8: &str = "text/xml; charset=\"utf-8\"";
const SERVER: &str = "Linux/5.0 UPnP/1.0 Spritz/0.1";

/// Returns a router covering all /upnp/* endpoints.
/// Generic over S so it merges cleanly with any outer Router<S>.
pub fn router<S: Clone + Send + Sync + 'static>(config: Arc<DlnaConfig>) -> Router<S> {
	let cfg_desc = Arc::clone(&config);
	let cfg_cd = Arc::clone(&config);
	let cfg_cm = Arc::clone(&config);

	Router::new()
		.route(
			"/upnp/description.xml",
			get(move || {
				let cfg = Arc::clone(&cfg_desc);
				async move {
					let xml = description::device_description(&cfg);
					(
						[(header::CONTENT_TYPE, XML_UTF8), (header::SERVER, SERVER)],
						xml,
					)
				}
			}),
		)
		.route(
			"/upnp/service/contentdirectory.xml",
			get(|| async {
				(
					[(header::CONTENT_TYPE, XML_UTF8), (header::SERVER, SERVER)],
					description::CONTENTDIRECTORY_SCPD,
				)
			}),
		)
		.route(
			"/upnp/service/connectionmanager.xml",
			get(|| async {
				(
					[(header::CONTENT_TYPE, XML_UTF8), (header::SERVER, SERVER)],
					description::CONNECTIONMANAGER_SCPD,
				)
			}),
		)
		.route(
			"/upnp/control/contentdirectory",
			post(move |headers, body| {
				let cfg = Arc::clone(&cfg_cd);
				async move { content_dir::handle_contentdirectory(headers, body, cfg).await }
			}),
		)
		.route(
			"/upnp/control/connectionmanager",
			post(move |headers, body| {
				let cfg = Arc::clone(&cfg_cm);
				async move { content_dir::handle_connectionmanager(headers, body, cfg).await }
			}),
		)
		.route("/upnp/event/contentdirectory", any(event_handler))
		.route("/upnp/event/connectionmanager", any(event_handler))
		// SOAP Browse bodies are <1 KiB in practice; cap at 64 KiB to bound
		// memory if a misbehaving client sends something huge. Applies only
		// to /upnp/* — the /m/{i}/ ServeDir is mounted outside this router.
		.layer(DefaultBodyLimit::max(64 * 1024))
		// UPnP control actions are cheap computations; 30s is generous and
		// protects against stalled connections tying up tasks forever.
		.layer(TimeoutLayer::with_status_code(
			StatusCode::REQUEST_TIMEOUT,
			Duration::from_secs(30),
		))
}

/// Handles SUBSCRIBE and UNSUBSCRIBE for both event endpoints.
/// We don't send actual events; the SID stub is enough to satisfy picky clients.
async fn event_handler(req: Request) -> Response {
	match req.method().as_str() {
		"SUBSCRIBE" => Response::builder()
			.status(200)
			.header("sid", format!("uuid:{}", Uuid::new_v4()))
			.header("timeout", "Second-1800")
			.header("server", SERVER)
			.body(Body::empty())
			.unwrap(),
		"UNSUBSCRIBE" => Response::builder()
			.status(200)
			.header("server", SERVER)
			.body(Body::empty())
			.unwrap(),
		_ => Response::builder()
			.status(StatusCode::METHOD_NOT_ALLOWED.as_u16())
			.body(Body::empty())
			.unwrap(),
	}
}

pub async fn run_ssdp(config: Arc<DlnaConfig>) -> anyhow::Result<()> {
	ssdp::run(config).await
}
