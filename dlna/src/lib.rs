use axum::{
	Router,
	extract::DefaultBodyLimit,
	http::{HeaderMap, StatusCode, header},
	routing::{any, get, post},
};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tower_http::timeout::TimeoutLayer;

pub mod content_dir;
pub mod description;
pub mod event;
pub mod registrar;
pub mod soap;
pub mod ssdp;

#[derive(Clone)]
pub struct FolderNode {
	pub path: PathBuf,
	pub display_name: String,
	/// Direct subfolder indices into `DlnaConfig::folder_nodes`.
	pub subfolder_indices: Vec<usize>,
	/// Indices into `DlnaConfig::media_files` for files sitting directly in this folder.
	pub media_indices: Vec<usize>,
}

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
	/// `dc:date` values parallel to `media_files` (`YYYY-MM-DD`).
	pub media_dates: Vec<String>,
	/// DIDL duration (`H:MM:SS.mmm`); empty when unknown.
	pub media_durations: Vec<String>,
	/// True when a sidecar or embedded cover exists for `/art/{i}`.
	pub media_has_art: Vec<bool>,
	/// Indices into `media_files` for video items (flat Videos container).
	pub video_idx: Vec<usize>,
	/// Indices into `media_files` for audio items (flat Music container).
	pub audio_idx: Vec<usize>,
	/// Flat list of every folder reachable from a source dir that contains
	/// media (directly or transitively). The first `media_dirs.len()` entries
	/// are the source roots; subsequent entries are subfolders discovered while
	/// indexing. Referenced by DIDL ids `f:N` in the "By folder" view.
	pub folder_nodes: Vec<FolderNode>,
	pub event_hub: event::EventHub,
}

/// DLNA protocolInfo 4th field: byte-seek (OP=01), original format (CI=0),
/// and flags for streaming + bg-transfer + conn-stalling + DLNA 1.5.
/// Strict clients like Infuse reject streams without these.
pub const DLNA_CONTENT_FEATURES: &str =
	"DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

const XML_UTF8: &str = "text/xml; charset=\"utf-8\"";
pub(crate) const SERVER: &str = concat!("Linux/5.0 UPnP/1.0 Spritz/", env!("CARGO_PKG_VERSION"));

/// Returns a router covering all /upnp/* endpoints.
/// Generic over S so it merges cleanly with any outer Router<S>.
pub fn router<S: Clone + Send + Sync + 'static>(config: Arc<DlnaConfig>) -> Router<S> {
	let cfg_desc = Arc::clone(&config);
	let cfg_cd = Arc::clone(&config);
	let cfg_cm = Arc::clone(&config);
	let hub_cd = config.event_hub.clone();
	let hub_cm = config.event_hub.clone();
	let hub_mrr = config.event_hub.clone();
	let source_info = content_dir::source_protocol_info();

	Router::new()
		.route(
			"/upnp/icon.png",
			get(|| async {
				(
					[
						(header::CONTENT_TYPE, "image/png"),
						(header::SERVER, SERVER),
					],
					description::DEVICE_ICON_PNG,
				)
			}),
		)
		.route(
			"/upnp/description.xml",
			get(move |headers: HeaderMap| {
				let cfg = Arc::clone(&cfg_desc);
				async move {
					let base = content_dir::public_base(&headers, cfg.local_ip, cfg.http_port);
					let xml = description::device_description(&cfg, &base);
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
			"/upnp/service/mediareceiverregistrar.xml",
			get(|| async {
				(
					[(header::CONTENT_TYPE, XML_UTF8), (header::SERVER, SERVER)],
					description::MEDIARECEIVER_SCPD,
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
		.route(
			"/upnp/control/mediareceiverregistrar",
			post(move |headers, body| async move { registrar::handle(headers, body).await }),
		)
		.route(
			"/upnp/event/contentdirectory",
			any({
				let hub = hub_cd;
				move |req| {
					let hub = hub.clone();
					async move {
						event::handle(
							req,
							hub,
							event::EventService::ContentDirectory,
							String::new(),
						)
						.await
					}
				}
			}),
		)
		.route(
			"/upnp/event/connectionmanager",
			any({
				let hub = hub_cm;
				let source = source_info.clone();
				move |req| {
					let hub = hub.clone();
					let source = source.clone();
					async move {
						event::handle(req, hub, event::EventService::ConnectionManager, source)
							.await
					}
				}
			}),
		)
		.route(
			"/upnp/event/mediareceiverregistrar",
			any({
				let hub = hub_mrr;
				move |req| {
					let hub = hub.clone();
					async move {
						event::handle(
							req,
							hub,
							event::EventService::MediaReceiverRegistrar,
							String::new(),
						)
						.await
					}
				}
			}),
		)
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

pub async fn run_ssdp(
	config: Arc<DlnaConfig>,
	shutdown: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
	ssdp::run(config, shutdown).await
}
