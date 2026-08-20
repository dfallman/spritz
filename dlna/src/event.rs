use axum::{
	body::Body,
	extract::Request,
	http::{HeaderMap, StatusCode},
	response::Response,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::SERVER;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EventService {
	ContentDirectory,
	ConnectionManager,
	MediaReceiverRegistrar,
}

#[derive(Clone, Default)]
pub struct EventHub {
	inner: Arc<Mutex<HashMap<String, String>>>,
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
	match req.method().as_str() {
		"SUBSCRIBE" => subscribe(req.headers(), hub, service, source_protocol_info).await,
		"UNSUBSCRIBE" => unsubscribe(req.headers(), hub),
		_ => Response::builder()
			.status(StatusCode::METHOD_NOT_ALLOWED.as_u16())
			.body(Body::empty())
			.unwrap(),
	}
}

async fn subscribe(
	headers: &HeaderMap,
	hub: EventHub,
	service: EventService,
	source_protocol_info: String,
) -> Response {
	let timeout = parse_timeout_seconds(headers.get("timeout").and_then(|v| v.to_str().ok()));
	let sid = if let Some(existing) = headers
		.get("sid")
		.and_then(|v| v.to_str().ok())
		.filter(|s| s.starts_with("uuid:"))
	{
		existing.to_string()
	} else {
		let callback = headers
			.get("callback")
			.and_then(|v| v.to_str().ok())
			.and_then(parse_callback_url);
		let Some(callback) = callback else {
			return Response::builder()
				.status(StatusCode::PRECONDITION_FAILED.as_u16())
				.header("server", SERVER)
				.body(Body::empty())
				.unwrap();
		};
		let sid = format!("uuid:{}", Uuid::new_v4());
		if let Ok(mut map) = hub.inner.lock() {
			map.insert(sid.clone(), callback.clone());
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

pub async fn send_notify(callback: &str, sid: &str, seq: u32, body: &str) -> std::io::Result<()> {
	let url = callback.trim();
	let rest = url.strip_prefix("http://").ok_or_else(|| {
		std::io::Error::new(std::io::ErrorKind::InvalidInput, "callback must be http")
	})?;
	let (hostport, path) = match rest.split_once('/') {
		Some((h, p)) => (h, format!("/{p}")),
		None => (rest, "/".to_string()),
	};
	let (host, port) = if hostport.starts_with('[') {
		let end = hostport.find(']').ok_or_else(|| {
			std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad ipv6 callback")
		})?;
		let host = &hostport[1..end];
		let port = hostport
			.get(end + 1..)
			.and_then(|s| s.strip_prefix(':'))
			.and_then(|p| p.parse().ok())
			.unwrap_or(80);
		(host.to_string(), port)
	} else {
		match hostport.split_once(':') {
			Some((h, p)) => (h.to_string(), p.parse().unwrap_or(80)),
			None => (hostport.to_string(), 80),
		}
	};

	let req = format!(
		"NOTIFY {path} HTTP/1.1\r\n\
		 HOST: {hostport}\r\n\
		 CONTENT-TYPE: text/xml; charset=\"utf-8\"\r\n\
		 NT: upnp:event\r\n\
		 NTS: upnp:propchange\r\n\
		 SID: {sid}\r\n\
		 SEQ: {seq}\r\n\
		 CONTENT-LENGTH: {}\r\n\
		 CONNECTION: close\r\n\
		 \r\n\
		 {body}",
		body.len()
	);

	let mut stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
	use tokio::io::AsyncWriteExt;
	stream.write_all(req.as_bytes()).await?;
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
}
