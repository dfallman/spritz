use axum::{
	Router,
	extract::State,
	http::{HeaderMap, HeaderValue, StatusCode, header},
	middleware::{self, Next},
	response::IntoResponse,
	routing::get,
};
use dlna::FolderNode;
use local_ip_address::local_ip;
use spritz_core::{
	album_art_sidecar, find_media, is_audio, media_url_path, safe_media_path, sort_media_paths,
	unique_canonical_roots, valid_http_host,
};
use std::collections::HashMap;
use std::fmt::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;
use tower_http::services::ServeFile;
use tower_http::set_header::SetResponseHeaderLayer;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
	media_dirs: Vec<PathBuf>,
	media_files: Vec<PathBuf>,
}

pub async fn start_server(
	port: u16,
	bind: IpAddr,
	name: &str,
	media_dirs: Vec<PathBuf>,
) -> anyhow::Result<()> {
	let media_dirs = unique_canonical_roots(&media_dirs);
	if media_dirs.is_empty() {
		anyhow::bail!("no readable source directories");
	}

	let listener = bind_http(bind, port).await?;
	let port = listener.local_addr()?.port();

	let dirs = media_dirs.clone();
	let indexed = tokio::task::spawn_blocking(move || {
		let mut media_files = Vec::new();
		for dir in &dirs {
			match find_media(dir) {
				Ok(mut found) => media_files.append(&mut found),
				Err(e) => tracing::warn!("could not scan {}: {e}", dir.display()),
			}
		}
		sort_media_paths(&mut media_files);

		let mut media_sizes = Vec::with_capacity(media_files.len());
		let mut media_dates = Vec::with_capacity(media_files.len());
		let mut media_durations = Vec::with_capacity(media_files.len());
		let mut media_resolutions = Vec::with_capacity(media_files.len());
		let mut media_pns = Vec::with_capacity(media_files.len());
		let mut media_has_art = Vec::with_capacity(media_files.len());
		for p in &media_files {
			match std::fs::metadata(p) {
				Ok(m) => {
					media_sizes.push(m.len());
					media_dates.push(
						m.modified()
							.map(spritz_core::dc_date)
							.unwrap_or_else(|_| "2000-01-01".into()),
					);
				}
				Err(_) => {
					media_sizes.push(0);
					media_dates.push("2000-01-01".into());
				}
			}
			let info = spritz_core::probe_media(p);
			let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
			media_durations.push(
				info.duration
					.map(spritz_core::format_dlna_duration)
					.unwrap_or_default(),
			);
			media_resolutions.push(info.resolution_attr().unwrap_or_default());
			media_pns.push(
				spritz_core::dlna_org_pn_for(ext, &info)
					.unwrap_or("")
					.to_string(),
			);
			media_has_art.push(
				spritz_core::album_art_sidecar(p).is_some() || spritz_core::has_embedded_art(p),
			);
		}

		let mut folder_nodes = build_folder_tree(&dirs, &media_files);
		sort_folder_tree(&mut folder_nodes, &media_files);

		let mut video_idx = Vec::new();
		let mut audio_idx = Vec::new();
		for (i, path) in media_files.iter().enumerate() {
			if is_audio(path) {
				audio_idx.push(i);
			} else {
				video_idx.push(i);
			}
		}

		(
			media_files,
			media_sizes,
			media_dates,
			media_durations,
			media_resolutions,
			media_pns,
			media_has_art,
			folder_nodes,
			video_idx,
			audio_idx,
		)
	})
	.await?;

	let (
		media_files,
		media_sizes,
		media_dates,
		media_durations,
		media_resolutions,
		media_pns,
		media_has_art,
		folder_nodes,
		video_idx,
		audio_idx,
	) = indexed;

	println!("Indexed {} media file(s)", media_files.len());
	for file in &media_files {
		tracing::debug!("  {}", file.display());
	}

	let ip = advertised_ip(bind, local_ip().ok());
	let friendly_name = friendly_name(name);

	let dlna_config = Arc::new(dlna::DlnaConfig {
		device_uuid: stable_device_uuid(),
		friendly_name: friendly_name.clone(),
		http_port: port,
		local_ip: ip,
		media_dirs: media_dirs.clone(),
		media_files: media_files.clone(),
		media_sizes,
		media_dates,
		media_durations,
		media_resolutions,
		media_pns,
		media_has_art,
		video_idx,
		audio_idx,
		folder_nodes,
		event_hub: dlna::event::EventHub::default(),
	});

	let state = Arc::new(AppState {
		media_dirs,
		media_files,
	});

	// Inject DLNA headers on every /m/{i}/ response. Strict clients (Infuse)
	// refuse to play a stream missing these, even if the raw HTTP is fine.
	let transfer_mode = HeaderValue::from_static("Streaming");
	let content_features = HeaderValue::from_static(dlna::DLNA_CONTENT_FEATURES);
	let dlna_layer = tower::ServiceBuilder::new()
		.layer(SetResponseHeaderLayer::if_not_present(
			header::HeaderName::from_static("transfermode.dlna.org"),
			transfer_mode,
		))
		.layer(SetResponseHeaderLayer::if_not_present(
			header::HeaderName::from_static("contentfeatures.dlna.org"),
			content_features,
		));

	// Mount media at /m/{index}/... — only indexed extensions, no symlink follow.
	let media_routes = Router::new()
		.route("/m/{idx}/{*path}", get(serve_media).head(serve_media))
		.layer(dlna_layer);

	let app = Router::new()
		.route("/spritz", get(generate_m3u))
		.route("/health", get(|| async { "ok" }))
		.route("/art/{idx}", get(serve_art).head(serve_art))
		.merge(media_routes)
		.merge(dlna::router(Arc::clone(&dlna_config)))
		.layer(middleware::from_fn(access_log))
		.with_state(Arc::clone(&state));

	// Bind happened before the scan so a busy port fails fast, and SSDP
	// only starts once HTTP is listening (clients fetching LOCATION succeed).
	let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
	let ssdp = tokio::spawn(dlna::run_ssdp(Arc::clone(&dlna_config), shutdown_rx));

	let port_str = if port == 80 {
		String::new()
	} else {
		format!(":{port}")
	};
	println!("Serving on http://{ip}{port_str}/spritz");
	println!("DLNA: discoverable as \"{friendly_name}\" on the local network");

	tokio::select! {
		result = axum::serve(listener, app) => {
			let _ = shutdown_tx.send(());
			let _ = ssdp.await;
			result?;
		}
		_ = wait_for_shutdown_signal() => {
			tracing::info!("shutdown signal received");
			let _ = shutdown_tx.send(());
			let _ = ssdp.await;
		}
	}
	Ok(())
}

