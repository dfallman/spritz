use axum::{
	Router,
	extract::State,
	http::{HeaderMap, HeaderValue, Method, StatusCode, header},
	middleware::{self, Next},
	response::IntoResponse,
	routing::get,
};
use dlna::FolderNode;
use local_ip_address::local_ip;
use spritz_core::{
	album_art_sidecar, decode_rel_path, find_media, is_audio, media_url_path, open_media_file,
	sort_media_paths, unique_canonical_roots, valid_http_host,
};
use std::collections::HashMap;
use std::fmt::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_http::set_header::SetResponseHeaderLayer;
use uuid::Uuid;

/// Shared HTTP state for the media, art, and M3U handlers.
#[derive(Clone)]
pub struct AppState {
	pub media_dirs: Vec<PathBuf>,
	pub media_files: Vec<PathBuf>,
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

	let bound = bind_http(bind, port).await?;
	let listener = bound.listener;
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

		let records = describe_media(&media_files);
		let mut media_sizes = Vec::with_capacity(records.len());
		let mut media_dates = Vec::with_capacity(records.len());
		let mut media_has_art = Vec::with_capacity(records.len());
		let mut media_subs = Vec::with_capacity(records.len());
		for record in records {
			media_sizes.push(record.size);
			media_dates.push(record.date);
			media_has_art.push(record.has_art);
			media_subs.push(record.subs);
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
			media_has_art,
			media_subs,
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
		media_has_art,
		media_subs,
		folder_nodes,
		video_idx,
		audio_idx,
	) = indexed;

	let probes = dlna::ProbeCache::empty(media_files.len());
	spawn_probe(media_files.clone(), std::sync::Arc::clone(&probes));

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
		http_ipv4: bound.ipv4,
		http_ipv6: bound.ipv6,
		media_dirs: media_dirs.clone(),
		media_files: media_files.clone(),
		media_sizes,
		media_dates,
		probes,
		media_has_art,
		media_subs,
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

