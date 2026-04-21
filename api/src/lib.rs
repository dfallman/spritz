use axum::{
	Router,
	extract::State,
	http::{StatusCode, header, HeaderMap},
	response::IntoResponse,
	routing::get,
};
use spritz_core::{find_videos, video_url_path};
use local_ip_address::local_ip;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::services::ServeDir;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
	video_dirs: Vec<PathBuf>,
	videos: Vec<PathBuf>,
}

pub async fn start_server(port: u16, video_dirs: Vec<PathBuf>) -> anyhow::Result<()> {
	let mut videos = Vec::new();
	for dir in &video_dirs {
		match find_videos(dir) {
			Ok(mut found) => videos.append(&mut found),
			Err(e) => eprintln!("Warning: could not scan {}: {e}", dir.display()),
		}
	}

	println!("Indexed {} video(s):", videos.len());
	for video in &videos {
		println!("  {}", video.display());
	}

	let ip = local_ip().unwrap_or_else(|_| "127.0.0.1".parse().unwrap());

	let dlna_config = Arc::new(dlna::DlnaConfig {
		device_uuid: Uuid::new_v4().to_string(),
		friendly_name: "Spritz Media Server".to_string(),
		http_port: port,
		local_ip: ip,
		video_dirs: video_dirs.clone(),
		videos: videos.clone(),
	});

	tokio::spawn(dlna::run_ssdp(Arc::clone(&dlna_config)));

	let state = Arc::new(AppState { video_dirs, videos });

	// Mount each folder at /v/{index}/ so URLs stay unambiguous across dirs
	let app = state.video_dirs.iter().enumerate().fold(
		Router::new().route("/spritz", get(generate_m3u)),
		|router, (i, dir)| router.nest_service(&format!("/v/{i}"), ServeDir::new(dir)),
	)
	.merge(dlna::router(Arc::clone(&dlna_config)))
	.with_state(Arc::clone(&state));

	let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
	let listener = tokio::net::TcpListener::bind(&addr).await?;

	let port_str = if port == 80 { String::new() } else { format!(":{port}") };
	println!("Serving on http://{ip}{port_str}/spritz");
	println!("DLNA: discoverable as \"Spritz Media Server\" on the local network");

	axum::serve(listener, app).await?;
	Ok(())
}

async fn generate_m3u(headers: HeaderMap, State(state): State<Arc<AppState>>) -> impl IntoResponse {
	if state.videos.is_empty() {
		return (StatusCode::INTERNAL_SERVER_ERROR, "No videos indexed.").into_response();
	}

	let hostname = headers
		.get(header::HOST)
		.and_then(|h| h.to_str().ok())
		.unwrap_or("127.0.0.1");

	let mut m3u = String::from("#EXTM3U\n");

	for video in &state.videos {
		if let Some((i, path)) = video_url_path(video, &state.video_dirs) {
			let filename = video.file_name().unwrap_or_default().to_string_lossy();
			write!(m3u, "#EXTINF:-1,{filename}\n").unwrap();
			write!(m3u, "http://{hostname}/v/{i}/{path}\n").unwrap();
		}
	}

	([(header::CONTENT_TYPE, "audio/x-mpegurl")], m3u).into_response()
}