async fn bind_http(bind: IpAddr, port: u16) -> anyhow::Result<tokio::net::TcpListener> {
	if bind.is_unspecified() {
		match bind_dual_stack(port) {
			Ok(listener) => return Ok(listener),
			Err(e) => tracing::warn!("dual-stack HTTP bind failed ({e}); using {bind}"),
		}
	}
	Ok(tokio::net::TcpListener::bind(std::net::SocketAddr::from((bind, port))).await?)
}

fn bind_dual_stack(port: u16) -> anyhow::Result<tokio::net::TcpListener> {
	use socket2::{Domain, Protocol, Socket, Type};
	let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
	socket.set_only_v6(false)?;
	socket.set_reuse_address(true)?;
	socket.set_nonblocking(true)?;
	socket.bind(&socket2::SockAddr::from(std::net::SocketAddr::from((
		std::net::Ipv6Addr::UNSPECIFIED,
		port,
	))))?;
	socket.listen(1024)?;
	let std_listener: std::net::TcpListener = socket.into();
	Ok(tokio::net::TcpListener::from_std(std_listener)?)
}

async fn access_log(req: axum::extract::Request, next: Next) -> axum::response::Response {
	let method = req.method().clone();
	let uri = req.uri().clone();
	let res = next.run(req).await;
	let path = uri.path();
	if path.starts_with("/m/") || path.starts_with("/upnp/") || path.starts_with("/art/") {
		tracing::info!("{method} {uri} -> {}", res.status());
	}
	res
}

fn advertised_ip(bind: IpAddr, discovered: Option<IpAddr>) -> IpAddr {
	if !bind.is_unspecified() {
		bind
	} else {
		discovered.unwrap_or_else(|| "127.0.0.1".parse().unwrap())
	}
}

const DEFAULT_FRIENDLY_NAME: &str = "Spritz Media Server";
const MAX_FRIENDLY_NAME_CHARS: usize = 64;

