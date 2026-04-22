use axum::{
	Router,
	extract::State,
	http::{HeaderMap, HeaderValue, StatusCode, header},
	response::IntoResponse,
	routing::get,
};
use dlna::FolderNode;
use local_ip_address::local_ip;
use spritz_core::{find_media, media_url_path};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
	media_dirs: Vec<PathBuf>,
	media_files: Vec<PathBuf>,
}

pub async fn start_server(port: u16, media_dirs: Vec<PathBuf>) -> anyhow::Result<()> {
	let mut media_files = Vec::new();
	for dir in &media_dirs {
		match find_media(dir) {
			Ok(mut found) => media_files.append(&mut found),
			Err(e) => tracing::warn!("could not scan {}: {e}", dir.display()),
		}
	}

	println!("Indexed {} media file(s)", media_files.len());
	for file in &media_files {
		tracing::debug!("  {}", file.display());
	}

	let media_sizes: Vec<u64> = media_files
		.iter()
		.map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
		.collect();

	let folder_nodes = build_folder_tree(&media_dirs, &media_files);

	let ip = local_ip().unwrap_or_else(|_| "127.0.0.1".parse().unwrap());

	let dlna_config = Arc::new(dlna::DlnaConfig {
		device_uuid: Uuid::new_v4().to_string(),
		friendly_name: "Spritz Media Server".to_string(),
		http_port: port,
		local_ip: ip,
		media_dirs: media_dirs.clone(),
		media_files: media_files.clone(),
		media_sizes,
		folder_nodes,
	});

	tokio::spawn(dlna::run_ssdp(Arc::clone(&dlna_config)));

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

	// Mount each folder at /m/{index}/ so URLs stay unambiguous across dirs
	let app = state
		.media_dirs
		.iter()
		.enumerate()
		.fold(
			Router::new()
				.route("/spritz", get(generate_m3u))
				.route("/health", get(|| async { "ok" })),
			|router, (i, dir)| {
				router.nest_service(
					&format!("/m/{i}"),
					tower::ServiceBuilder::new()
						.layer(dlna_layer.clone())
						.service(ServeDir::new(dir)),
				)
			},
		)
		.merge(dlna::router(Arc::clone(&dlna_config)))
		.with_state(Arc::clone(&state));

	let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
	let listener = tokio::net::TcpListener::bind(&addr).await?;

	let port_str = if port == 80 {
		String::new()
	} else {
		format!(":{port}")
	};
	println!("Serving on http://{ip}{port_str}/spritz");
	println!("DLNA: discoverable as \"Spritz Media Server\" on the local network");

	axum::serve(listener, app).await?;
	Ok(())
}

async fn generate_m3u(headers: HeaderMap, State(state): State<Arc<AppState>>) -> impl IntoResponse {
	if state.media_files.is_empty() {
		return (StatusCode::INTERNAL_SERVER_ERROR, "No media indexed.").into_response();
	}

	let hostname = headers
		.get(header::HOST)
		.and_then(|h| h.to_str().ok())
		.unwrap_or("127.0.0.1");

	let mut m3u = String::from("#EXTM3U\n");

	for file in &state.media_files {
		if let Some((i, path)) = media_url_path(file, &state.media_dirs) {
			let filename = file.file_name().unwrap_or_default().to_string_lossy();
			writeln!(m3u, "#EXTINF:-1,{filename}").unwrap();
			writeln!(m3u, "http://{hostname}/m/{i}/{path}").unwrap();
		}
	}

	([(header::CONTENT_TYPE, "audio/x-mpegurl")], m3u).into_response()
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