	// Bind happened before the scan so a busy port fails fast. HTTP and SSDP
	// start with empty duration and resolution; `spawn_probe` fills those in.
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
		result = serve_http(listener, app) => {
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

/// Serve `app` with peer addresses attached. The event handlers read
/// `ConnectInfo<SocketAddr>` to compare the SUBSCRIBE callback host against
/// the peer; without it every new subscription is refused.
pub async fn serve_http(listener: tokio::net::TcpListener, app: Router) -> std::io::Result<()> {
	axum::serve(
		listener,
		app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
	)
	.await
}

/// HTTP listener plus which IP families it actually accepts.
pub struct BoundHttp {
	pub listener: tokio::net::TcpListener,
	pub ipv4: bool,
	pub ipv6: bool,
}

/// Bind the HTTP listener. Unspecified `bind` tries dual-stack first.
pub async fn bind_http(bind: IpAddr, port: u16) -> anyhow::Result<BoundHttp> {
	if bind.is_unspecified() {
		match bind_dual_stack(port) {
			Ok(listener) => {
				return Ok(BoundHttp {
					listener,
					ipv4: true,
					ipv6: true,
				});
			}
			Err(e) => tracing::warn!("dual-stack HTTP bind failed ({e}); using {bind}"),
		}
	}
	let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from((bind, port))).await?;
	// 0.0.0.0 is IPv4. :: is IPv6. Unspecified is not both.
	Ok(BoundHttp {
		listener,
		ipv4: bind.is_ipv4(),
		ipv6: bind.is_ipv6(),
	})
}

struct FileRecord {
	size: u64,
	date: String,
	has_art: bool,
	subs: u8,
}

fn file_record(path: &Path) -> FileRecord {
	let (size, date) = match std::fs::metadata(path) {
		Ok(meta) => (
			meta.len(),
			meta.modified()
				.map(spritz_core::dc_date)
				.unwrap_or_else(|_| "2000-01-01".into()),
		),
		Err(_) => (0, "2000-01-01".into()),
	};
	FileRecord {
		size,
		date,
		has_art: spritz_core::album_art_sidecar(path).is_some()
			|| spritz_core::has_embedded_art(path),
		subs: spritz_core::sidecar_subtitle_bits(path),
	}
}

struct ProbeFields {
	duration: String,
	resolution: String,
	pn: String,
}

fn probe_fields(path: &Path) -> ProbeFields {
	let info = spritz_core::probe_media(path);
	let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
	ProbeFields {
		duration: info
			.duration
			.map(spritz_core::format_dlna_duration)
			.unwrap_or_default(),
		resolution: info.resolution_attr().unwrap_or_default(),
		pn: spritz_core::dlna_org_pn_for(ext, &info)
			.unwrap_or("")
			.to_string(),
	}
}

fn spawn_probe(files: Vec<PathBuf>, probes: std::sync::Arc<std::sync::RwLock<dlna::ProbeCache>>) {
	if files.is_empty() {
		return;
	}
	let _ = std::thread::Builder::new()
		.name("spritz-probe".into())
		.spawn(move || fill_probes(&files, &probes));
}

fn fill_probes(files: &[PathBuf], probes: &std::sync::RwLock<dlna::ProbeCache>) {
	if files.is_empty() {
		return;
	}
	let workers = std::thread::available_parallelism()
		.map(|n| n.get())
		.unwrap_or(1)
		.min(files.len())
		.max(1);
	let chunk = files.len().div_ceil(workers);
	std::thread::scope(|scope| {
		for (offset, chunk_paths) in files.chunks(chunk).enumerate() {
			let start = offset * chunk;
			scope.spawn(move || {
				for (i, path) in chunk_paths.iter().enumerate() {
					let fields = probe_fields(path);
					let mut guard = probes.write().unwrap_or_else(|err| err.into_inner());
					let idx = start + i;
					if let Some(slot) = guard.durations.get_mut(idx) {
						*slot = fields.duration;
					}
					if let Some(slot) = guard.resolutions.get_mut(idx) {
						*slot = fields.resolution;
					}
					if let Some(slot) = guard.pns.get_mut(idx) {
						*slot = fields.pn;
					}
				}
			});
		}
	});
}

/// Stat files on a thread pool. Output stays aligned with `files`.
/// Duration and resolution are filled later by `fill_probes`.
fn describe_media(files: &[PathBuf]) -> Vec<FileRecord> {
	if files.is_empty() {
		return Vec::new();
	}
	let workers = std::thread::available_parallelism()
		.map(|n| n.get())
		.unwrap_or(1)
		.min(files.len())
		.max(1);
	let chunk = files.len().div_ceil(workers);
	let mut slots: Vec<Option<FileRecord>> = (0..files.len()).map(|_| None).collect();
	std::thread::scope(|scope| {
		for (files_chunk, slots_chunk) in files.chunks(chunk).zip(slots.chunks_mut(chunk)) {
			scope.spawn(move || {
				for (slot, path) in slots_chunk.iter_mut().zip(files_chunk) {
					*slot = Some(file_record(path));
				}
			});
		}
	});
	slots
		.into_iter()
		.map(|slot| slot.expect("every media file is described"))
		.collect()
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

/// Access log for `/m/`, `/upnp/`, and `/art/` requests.
pub async fn access_log(req: axum::extract::Request, next: Next) -> axum::response::Response {
	let method = req.method().clone();
	let uri = req.uri().clone();
	let res = next.run(req).await;
	let path = uri.path();
	if path.starts_with("/m/") || path.starts_with("/upnp/") || path.starts_with("/art/") {
		tracing::info!("{method} {uri} -> {}", res.status());
	}
	res
}

/// The IP advertised in SSDP `LOCATION` and DIDL URLs.
pub fn advertised_ip(bind: IpAddr, discovered: Option<IpAddr>) -> IpAddr {
	if !bind.is_unspecified() {
		bind
	} else {
		discovered.unwrap_or_else(|| "127.0.0.1".parse().unwrap())
	}
}

/// `advertised_ip` using the host's primary local address when `bind` is unspecified.
pub fn discover_advertised_ip(bind: IpAddr) -> IpAddr {
	advertised_ip(bind, local_ip().ok())
}

const DEFAULT_FRIENDLY_NAME: &str = "Spritz Media Server";
const MAX_FRIENDLY_NAME_CHARS: usize = 64;

/// Trim and cap a friendly name; empty input falls back to the default.
pub fn friendly_name(name: &str) -> String {
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

/// M3U playlist handler for `GET /spritz`.
pub async fn generate_m3u(
	headers: HeaderMap,
	State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
	let hostname = headers
		.get(header::HOST)
		.and_then(|h| h.to_str().ok())
		.filter(|h| valid_http_host(h))
		.unwrap_or("127.0.0.1");

	let m3u = m3u_playlist(&state.media_files, &state.media_dirs, hostname);
	([(header::CONTENT_TYPE, "audio/x-mpegurl")], m3u).into_response()
}

/// Render an M3U playlist for the given files.
pub fn m3u_playlist(files: &[PathBuf], dirs: &[PathBuf], hostname: &str) -> String {
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

/// Media file handler for `GET|HEAD /m/{idx}/{*path}`.
///
/// The tail is taken from the raw request URI, which stays percent-encoded
/// ASCII, so a non-UTF-8 filename can round-trip. Axum's `Path` extractor
/// would reject those bytes before this handler ran.
pub async fn serve_media(
	State(state): State<Arc<AppState>>,
	req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
	let Some((idx, relative)) = media_request(req.uri().path()) else {
		return StatusCode::NOT_FOUND.into_response();
	};
	let Some(root) = state.media_dirs.get(idx).cloned() else {
		return StatusCode::NOT_FOUND.into_response();
	};
	let opened = tokio::task::spawn_blocking(move || open_media_file(&root, &relative))
		.await
		.ok()
		.flatten();
	let Some((file, mime)) = opened else {
		return StatusCode::NOT_FOUND.into_response();
	};
	serve_opened(file, mime, req).await
}

fn media_request(uri_path: &str) -> Option<(usize, PathBuf)> {
	let rest = uri_path.strip_prefix("/m/")?;
	let (idx, tail) = rest.split_once('/')?;
	if idx.is_empty() || tail.is_empty() {
		return None;
	}
	let idx = idx.parse().ok()?;
	Some((idx, decode_rel_path(tail)?))
}

/// Album art handler for `GET|HEAD /art/{idx}`.
pub async fn serve_art(
	axum::extract::Path(idx): axum::extract::Path<usize>,
	State(state): State<Arc<AppState>>,
	req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
	let Some(media) = state.media_files.get(idx).cloned() else {
		return StatusCode::NOT_FOUND.into_response();
	};
	let roots = state.media_dirs.clone();
	let opened = tokio::task::spawn_blocking(move || {
		let art = album_art_sidecar(&media)?;
		roots.iter().find_map(|root| {
			let relative = art.strip_prefix(root).ok()?;
			open_media_file(root, relative)
		})
	})
	.await
	.ok()
	.flatten();
	let Some((file, mime)) = opened else {
		return StatusCode::NOT_FOUND.into_response();
	};
	serve_opened(file, mime, req).await
}

enum ByteRange {
	Full,
	/// Inclusive byte range.
	Part {
		start: u64,
		end: u64,
	},
	Unsatisfiable,
}

/// One `Range: bytes=` interval. Multiple ranges are ignored (the response is
/// the whole file), which RFC 9110 allows. A range the file cannot satisfy is
/// [`ByteRange::Unsatisfiable`].
fn parse_byte_range(header: Option<&str>, len: u64) -> ByteRange {
	let Some(header) = header else {
		return ByteRange::Full;
	};
	let Some(spec) = header.trim().strip_prefix("bytes=") else {
		return ByteRange::Full;
	};
	if spec.contains(',') {
		return ByteRange::Full;
	}
	if len == 0 {
		return ByteRange::Unsatisfiable;
	}
	if let Some(suffix) = spec.strip_prefix('-') {
		let Ok(n) = suffix.parse::<u64>() else {
			return ByteRange::Unsatisfiable;
		};
		if n == 0 {
			return ByteRange::Unsatisfiable;
		}
		let n = n.min(len);
		return ByteRange::Part {
			start: len - n,
			end: len - 1,
		};
	}
	let Some((start_s, end_s)) = spec.split_once('-') else {
		return ByteRange::Unsatisfiable;
	};
	let Ok(start) = start_s.parse::<u64>() else {
		return ByteRange::Unsatisfiable;
	};
	if start >= len {
		return ByteRange::Unsatisfiable;
	}
	let end = if end_s.is_empty() {
		len - 1
	} else {
		let Ok(end) = end_s.parse::<u64>() else {
			return ByteRange::Unsatisfiable;
		};
		if end < start {
			return ByteRange::Unsatisfiable;
		}
		end.min(len - 1)
	};
	ByteRange::Part { start, end }
}

/// Read size per blocking-pool hop while streaming a file. tokio-util's
/// default is 4 KiB, which is a million round trips for a 4 GB video.
const STREAM_CHUNK: usize = 64 * 1024;

fn header_len(n: u64) -> HeaderValue {
	HeaderValue::from_str(&n.to_string()).expect("decimal length is a valid header")
}

/// Stream an already-opened file. Range requests are answered from this
/// handle, so the path is not opened again.
async fn serve_opened(
	file: std::fs::File,
	mime: &'static str,
	req: axum::http::Request<axum::body::Body>,
) -> axum::response::Response {
	use tokio::io::{AsyncReadExt, AsyncSeekExt};
	use tokio_util::io::ReaderStream;

	let len = match file.metadata() {
		Ok(meta) => meta.len(),
		Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
	};
	let head = req.method() == Method::HEAD;
	let range = req
		.headers()
		.get(header::RANGE)
		.and_then(|value| value.to_str().ok())
		.map(str::to_string);
	let mut headers = HeaderMap::new();
	headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
	headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

	match parse_byte_range(range.as_deref(), len) {
		ByteRange::Unsatisfiable => {
			headers.insert(
				header::CONTENT_RANGE,
				HeaderValue::from_str(&format!("bytes */{len}"))
					.expect("content-range is a valid header"),
			);
			(StatusCode::RANGE_NOT_SATISFIABLE, headers).into_response()
		}
		ByteRange::Full => {
			headers.insert(header::CONTENT_LENGTH, header_len(len));
			if head {
				return (StatusCode::OK, headers).into_response();
			}
			let file = tokio::fs::File::from_std(file);
			let body =
				axum::body::Body::from_stream(ReaderStream::with_capacity(file, STREAM_CHUNK));
			(StatusCode::OK, headers, body).into_response()
		}
		ByteRange::Part { start, end } => {
			let n = end - start + 1;
			headers.insert(header::CONTENT_LENGTH, header_len(n));
			headers.insert(
				header::CONTENT_RANGE,
				HeaderValue::from_str(&format!("bytes {start}-{end}/{len}"))
					.expect("content-range is a valid header"),
			);
			if head {
				return (StatusCode::PARTIAL_CONTENT, headers).into_response();
			}
			let mut file = tokio::fs::File::from_std(file);
			if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
				return StatusCode::INTERNAL_SERVER_ERROR.into_response();
			}
			let body = axum::body::Body::from_stream(ReaderStream::with_capacity(
				file.take(n),
				STREAM_CHUNK,
			));
			(StatusCode::PARTIAL_CONTENT, headers, body).into_response()
		}
	}
}

/// Stable v5 UUID for this machine so clients see one device across restarts.
pub fn stable_device_uuid() -> String {
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
pub fn build_folder_tree(media_dirs: &[PathBuf], media_files: &[PathBuf]) -> Vec<FolderNode> {
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
		let Some(parent_idx) = ensure_folder(&mut nodes, &mut path_to_idx, parent) else {
			continue;
		};
		nodes[parent_idx].media_indices.push(media_i);
	}

	nodes
}

/// Sort subfolders and files within each folder node by lowercase name.
pub fn sort_folder_tree(nodes: &mut [FolderNode], media_files: &[PathBuf]) {
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

/// Ensure `path` has a `FolderNode`, creating intermediate nodes up to a
/// source root already registered in `path_to_idx`. Returns `None` when the
/// path never meets a root.
fn ensure_folder(
	nodes: &mut Vec<FolderNode>,
	path_to_idx: &mut HashMap<PathBuf, usize>,
	path: &Path,
) -> Option<usize> {
	if let Some(&idx) = path_to_idx.get(path) {
		return Some(idx);
	}

	let mut pending = Vec::new();
	let mut cur = path;
	let mut parent_idx = loop {
		let parent = cur.parent()?;
		if let Some(&idx) = path_to_idx.get(parent) {
			pending.push(cur.to_path_buf());
			break idx;
		}
		pending.push(cur.to_path_buf());
		cur = parent;
	};

	for dir in pending.into_iter().rev() {
		let display_name = dir
			.file_name()
			.map(|n| n.to_string_lossy().into_owned())
			.unwrap_or_default();
		let new_idx = nodes.len();
		path_to_idx.insert(dir.clone(), new_idx);
		nodes.push(FolderNode {
			path: dir,
			display_name,
			subfolder_indices: Vec::new(),
			media_indices: Vec::new(),
		});
		nodes[parent_idx].subfolder_indices.push(new_idx);
		parent_idx = new_idx;
	}
	Some(parent_idx)
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

	#[test]
	fn build_folder_tree_links_a_deep_chain() {
		let root = PathBuf::from("/media");
		let mut file = root.clone();
		for i in 0..128 {
			file.push(format!("d{i}"));
		}
		file.push("clip.mp4");
		let nodes = build_folder_tree(&[root], &[file]);
		assert_eq!(nodes.len(), 129);
		assert_eq!(nodes[0].subfolder_indices, vec![1]);
		assert_eq!(nodes[128].display_name, "d127");
		assert!(nodes[128].subfolder_indices.is_empty());
		assert_eq!(nodes[128].media_indices, vec![0]);
	}

	#[test]
	fn fill_probes_publishes_a_wav_duration() {
		let tmp = tempfile::tempdir().unwrap();
		let path = tmp.path().join("beep.wav");
		std::fs::write(&path, tiny_wav(2)).unwrap();
		let probes = dlna::ProbeCache::empty(1);
		assert!(probes.read().unwrap().durations[0].is_empty());
		fill_probes(&[path], &probes);
		assert_eq!(probes.read().unwrap().durations[0], "0:00:02.000");
	}

	fn tiny_wav(duration_secs: u32) -> Vec<u8> {
		let sr = 8000u32;
		let data_size = sr * 2 * duration_secs;
		let mut w = Vec::new();
		w.extend(b"RIFF");
		w.extend(&(36 + data_size).to_le_bytes());
		w.extend(b"WAVEfmt ");
		w.extend(&16u32.to_le_bytes());
		w.extend(&1u16.to_le_bytes());
		w.extend(&1u16.to_le_bytes());
		w.extend(&sr.to_le_bytes());
		w.extend(&(sr * 2).to_le_bytes());
		w.extend(&2u16.to_le_bytes());
		w.extend(&16u16.to_le_bytes());
		w.extend(b"data");
		w.extend(&data_size.to_le_bytes());
		w.extend(vec![0u8; data_size as usize]);
		w
	}

	#[test]
	fn describe_media_keeps_sidecar_bits_aligned() {
		let tmp = tempfile::tempdir().unwrap();
		let first = tmp.path().join("a.mp4");
		let second = tmp.path().join("b.mp3");
		std::fs::write(&first, b"x").unwrap();
		std::fs::write(&second, b"y").unwrap();
		std::fs::write(tmp.path().join("b.srt"), b"1").unwrap();
		let records = describe_media(&[first, second]);
		assert_eq!(records.len(), 2);
		assert_eq!(records[0].subs, 0);
		assert_eq!(
			records[1].subs & spritz_core::SUBTITLE_SRT,
			spritz_core::SUBTITLE_SRT
		);
	}

	#[tokio::test]
	async fn serve_media_serves_a_percent_encoded_name() {
		use tower::ServiceExt;

		let tmp = tempfile::tempdir().unwrap();
		let path = tmp.path().join("My Movie.mp4");
		std::fs::write(&path, b"xyz").unwrap();
		let state = Arc::new(AppState {
			media_dirs: vec![tmp.path().to_path_buf()],
			media_files: vec![path],
		});
		let app = Router::new()
			.route("/m/{idx}/{*path}", get(serve_media))
			.with_state(state);
		let req = axum::http::Request::builder()
			.uri("/m/0/My%20Movie.mp4")
			.body(axum::body::Body::empty())
			.unwrap();
		let res = app.oneshot(req).await.unwrap();
		assert_eq!(res.status(), StatusCode::OK);
		let body = axum::body::to_bytes(res.into_body(), 64).await.unwrap();
		assert_eq!(&body[..], b"xyz");
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn serve_media_accepts_a_non_utf8_percent_encoding() {
		use std::os::unix::ffi::OsStrExt;
		use tower::ServiceExt;

		let (idx, relative) = media_request("/m/0/%FF.mp4").unwrap();
		assert_eq!(idx, 0);
		assert_eq!(
			relative.as_os_str().as_bytes(),
			&[0xff, b'.', b'm', b'p', b'4']
		);

		let state = Arc::new(AppState {
			media_dirs: vec![PathBuf::from("/tmp")],
			media_files: Vec::new(),
		});
		let app = Router::new()
			.route("/m/{idx}/{*path}", get(serve_media))
			.with_state(state);
		let req = axum::http::Request::builder()
			.uri("/m/0/%FF.mp4")
			.body(axum::body::Body::empty())
			.unwrap();
		let res = app.oneshot(req).await.unwrap();
		assert_eq!(res.status(), StatusCode::NOT_FOUND);
	}

	#[tokio::test]
	async fn serve_opened_returns_the_requested_byte_range() {
		let tmp = tempfile::tempdir().unwrap();
		let path = tmp.path().join("clip.mp4");
		std::fs::write(&path, b"0123456789").unwrap();
		let file = std::fs::File::open(&path).unwrap();
		let req = axum::http::Request::builder()
			.header("range", "bytes=2-5")
			.body(axum::body::Body::empty())
			.unwrap();
		let res = serve_opened(file, "video/mp4", req).await;
		assert_eq!(res.status(), StatusCode::PARTIAL_CONTENT);
		assert_eq!(res.headers()["content-range"], "bytes 2-5/10");
		assert_eq!(res.headers()["content-length"], "4");
		let body = axum::body::to_bytes(res.into_body(), 64).await.unwrap();
		assert_eq!(&body[..], b"2345");
	}

	#[tokio::test]
	async fn serve_http_lets_a_peer_subscribe_to_events() {
		use tokio::io::{AsyncReadExt, AsyncWriteExt};

		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let config = Arc::new(dlna::DlnaConfig {
			device_uuid: "uuid:test".into(),
			friendly_name: "Spritz".into(),
			http_port: addr.port(),
			local_ip: addr.ip(),
			http_ipv4: true,
			http_ipv6: false,
			media_dirs: vec![],
			media_files: vec![],
			media_sizes: vec![],
			media_dates: vec![],
			probes: dlna::ProbeCache::empty(0),
			media_has_art: vec![],
			media_subs: vec![],
			video_idx: vec![],
			audio_idx: vec![],
			folder_nodes: vec![],
			event_hub: Default::default(),
		});
		let app = Router::new().merge(dlna::router::<()>(config));
		let server = tokio::spawn(serve_http(listener, app));

		let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
		let req = format!(
			"SUBSCRIBE /upnp/event/contentdirectory HTTP/1.1\r\n\
			 HOST: {addr}\r\n\
			 CALLBACK: <http://127.0.0.1:1/evt>\r\n\
			 NT: upnp:event\r\n\
			 TIMEOUT: Second-300\r\n\
			 Connection: close\r\n\
			 \r\n"
		);
		stream.write_all(req.as_bytes()).await.unwrap();
		let mut res = String::new();
		stream.read_to_string(&mut res).await.unwrap();
		server.abort();
		assert!(res.starts_with("HTTP/1.1 200"), "{res}");
		assert!(res.to_ascii_lowercase().contains("sid: uuid:"), "{res}");
	}

	#[test]
	fn discover_advertised_ip_honours_a_specific_bind() {
		let bind: IpAddr = "192.0.2.7".parse().unwrap();
		assert_eq!(discover_advertised_ip(bind), bind);
		// Unspecified bind must produce *some* address, never panic.
		let _ = discover_advertised_ip("0.0.0.0".parse().unwrap());
	}
}