fn friendly_name(name: &str) -> String {
	let t = name.trim();
	if t.is_empty() {
		return DEFAULT_FRIENDLY_NAME.to_string();
	}
	t.chars().take(MAX_FRIENDLY_NAME_CHARS).collect()
}

async fn wait_for_shutdown_signal() {
	let ctrl_c = tokio::signal::ctrl_c();
	#[cfg(unix)]
	{
		let mut sigterm =
			match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
				Ok(s) => s,
				Err(e) => {
					tracing::warn!("could not install SIGTERM handler: {e}");
					let _ = ctrl_c.await;
					return;
				}
			};
		tokio::select! {
			_ = ctrl_c => {}
			_ = sigterm.recv() => {}
		}
	}
	#[cfg(not(unix))]
	{
		let _ = ctrl_c.await;
	}
}

async fn generate_m3u(headers: HeaderMap, State(state): State<Arc<AppState>>) -> impl IntoResponse {
	let hostname = headers
		.get(header::HOST)
		.and_then(|h| h.to_str().ok())
		.filter(|h| valid_http_host(h))
		.unwrap_or("127.0.0.1");

	let m3u = m3u_playlist(&state.media_files, &state.media_dirs, hostname);
	([(header::CONTENT_TYPE, "audio/x-mpegurl")], m3u).into_response()
}

fn m3u_playlist(files: &[PathBuf], dirs: &[PathBuf], hostname: &str) -> String {
	let mut m3u = String::from("#EXTM3U\n");
	for file in files {
		if let Some((i, path)) = media_url_path(file, dirs) {
			let filename = file.file_name().unwrap_or_default().to_string_lossy();
			writeln!(m3u, "#EXTINF:-1,{filename}").unwrap();
			writeln!(m3u, "http://{hostname}/m/{i}/{path}").unwrap();
		}
	}
	m3u
}

async fn serve_media(
	axum::extract::Path((idx, tail)): axum::extract::Path<(usize, String)>,
	State(state): State<Arc<AppState>>,
	req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
	let Some(root) = state.media_dirs.get(idx) else {
		return StatusCode::NOT_FOUND.into_response();
	};
	let Some(path) = safe_media_path(root, Path::new(&tail)) else {
		return StatusCode::NOT_FOUND.into_response();
	};
	match ServeFile::new(path).oneshot(req).await {
		Ok(res) => res.into_response(),
		Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
	}
}

async fn serve_art(
	axum::extract::Path(idx): axum::extract::Path<usize>,
	State(state): State<Arc<AppState>>,
	req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
	let Some(media) = state.media_files.get(idx) else {
		return StatusCode::NOT_FOUND.into_response();
	};
	let Some(art) = album_art_sidecar(media) else {
		return StatusCode::NOT_FOUND.into_response();
	};
	if !state.media_dirs.iter().any(|root| art.starts_with(root)) {
		return StatusCode::NOT_FOUND.into_response();
	}
	match ServeFile::new(art).oneshot(req).await {
		Ok(res) => res.into_response(),
		Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
	}
}

fn stable_device_uuid() -> String {
	uuid_from_identity(&machine_identity())
}

fn uuid_from_identity(identity: &str) -> String {
	Uuid::new_v5(
		&Uuid::NAMESPACE_URL,
		format!("https://github.com/dfallman/spritz#{identity}").as_bytes(),
	)
	.to_string()
}

fn machine_identity() -> String {
	for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
		if let Ok(id) = std::fs::read_to_string(path) {
			let id = id.trim();
			if !id.is_empty() {
				return id.to_string();
			}
		}
	}
	hostname::get()
		.ok()
		.and_then(|h| h.into_string().ok())
		.filter(|h| !h.is_empty())
		.unwrap_or_else(|| "spritz-unknown".into())
}

