use axum::{
	Router,
	body::Body,
	extract::Request,
	http::{StatusCode, header},
	response::Response,
	routing::{any, get, post},
};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
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
	pub video_dirs: Vec<PathBuf>,
	pub videos: Vec<PathBuf>,
	/// File sizes parallel to `videos`; 0 when stat() failed.
	pub video_sizes: Vec<u64>,
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
					([(header::CONTENT_TYPE, XML_UTF8), (header::SERVER, SERVER)], xml)
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
