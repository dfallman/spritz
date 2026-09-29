use axum::{
	body::Body,
	extract::{ConnectInfo, Request},
	http::{HeaderMap, StatusCode},
	response::Response,
};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::SERVER;

const MAX_SUBSCRIPTIONS: usize = 128;
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EventService {
	ContentDirectory,
	ConnectionManager,
	MediaReceiverRegistrar,
}

/// Eventing state shared by every copy of a server's `DlnaConfig`.
#[derive(Clone)]
pub struct EventHub {
	/// Every live subscription: SID to expiry.
	inner: Arc<Mutex<HashMap<String, Instant>>>,
	/// `ContentDirectory` subscribers, to notify when the library changes.
	content: Arc<Mutex<HashMap<String, ContentSubscriber>>>,
	/// `ContentDirectory` `SystemUpdateID`. Starts at 1; only a server that
	/// swaps its library bumps it (see [`EventHub::content_changed`]).
	system_update_id: Arc<AtomicU32>,
}

struct ContentSubscriber {
	callback: String,
	/// SEQ for the next NOTIFY. The initial event on SUBSCRIBE is 0.
	next_seq: u32,
}

impl Default for EventHub {
	fn default() -> Self {
		Self {
			inner: Arc::default(),
			content: Arc::default(),
			system_update_id: Arc::new(AtomicU32::new(1)),
		}
	}
}

impl EventHub {
	/// The current `ContentDirectory` `SystemUpdateID`.
	#[must_use]
	pub fn system_update_id(&self) -> u32 {
		self.system_update_id.load(Ordering::SeqCst)
	}

	/// Remember a new `ContentDirectory` subscriber, dropping any whose
	/// subscription has lapsed, so this map stays within the subscription cap
	/// even on a server that never calls `content_changed`.
	fn track_content_subscriber(&self, sid: &str, callback: String) {
		let live: HashSet<String> = self
			.inner
			.lock()
			.map(|map| map.keys().cloned().collect())
			.unwrap_or_default();
		if let Ok(mut subs) = self.content.lock() {
			subs.retain(|s, _| live.contains(s));
			subs.insert(
				sid.to_string(),
				ContentSubscriber {
					callback,
					next_seq: 1,
				},
			);
		}
	}

	/// Record that the library changed: bump `SystemUpdateID` and tell every
	/// live `ContentDirectory` subscriber, so renderers that cache Browse
	/// results refresh. The id changes before this returns. The future sends
	/// the NOTIFYs and resolves once each is delivered or has timed out; it
	/// needs a Tokio runtime.
	pub fn content_changed(&self) -> impl Future<Output = ()> + Send + 'static {
		let id = self
			.system_update_id
			.fetch_add(1, Ordering::SeqCst)
			.wrapping_add(1);
		let now = Instant::now();
		let live: HashSet<String> = self
			.inner
			.lock()
			.map(|map| {
				map.iter()
					.filter(|(_, expires)| **expires > now)
					.map(|(sid, _)| sid.clone())
					.collect()
			})
			.unwrap_or_default();
		let targets: Vec<(String, String, u32)> = self
			.content
			.lock()
			.map(|mut subs| {
				subs.retain(|sid, _| live.contains(sid));
				subs.iter_mut()
					.map(|(sid, sub)| {
						let seq = sub.next_seq;
						// SEQ wraps to 1; 0 is reserved for the initial event.
						sub.next_seq = sub.next_seq.checked_add(1).unwrap_or(1);
						(sid.clone(), sub.callback.clone(), seq)
					})
					.collect()
			})
			.unwrap_or_default();
		let body = cd_event_body(id);
		async move {
			let mut sends = tokio::task::JoinSet::new();
			for (sid, callback, seq) in targets {
				let body = body.clone();
				sends.spawn(async move {
					let _ = send_notify(&callback, &sid, seq, &body).await;
				});
			}
			while sends.join_next().await.is_some() {}
		}
	}
}

pub fn parse_callback_url(header: &str) -> Option<String> {
	let s = header.trim();
	let url = if let Some(start) = s.find('<') {
		let rest = &s[start + 1..];
		let end = rest.find('>')?;
		rest[..end].trim().to_string()
	} else {
		s.to_string()
	};
	if url.starts_with("http://") && url.len() > "http://h".len() {
		Some(url)
	} else {
		None
	}
}

