use axum::{
	Router,
	extract::State,
	http::{HeaderMap, HeaderValue, StatusCode, header},
	response::IntoResponse,
	routing::get,
};
use local_ip_address::local_ip;
use spritz_core::{find_media, media_url_path};
use std::fmt::Write;
use std::path::PathBuf;
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
			Err(e) => eprintln!("Warning: could not scan {}: {e}", dir.display()),
		}
	}

	println!("Indexed {} media file(s):", media_files.len());
	for file in &media_files {
		println!("  {}", file.display());
	}

	let media_sizes: Vec<u64> = media_files
		.iter()
		.map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
		.collect();

	let ip = local_ip().unwrap_or_else(|_| "127.0.0.1".parse().unwrap());

	let dlna_config = Arc::new(dlna::DlnaConfig {
		device_uuid: Uuid::new_v4().to_string(),
		friendly_name: "Spritz Media Server".to_string(),
		http_port: port,
		local_ip: ip,
		media_dirs: media_dirs.clone(),
		media_files: media_files.clone(),
		media_sizes,
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
			Router::new().route("/spritz", get(generate_m3u)),
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
