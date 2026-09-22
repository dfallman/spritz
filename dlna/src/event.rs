use axum::{
	body::Body,
	extract::{ConnectInfo, Request},
	http::{HeaderMap, StatusCode},
	response::Response,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
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

#[derive(Clone, Default)]
pub struct EventHub {
	inner: Arc<Mutex<HashMap<String, Instant>>>,
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

pub fn cd_event_body() -> String {
	propertyset(&[("SystemUpdateID", "1")])
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

pub fn event_body_for(service: EventService, source_protocol_info: &str) -> String {
	match service {
		EventService::ContentDirectory => cd_event_body(),
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
		let body = event_body_for(service, &source_protocol_info);
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
	if let Some(sid) = headers.get("sid").and_then(|v| v.to_str().ok())
		&& let Ok(mut map) = hub.inner.lock()
	{
		map.remove(sid);
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
		let xml = cd_event_body();
		assert!(xml.contains("<SystemUpdateID>1</SystemUpdateID>"));
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