pub fn parse_timeout_seconds(header: Option<&str>) -> u32 {
	let Some(h) = header else {
		return 1800;
	};
	let h = h.trim();
	if h.eq_ignore_ascii_case("Second-infinite") {
		return 1800;
	}
	let digits = h
		.strip_prefix("Second-")
		.or_else(|| h.strip_prefix("second-"))
		.unwrap_or(h);
	digits.parse::<u32>().unwrap_or(1800).clamp(30, 1800)
}

pub fn propertyset(pairs: &[(&str, &str)]) -> String {
	let mut props = String::new();
	for (name, value) in pairs {
		props.push_str(&format!(
			"<e:property><{name}>{value}</{name}></e:property>"
		));
	}
	format!(
		"<?xml version=\"1.0\"?>\
		<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">{props}</e:propertyset>"
	)
}

pub fn cd_event_body(system_update_id: u32) -> String {
	propertyset(&[("SystemUpdateID", &system_update_id.to_string())])
}

pub fn cm_event_body(source: &str) -> String {
	propertyset(&[
		("SourceProtocolInfo", source),
		("SinkProtocolInfo", ""),
		("CurrentConnectionIDs", "0"),
	])
}

pub fn mrr_event_body() -> String {
	propertyset(&[
		("AuthorizationGrantedUpdateID", "1"),
		("AuthorizationDeniedUpdateID", "1"),
		("ValidationSucceededUpdateID", "1"),
		("ValidationRevokedUpdateID", "1"),
	])
}

pub fn event_body_for(
	service: EventService,
	source_protocol_info: &str,
	system_update_id: u32,
) -> String {
	match service {
		EventService::ContentDirectory => cd_event_body(system_update_id),
		EventService::ConnectionManager => cm_event_body(source_protocol_info),
		EventService::MediaReceiverRegistrar => mrr_event_body(),
	}
}

pub async fn handle(
	req: Request,
	hub: EventHub,
	service: EventService,
	source_protocol_info: String,
) -> Response {
	let peer = req
		.extensions()
		.get::<ConnectInfo<SocketAddr>>()
		.map(|info| info.0.ip());
	match req.method().as_str() {
		"SUBSCRIBE" => subscribe(req.headers(), hub, service, source_protocol_info, peer).await,
		"UNSUBSCRIBE" => unsubscribe(req.headers(), hub),
		_ => Response::builder()
			.status(StatusCode::METHOD_NOT_ALLOWED.as_u16())
			.body(Body::empty())
			.unwrap(),
	}
}

fn precondition_failed() -> Response {
	Response::builder()
		.status(StatusCode::PRECONDITION_FAILED.as_u16())
		.header("server", SERVER)
		.body(Body::empty())
		.unwrap()
}

async fn subscribe(
	headers: &HeaderMap,
	hub: EventHub,
	service: EventService,
	source_protocol_info: String,
	peer: Option<IpAddr>,
) -> Response {
	let timeout = parse_timeout_seconds(headers.get("timeout").and_then(|v| v.to_str().ok()));
	let ttl = Duration::from_secs(u64::from(timeout));
	let sid = if let Some(existing) = headers
		.get("sid")
		.and_then(|v| v.to_str().ok())
		.filter(|s| s.starts_with("uuid:"))
	{
		let renewed = hub
			.inner
			.lock()
			.ok()
			.is_some_and(|mut map| renew_subscription(&mut map, existing, ttl, Instant::now()));
		if !renewed {
			return precondition_failed();
		}
		existing.to_string()
	} else {
		let Some(peer) = peer else {
			return precondition_failed();
		};
		let callback_header = headers.get("callback").and_then(|v| v.to_str().ok());
		let Some(callback_header) = callback_header else {
			return precondition_failed();
		};
		if !callback_targets_peer(callback_header, peer) {
			return precondition_failed();
		}
		let Some(callback) = parse_callback_url(callback_header) else {
			return precondition_failed();
		};
		let sid = format!("uuid:{}", Uuid::new_v4());
		let admitted = hub.inner.lock().ok().is_some_and(|mut map| {
			admit_subscription(&mut map, &sid, ttl, Instant::now(), MAX_SUBSCRIPTIONS)
		});
		if !admitted {
			return Response::builder()
				.status(StatusCode::SERVICE_UNAVAILABLE.as_u16())
				.header("server", SERVER)
				.body(Body::empty())
				.unwrap();
		}
		if service == EventService::ContentDirectory {
			hub.track_content_subscriber(&sid, callback.clone());
		}
		let body = event_body_for(service, &source_protocol_info, hub.system_update_id());
		let sid_notify = sid.clone();
		tokio::spawn(async move {
			let _ = send_notify(&callback, &sid_notify, 0, &body).await;
		});
		sid
	};

	Response::builder()
		.status(200)
		.header("sid", sid)
		.header("timeout", format!("Second-{timeout}"))
		.header("server", SERVER)
		.body(Body::empty())
		.unwrap()
}

