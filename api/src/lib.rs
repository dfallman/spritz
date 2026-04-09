use axum::{
	Router,
	extract::State,
	http::{StatusCode, header, HeaderMap},
	response::IntoResponse,
	routing::get,
};
use spritz_core::find_videos;
use local_ip_address::local_ip;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::services::ServeDir;

#[derive(Clone)]
struct AppState {
	video_dir: PathBuf,
	videos: Vec<PathBuf>,
}

pub async fn start_server(port: u16, video_dir: PathBuf) -> anyhow::Result<()> {
	// Index videos ONCE sequentially upon startup
	let videos = match find_videos(&video_dir) {
		Ok(v) => {
			println!("Indexed {} video(s):", v.len());
			for video in &v {
				println!("- {}", video.display());
			}
			v
		}
		Err(e) => {
			println!("Scanning warning: could not read video directory: {}", e);
			Vec::new()
		}
	};

	let state = AppState {
		video_dir: video_dir.clone(),
		videos,
	};

	let app = Router::new()
		.route("/spritz", get(generate_m3u))
		.nest_service("/v", ServeDir::new(&video_dir))
		.with_state(Arc::new(state));

	let addr = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
	let listener = tokio::net::TcpListener::bind(&addr).await?;

	let ip = local_ip().unwrap_or_else(|_| "127.0.0.1".parse().unwrap());
	let port_str = if port == 80 {
		String::new()
	} else {
		format!(":{}", port)
	};
	println!("Server running! Plug this link into VLC or other media player supporting M3U:");
	println!("http://{}{}/spritz", ip, port_str);

	axum::serve(listener, app).await?;
	Ok(())
}

async fn generate_m3u(headers: HeaderMap, State(state): State<Arc<AppState>>) -> impl IntoResponse {
	let mut m3u = String::from("#EXTM3U\n");

	let hostname = headers
		.get(header::HOST)
		.and_then(|h| h.to_str().ok())
		.unwrap_or("127.0.0.1");

	if state.videos.is_empty() {
		return (
			StatusCode::INTERNAL_SERVER_ERROR,
			"No videos were indexed at deployment.",
		)
			.into_response();
	}

	for video in &state.videos {
		if let Ok(relative) = video.strip_prefix(&state.video_dir) {
			#[cfg(windows)]
			let path_str = relative.to_string_lossy().replace('\\', "/");
			#[cfg(not(windows))]
			let path_str = relative.to_string_lossy().to_string();

			let url_encoded_path = path_str
				.split('/')
				.map(|segment| urlencoding::encode(segment).into_owned())
				.collect::<Vec<_>>()
				.join("/");

			let filename = video.file_name().unwrap_or_default().to_string_lossy();

			m3u.push_str(&format!("#EXTINF:-1,{}\n", filename));
			m3u.push_str(&format!(
				"http://{}/v/{}\n",
				hostname, url_encoded_path
			));
		}
	}

	([(header::CONTENT_TYPE, "audio/x-mpegurl")], m3u).into_response()
}
