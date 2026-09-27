//! Records each HTTP request's stage in the client tracker.
//!
//! A client that plays the M3U playlist streams from `/m/*` without ever
//! browsing; it is marked `streamed` without `browsed`, and the app's green
//! dot ("browsed or streamed") covers it on purpose.

use crate::identity::WELL_KNOWN_PATH;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use dlna::clients::{ClientTracker, Stage};
use std::net::SocketAddr;

/// Optional user-visible device name sent by Spritz clients.
pub const DEVICE_NAME_HEADER: &str = "x-spritz-device-name";

#[must_use]
pub fn stage_for_path(path: &str) -> Option<Stage> {
	if path == WELL_KNOWN_PATH
		|| path == "/upnp/description.xml"
		|| path == "/spritz"
		|| path.starts_with("/upnp/service/")
	{
		Some(Stage::Found)
	} else if path.starts_with("/upnp/control/") || path.starts_with("/upnp/event/") {
		Some(Stage::Browsed)
	} else if path.starts_with("/m/") || path.starts_with("/art/") {
		Some(Stage::Streamed)
	} else {
		None
	}
}

/// `middleware::from_fn_with_state(tracker, track_clients)`.
///
/// Needs the app served with `ConnectInfo<SocketAddr>` (see `serve_http`);
/// without it nothing is recorded. The tracker sanitises every string it
/// keeps.
pub async fn track_clients(
	State(tracker): State<ClientTracker>,
	req: Request,
	next: Next,
) -> Response {
	let peer = req
		.extensions()
		.get::<ConnectInfo<SocketAddr>>()
		.map(|c| c.0);
	if let (Some(stage), Some(peer)) = (stage_for_path(req.uri().path()), peer) {
		// `HeaderValue::to_str` rejects anything but visible ASCII, which would
		// drop a name like "Vardagsrum Å"; decode as UTF-8 and let the tracker
		// sanitise.
		fn text(v: &axum::http::HeaderValue) -> Option<&str> {
			std::str::from_utf8(v.as_bytes()).ok()
		}
		let agent = req.headers().get(header::USER_AGENT).and_then(text);
		let name = req.headers().get(DEVICE_NAME_HEADER).and_then(text);
		tracker.record(peer.ip(), stage, agent, name);
	}
	next.run(req).await
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::body::Body;
	use axum::extract::ConnectInfo;
	use axum::http::Request;
	use axum::{Router, middleware, routing::get};
	use std::net::SocketAddr;
	use tower::ServiceExt;

	#[test]
	fn maps_paths_to_stages() {
		assert_eq!(stage_for_path("/.well-known/spritz"), Some(Stage::Found));
		assert_eq!(stage_for_path("/upnp/description.xml"), Some(Stage::Found));
		assert_eq!(
			stage_for_path("/upnp/service/contentdirectory.xml"),
			Some(Stage::Found)
		);
		assert_eq!(stage_for_path("/spritz"), Some(Stage::Found));
		assert_eq!(
			stage_for_path("/upnp/control/contentdirectory"),
			Some(Stage::Browsed)
		);
		assert_eq!(
			stage_for_path("/upnp/event/contentdirectory"),
			Some(Stage::Browsed)
		);
		assert_eq!(stage_for_path("/m/0/a.mp4"), Some(Stage::Streamed));
		assert_eq!(stage_for_path("/art/3"), Some(Stage::Streamed));
		assert_eq!(stage_for_path("/health"), None);
		assert_eq!(stage_for_path("/upnp/icon.png"), None);
		assert_eq!(stage_for_path("/spritzy"), None);
	}

	fn app(tracker: ClientTracker) -> Router {
		Router::new()
			.route("/upnp/description.xml", get(|| async { "x" }))
			.route("/m/0/a.mp4", get(|| async { "v" }))
			.route("/health", get(|| async { "ok" }))
			.layer(middleware::from_fn_with_state(tracker, track_clients))
	}

	fn request(path: &str, peer: &str, ua: Option<&str>, name: Option<&str>) -> Request<Body> {
		let mut b = Request::get(path);
		if let Some(ua) = ua {
			b = b.header("user-agent", ua);
		}
		if let Some(name) = name {
			b = b.header(DEVICE_NAME_HEADER, name);
		}
		let mut req = b.body(Body::empty()).unwrap();
		let peer: SocketAddr = peer.parse().unwrap();
		req.extensions_mut().insert(ConnectInfo(peer));
		req
	}

	#[tokio::test]
	async fn records_the_stage_agent_and_name() {
		let tracker = ClientTracker::default();
		let res = app(tracker.clone())
			.oneshot(request(
				"/upnp/description.xml",
				"192.168.1.40:5000",
				Some("SpritzPlayer/1.2 (tvOS 26.0; Apple TV)"),
				Some("Living Room"),
			))
			.await
			.unwrap();
		assert_eq!(res.status(), 200);
		let list = tracker.snapshot();
		assert_eq!(list.len(), 1);
		assert!(list[0].found.is_some());
		assert_eq!(list[0].label(), "Living Room (tvOS 26.0)");
	}

	#[tokio::test]
	async fn a_playlist_client_streams_without_browsing() {
		let tracker = ClientTracker::default();
		app(tracker.clone())
			.oneshot(request(
				"/m/0/a.mp4",
				"192.168.1.60:5000",
				Some("VLC/3.0.20 LibVLC/3.0.20"),
				None,
			))
			.await
			.unwrap();
		let r = &tracker.snapshot()[0];
		assert!(r.streamed.is_some());
		assert!(r.browsed.is_none());
	}

	#[tokio::test]
	async fn a_utf8_device_name_is_kept_and_cleaned() {
		// HTTP header values cannot carry C0 controls such as ESC (hyper
		// rejects the request), but UTF-8 bytes can smuggle the C1 CSI and
		// bidi overrides. Those go; the non-ASCII letters stay.
		let tracker = ClientTracker::default();
		let mut req = request(
			"/upnp/description.xml",
			"192.168.1.40:5000",
			Some("SpritzPlayer/1.2 (tvOS 26.0; Apple TV)"),
			None,
		);
		req.headers_mut().insert(
			DEVICE_NAME_HEADER,
			axum::http::HeaderValue::from_bytes("\u{9b}2J\u{202e}Vardagsrum Å".as_bytes()).unwrap(),
		);
		app(tracker.clone()).oneshot(req).await.unwrap();
		assert_eq!(tracker.snapshot()[0].device_name, "2JVardagsrum Å");
	}

	#[tokio::test]
	async fn ignores_untracked_paths_and_missing_peers() {
		let tracker = ClientTracker::default();
		app(tracker.clone())
			.oneshot(request("/health", "192.168.1.40:5000", None, None))
			.await
			.unwrap();
		let no_peer = Request::get("/upnp/description.xml")
			.body(Body::empty())
			.unwrap();
		app(tracker.clone()).oneshot(no_peer).await.unwrap();
		assert!(tracker.snapshot().is_empty());
	}

	#[tokio::test]
	async fn tolerates_non_utf8_headers() {
		let tracker = ClientTracker::default();
		let mut req = request("/upnp/description.xml", "192.168.1.40:5000", None, None);
		req.headers_mut().insert(
			"user-agent",
			axum::http::HeaderValue::from_bytes(b"Spritz\xffPlayer/1").unwrap(),
		);
		app(tracker.clone()).oneshot(req).await.unwrap();
		let list = tracker.snapshot();
		assert_eq!(list.len(), 1);
		assert!(!list[0].agent.spritz);
	}
}