/// Drop expired rows, then insert `sid` when the table is under `cap`.
pub fn admit_subscription(
	map: &mut HashMap<String, Instant>,
	sid: &str,
	ttl: Duration,
	now: Instant,
	cap: usize,
) -> bool {
	map.retain(|_, expires| *expires > now);
	if map.len() >= cap {
		return false;
	}
	map.insert(sid.to_string(), now + ttl);
	true
}

/// Extend `sid` when it is still present and unexpired.
pub fn renew_subscription(
	map: &mut HashMap<String, Instant>,
	sid: &str,
	ttl: Duration,
	now: Instant,
) -> bool {
	map.retain(|_, expires| *expires > now);
	match map.get_mut(sid) {
		Some(expires) => {
			*expires = now + ttl;
			true
		}
		None => false,
	}
}

/// True when the callback URL is HTTP and its host is `peer`.
/// Hostnames are rejected so the server does not resolve attacker DNS.
pub fn callback_targets_peer(callback_header: &str, peer: IpAddr) -> bool {
	let Some(url) = parse_callback_url(callback_header) else {
		return false;
	};
	let Some(target) = parse_callback_target(&url) else {
		return false;
	};
	let Ok(ip) = target.host.parse::<IpAddr>() else {
		return false;
	};
	same_ip(ip, peer)
}

fn same_ip(a: IpAddr, b: IpAddr) -> bool {
	normalize_ip(a) == normalize_ip(b)
}

fn normalize_ip(ip: IpAddr) -> IpAddr {
	match ip {
		IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v)),
		other => other,
	}
}

fn unsubscribe(headers: &HeaderMap, hub: EventHub) -> Response {
	if let Some(sid) = headers.get("sid").and_then(|v| v.to_str().ok()) {
		if let Ok(mut map) = hub.inner.lock() {
			map.remove(sid);
		}
		if let Ok(mut subs) = hub.content.lock() {
			subs.remove(sid);
		}
	}
	Response::builder()
		.status(200)
		.header("server", SERVER)
		.body(Body::empty())
		.unwrap()
}

struct CallbackTarget {
	host: String,
	port: u16,
	hostport: String,
	path: String,
}

fn parse_callback_target(url: &str) -> Option<CallbackTarget> {
	let rest = url.trim().strip_prefix("http://")?;
	let (hostport, path) = match rest.split_once('/') {
		Some((h, p)) => (h.to_string(), format!("/{p}")),
		None => (rest.to_string(), "/".to_string()),
	};
	let (host, port) = if hostport.starts_with('[') {
		let end = hostport.find(']')?;
		let host = hostport[1..end].to_string();
		let port = hostport
			.get(end + 1..)
			.and_then(|s| s.strip_prefix(':'))
			.and_then(|p| p.parse().ok())
			.unwrap_or(80);
		(host, port)
	} else {
		match hostport.split_once(':') {
			Some((h, p)) => (h.to_string(), p.parse().unwrap_or(80)),
			None => (hostport.clone(), 80),
		}
	};
	if host.is_empty() {
		return None;
	}
	Some(CallbackTarget {
		host,
		port,
		hostport,
		path,
	})
}