/// Build the flat `folder_nodes` vector that powers the "By folder" browse
/// hierarchy. The first `media_dirs.len()` entries are the source roots
/// (always present, even when empty, so `f:N` indices match source indices).
/// Subsequent entries are intermediate directories discovered by climbing
/// from each media file up to its source root.
fn build_folder_tree(media_dirs: &[PathBuf], media_files: &[PathBuf]) -> Vec<FolderNode> {
	let mut nodes: Vec<FolderNode> = Vec::with_capacity(media_dirs.len());
	let mut path_to_idx: HashMap<PathBuf, usize> = HashMap::new();

	for (src_idx, dir) in media_dirs.iter().enumerate() {
		let display_name = dir
			.file_name()
			.map(|n| n.to_string_lossy().into_owned())
			.unwrap_or_else(|| format!("Source {src_idx}"));
		path_to_idx.insert(dir.clone(), nodes.len());
		nodes.push(FolderNode {
			path: dir.clone(),
			display_name,
			subfolder_indices: Vec::new(),
			media_indices: Vec::new(),
		});
	}

	for (media_i, file) in media_files.iter().enumerate() {
		// Skip files not under any declared source dir (defensive; shouldn't happen).
		if !media_dirs.iter().any(|d| file.starts_with(d)) {
			continue;
		}
		let Some(parent) = file.parent() else {
			continue;
		};
		let parent_idx = ensure_folder(&mut nodes, &mut path_to_idx, parent);
		nodes[parent_idx].media_indices.push(media_i);
	}

	nodes
}

fn sort_folder_tree(nodes: &mut [FolderNode], media_files: &[PathBuf]) {
	for i in 0..nodes.len() {
		let mut subs = nodes[i].subfolder_indices.clone();
		subs.sort_by_key(|&j| nodes[j].display_name.to_lowercase());
		nodes[i].subfolder_indices = subs;
		nodes[i].media_indices.sort_by(|&a, &b| {
			let an = media_files
				.get(a)
				.and_then(|p| p.file_name())
				.map(|n| n.to_string_lossy().to_lowercase())
				.unwrap_or_default();
			let bn = media_files
				.get(b)
				.and_then(|p| p.file_name())
				.map(|n| n.to_string_lossy().to_lowercase())
				.unwrap_or_default();
			an.cmp(&bn)
		});
	}
}

/// Recursively ensure `path` has a corresponding `FolderNode`, creating
/// intermediate nodes along the way and wiring parent→child `subfolder_indices`.
/// Recursion terminates when we hit a source root (pre-registered in `path_to_idx`).
fn ensure_folder(
	nodes: &mut Vec<FolderNode>,
	path_to_idx: &mut HashMap<PathBuf, usize>,
	path: &Path,
) -> usize {
	if let Some(&idx) = path_to_idx.get(path) {
		return idx;
	}

	let parent = path
		.parent()
		.expect("media file path always descends from a source root");
	let parent_idx = ensure_folder(nodes, path_to_idx, parent);

	let display_name = path
		.file_name()
		.map(|n| n.to_string_lossy().into_owned())
		.unwrap_or_default();
	let new_idx = nodes.len();
	path_to_idx.insert(path.to_path_buf(), new_idx);
	nodes.push(FolderNode {
		path: path.to_path_buf(),
		display_name,
		subfolder_indices: Vec::new(),
		media_indices: Vec::new(),
	});
	nodes[parent_idx].subfolder_indices.push(new_idx);
	new_idx
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn uuid_from_identity_is_stable_for_the_same_input() {
		let a = uuid_from_identity("host-abc");
		let b = uuid_from_identity("host-abc");
		assert_eq!(a, b);
		assert_ne!(a, uuid_from_identity("host-xyz"));
	}

	#[test]
	fn uuid_from_identity_is_a_canonical_uuid_string() {
		let id = uuid_from_identity("nas-1");
		assert!(Uuid::parse_str(&id).is_ok(), "{id}");
	}

	#[test]
	fn empty_library_m3u_is_a_valid_empty_playlist() {
		assert_eq!(m3u_playlist(&[], &[], "127.0.0.1"), "#EXTM3U\n");
	}

	#[test]
	fn advertised_ip_prefers_a_specific_bind_address() {
		let discovered = "10.0.0.1".parse().unwrap();
		assert_eq!(
			advertised_ip("192.168.1.5".parse().unwrap(), Some(discovered)),
			"192.168.1.5".parse::<std::net::IpAddr>().unwrap()
		);
		assert_eq!(
			advertised_ip("0.0.0.0".parse().unwrap(), Some(discovered)),
			discovered
		);
	}

	#[test]
	fn friendly_name_defaults_and_truncates() {
		assert_eq!(friendly_name(""), "Spritz Media Server");
		assert_eq!(friendly_name("  Living Room  "), "Living Room");
		assert_eq!(friendly_name(&"x".repeat(80)).len(), 64);
	}
}