pub async fn send_notify(callback: &str, sid: &str, seq: u32, body: &str) -> std::io::Result<()> {
	let target = parse_callback_target(callback).ok_or_else(|| {
		std::io::Error::new(std::io::ErrorKind::InvalidInput, "callback must be http")
	})?;
	// Only dial a literal IP. DNS here would undo the peer check in subscribe.
	if target.host.parse::<IpAddr>().is_err() {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidInput,
			"callback host must be an ip address",
		));
	}

	let path = &target.path;
	let hostport = &target.hostport;
	let len = body.len();
	let req = format!(
		"NOTIFY {path} HTTP/1.1\r\n\
		 HOST: {hostport}\r\n\
		 CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
		 NT: upnp:event\r\n\
		 NTS: upnp:propchange\r\n\
		 SID: {sid}\r\n\
		 SEQ: {seq}\r\n\
		 CONTENT-LENGTH: {len}\r\n\
		 CONNECTION: close\r\n\
		 \r\n\
		 {body}"
	);

	let connect = tokio::net::TcpStream::connect((target.host.as_str(), target.port));
	let mut stream = tokio::time::timeout(NOTIFY_TIMEOUT, connect)
		.await
		.map_err(|e| std::io::Error::new(std::io::ErrorKind::TimedOut, e))??;
	use tokio::io::AsyncWriteExt;
	tokio::time::timeout(NOTIFY_TIMEOUT, stream.write_all(req.as_bytes()))
		.await
		.map_err(|e| std::io::Error::new(std::io::ErrorKind::TimedOut, e))??;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parse_callback_url_reads_bracketed_url() {
		assert_eq!(
			parse_callback_url("<http://192.168.1.9:54000/evt>").as_deref(),
			Some("http://192.168.1.9:54000/evt")
		);
		assert_eq!(
			parse_callback_url(" <http://10.0.0.5:1/> ").as_deref(),
			Some("http://10.0.0.5:1/")
		);
		assert_eq!(parse_callback_url("https://evil"), None);
		assert_eq!(parse_callback_url(""), None);
	}

	#[test]
	fn parse_timeout_seconds_clamps() {
		assert_eq!(parse_timeout_seconds(None), 1800);
		assert_eq!(parse_timeout_seconds(Some("Second-300")), 300);
		assert_eq!(parse_timeout_seconds(Some("Second-infinite")), 1800);
		assert_eq!(parse_timeout_seconds(Some("Second-5")), 30);
		assert_eq!(parse_timeout_seconds(Some("Second-99999")), 1800);
	}

	#[test]
	fn propertyset_wraps_state_variables() {
		let xml = cd_event_body(7);
		assert!(xml.contains("<SystemUpdateID>7</SystemUpdateID>"));
		assert!(xml.contains("urn:schemas-upnp-org:event-1-0"));
	}

	#[tokio::test]
	async fn send_notify_posts_to_callback() {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		let server = tokio::spawn(async move {
			let (mut sock, _) = listener.accept().await.unwrap();
			use tokio::io::AsyncReadExt;
			let mut buf = vec![0u8; 2048];
			let n = sock.read(&mut buf).await.unwrap();
			String::from_utf8_lossy(&buf[..n]).into_owned()
		});
		send_notify(
			&format!("http://{addr}/n"),
			"uuid:abc",
			0,
			"<e:propertyset/>",
		)
		.await
		.unwrap();
		let req = tokio::time::timeout(std::time::Duration::from_secs(2), server)
			.await
			.unwrap()
			.unwrap();
		assert!(req.starts_with("NOTIFY /n HTTP/1.1"), "{req}");
		assert!(req.contains("SID: uuid:abc"), "{req}");
		assert!(req.contains("SEQ: 0"), "{req}");
		assert!(req.contains("NTS: upnp:propchange"), "{req}");
	}

	async fn next_request(listener: &tokio::net::TcpListener) -> String {
		use tokio::io::AsyncReadExt;
		let (mut sock, _) = tokio::time::timeout(Duration::from_secs(2), listener.accept())
			.await
			.expect("no NOTIFY within 2 s")
			.unwrap();
		let mut req = String::new();
		sock.read_to_string(&mut req).await.unwrap();
		req
	}

	async fn subscribe_from_localhost(
		hub: &EventHub,
		service: EventService,
		listener: &tokio::net::TcpListener,
	) -> String {
		let mut headers = HeaderMap::new();
		let callback = format!("<http://{}/evt>", listener.local_addr().unwrap());
		headers.insert("callback", callback.parse().unwrap());
		let peer = "127.0.0.1".parse().ok();
		let res = subscribe(&headers, hub.clone(), service, String::new(), peer).await;
		assert_eq!(res.status(), 200);
		res.headers()["sid"].to_str().unwrap().to_string()
	}

	#[tokio::test]
	async fn content_changed_bumps_the_id_and_notifies_content_directory_subscribers() {
		let cd = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let cm = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let hub = EventHub::default();
		assert_eq!(hub.system_update_id(), 1);

		subscribe_from_localhost(&hub, EventService::ContentDirectory, &cd).await;
		subscribe_from_localhost(&hub, EventService::ConnectionManager, &cm).await;
		assert!(
			next_request(&cd)
				.await
				.contains("<SystemUpdateID>1</SystemUpdateID>")
		);
		assert!(next_request(&cm).await.contains("SEQ: 0"));

		hub.content_changed().await;
		assert_eq!(hub.system_update_id(), 2);
		let req = next_request(&cd).await;
		assert!(req.contains("SEQ: 1"), "{req}");
		assert!(req.contains("<SystemUpdateID>2</SystemUpdateID>"), "{req}");
		assert!(
			tokio::time::timeout(Duration::from_millis(200), cm.accept())
				.await
				.is_err(),
			"only ContentDirectory subscribers hear about library changes"
		);

		hub.content_changed().await;
		let req = next_request(&cd).await;
		assert!(req.contains("SEQ: 2"), "{req}");
		assert!(req.contains("<SystemUpdateID>3</SystemUpdateID>"), "{req}");
	}

	#[tokio::test]
	async fn expired_content_subscribers_are_pruned_on_subscribe() {
		let cd = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let hub = EventHub::default();
		let first = subscribe_from_localhost(&hub, EventService::ContentDirectory, &cd).await;
		// Expire the first subscription, as time passing would.
		hub.inner.lock().unwrap().remove(&first);

		subscribe_from_localhost(&hub, EventService::ContentDirectory, &cd).await;
		assert_eq!(hub.content.lock().unwrap().len(), 1);
	}

	#[tokio::test]
	async fn unsubscribed_clients_hear_nothing_more() {
		let cd = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let hub = EventHub::default();
		let sid = subscribe_from_localhost(&hub, EventService::ContentDirectory, &cd).await;
		next_request(&cd).await;

		let mut headers = HeaderMap::new();
		headers.insert("sid", sid.parse().unwrap());
		assert_eq!(unsubscribe(&headers, hub.clone()).status(), 200);

		hub.content_changed().await;
		assert!(
			tokio::time::timeout(Duration::from_millis(200), cd.accept())
				.await
				.is_err()
		);
	}

	#[test]
	fn callback_must_target_the_peer_ip() {
		let peer: IpAddr = "192.168.1.9".parse().unwrap();
		assert!(callback_targets_peer(
			"<http://192.168.1.9:54000/evt>",
			peer
		));
		assert!(!callback_targets_peer("<http://127.0.0.1:80/>", peer));
		assert!(!callback_targets_peer("<http://evil.example/x>", peer));
		assert!(!callback_targets_peer("<https://192.168.1.9/>", peer));
		let mapped: IpAddr = "::ffff:192.168.1.9".parse().unwrap();
		assert!(callback_targets_peer("<http://192.168.1.9:1/>", mapped));
	}

	#[test]
	fn subscriptions_expire_and_honor_the_cap() {
		let now = Instant::now();
		let mut map = HashMap::new();
		assert!(admit_subscription(
			&mut map,
			"uuid:a",
			Duration::from_secs(30),
			now,
			1
		));
		assert!(!admit_subscription(
			&mut map,
			"uuid:b",
			Duration::from_secs(30),
			now,
			1
		));
		assert!(renew_subscription(
			&mut map,
			"uuid:a",
			Duration::from_secs(30),
			now
		));
		assert!(!renew_subscription(
			&mut map,
			"uuid:missing",
			Duration::from_secs(30),
			now
		));

		let mut expired = HashMap::new();
		assert!(admit_subscription(
			&mut expired,
			"uuid:a",
			Duration::from_secs(1),
			now,
			1
		));
		let later = now + Duration::from_secs(2);
		assert!(admit_subscription(
			&mut expired,
			"uuid:b",
			Duration::from_secs(30),
			later,
			1
		));
		assert!(!expired.contains_key("uuid:a"));
		assert!(expired.contains_key("uuid:b"));
	}
}
