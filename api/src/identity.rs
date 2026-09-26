//! `GET /.well-known/spritz`: who this server is, for Spritz clients that
//! found it without SSDP (Bonjour, iCloud, subnet sweep, a typed address).

use axum::{Json, Router, routing::get};
use serde::Serialize;
use std::net::Ipv4Addr;
use std::sync::Arc;

pub const PROTOCOL_VERSION: u32 = 1;
pub const WELL_KNOWN_PATH: &str = "/.well-known/spritz";
pub const DESCRIPTION_PATH: &str = "/upnp/description.xml";

/// The server's LAN IPv4 addresses, advertised first, read per request so a
/// new interface shows up without a restart.
pub type AddressSource = Arc<dyn Fn() -> Vec<Ipv4Addr> + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
pub struct ServerIdentity {
	pub protocol: u32,
	/// `"spritz"` (CLI) or `"spritz-server"` (macOS app).
	pub server: &'static str,
	pub version: String,
	pub name: String,
	/// The `UPnP` UDN, `uuid:` prefix included.
	pub uuid: String,
	pub port: u16,
	pub description: &'static str,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub hostname: Option<String>,
}

impl ServerIdentity {
	#[must_use]
	pub fn new(
		server: &'static str,
		version: &str,
		name: &str,
		device_uuid: &str,
		port: u16,
		hostname: Option<String>,
	) -> Self {
		let uuid = if device_uuid.starts_with("uuid:") {
			device_uuid.to_string()
		} else {
			format!("uuid:{device_uuid}")
		};
		Self {
			protocol: PROTOCOL_VERSION,
			server,
			version: version.to_string(),
			name: name.to_string(),
			uuid,
			port,
			description: DESCRIPTION_PATH,
			hostname,
		}
	}
}

#[derive(Serialize)]
struct IdentityJson<'a> {
	#[serde(flatten)]
	identity: &'a ServerIdentity,
	addresses: Vec<String>,
}

/// The identity route. `get` also answers `HEAD`.
pub fn router<S: Clone + Send + Sync + 'static>(
	identity: Arc<ServerIdentity>,
	addresses: AddressSource,
) -> Router<S> {
	Router::new().route(
		WELL_KNOWN_PATH,
		get(move || {
			let identity = Arc::clone(&identity);
			let addresses = Arc::clone(&addresses);
			async move {
				let body = IdentityJson {
					identity: &identity,
					addresses: addresses().iter().map(ToString::to_string).collect(),
				};
				Json(serde_json::to_value(&body).unwrap_or_default())
			}
		}),
	)
}

/// `<first label>.local`, the name mDNS responders publish for this host.
#[must_use]
pub fn mdns_hostname(system: &str) -> Option<String> {
	let first = system.trim().split('.').next().unwrap_or("").trim();
	(!first.is_empty()).then(|| format!("{first}.local"))
}

/// This machine's `.local` name, or `None` when it cannot be determined.
#[must_use]
pub fn local_hostname() -> Option<String> {
	#[cfg(target_os = "macos")]
	if let Some(name) = macos::local_host_name() {
		return mdns_hostname(&name);
	}
	let system = hostname::get().ok()?.into_string().ok()?;
	mdns_hostname(&system)
}

#[cfg(target_os = "macos")]
mod macos {
	use std::ffi::{CStr, c_char, c_void};

	type CfStringRef = *const c_void;
	const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

	#[link(name = "SystemConfiguration", kind = "framework")]
	unsafe extern "C" {
		fn SCDynamicStoreCopyLocalHostName(store: *const c_void) -> CfStringRef;
	}

	#[link(name = "CoreFoundation", kind = "framework")]
	unsafe extern "C" {
		fn CFStringGetCString(s: CfStringRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
		fn CFRelease(cf: *const c_void);
	}

	/// The Bonjour name from System Settings › General › Sharing, without `.local`.
	pub fn local_host_name() -> Option<String> {
		// SAFETY: a NULL store is allowed; the returned string is owned by us
		// and released below; the buffer outlives the call.
		unsafe {
			let s = SCDynamicStoreCopyLocalHostName(std::ptr::null());
			if s.is_null() {
				return None;
			}
			let mut buf = [0 as c_char; 256];
			let ok = CFStringGetCString(s, buf.as_mut_ptr(), 256, CF_STRING_ENCODING_UTF8) != 0;
			CFRelease(s);
			let name = CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned();
			(ok && !name.is_empty()).then_some(name)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::body::Body;
	use axum::http::{Request, StatusCode};
	use tower::ServiceExt;

	fn sample() -> ServerIdentity {
		ServerIdentity::new(
			"spritz",
			"0.1.9",
			"Daniel's \"Mac\" 🍹",
			"6f1c",
			8080,
			Some("Mini.local".into()),
		)
	}

	fn addrs(list: &'static [&'static str]) -> AddressSource {
		Arc::new(move || list.iter().map(|a| a.parse().unwrap()).collect())
	}

	async fn fetch(app: axum::Router) -> serde_json::Value {
		let res = app
			.oneshot(Request::get(WELL_KNOWN_PATH).body(Body::empty()).unwrap())
			.await
			.unwrap();
		assert_eq!(res.status(), StatusCode::OK);
		assert_eq!(res.headers()["content-type"], "application/json");
		let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
		serde_json::from_slice(&body).unwrap()
	}

	#[test]
	fn udn_gets_the_uuid_prefix_once() {
		assert_eq!(sample().uuid, "uuid:6f1c");
		let already = ServerIdentity::new("spritz", "1", "n", "uuid:6f1c", 1, None);
		assert_eq!(already.uuid, "uuid:6f1c");
	}

	#[tokio::test]
	async fn serves_protocol_v1_json() {
		let v = fetch(router(
			Arc::new(sample()),
			addrs(&["192.168.1.23", "10.0.0.5"]),
		))
		.await;
		assert_eq!(v["protocol"], 1);
		assert_eq!(v["server"], "spritz");
		assert_eq!(v["version"], "0.1.9");
		assert_eq!(v["name"], "Daniel's \"Mac\" 🍹");
		assert_eq!(v["uuid"], "uuid:6f1c");
		assert_eq!(v["port"], 8080);
		assert_eq!(v["description"], "/upnp/description.xml");
		assert_eq!(v["hostname"], "Mini.local");
		assert_eq!(
			v["addresses"],
			serde_json::json!(["192.168.1.23", "10.0.0.5"])
		);
	}

	#[tokio::test]
	async fn addresses_are_read_on_every_request() {
		let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
		let counter = Arc::clone(&calls);
		let source: AddressSource = Arc::new(move || {
			let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
			vec![std::net::Ipv4Addr::new(
				192,
				168,
				1,
				u8::try_from(10 + n).unwrap(),
			)]
		});
		let app: axum::Router = router(Arc::new(sample()), source);
		assert_eq!(fetch(app.clone()).await["addresses"][0], "192.168.1.10");
		assert_eq!(fetch(app).await["addresses"][0], "192.168.1.11");
	}

	#[tokio::test]
	async fn no_addresses_is_an_empty_list() {
		let v = fetch(router(Arc::new(sample()), addrs(&[]))).await;
		assert_eq!(v["addresses"], serde_json::json!([]));
	}

	#[tokio::test]
	async fn omits_an_unknown_hostname() {
		let id = ServerIdentity::new("spritz", "1", "n", "u", 1, None);
		let v = fetch(router(Arc::new(id), addrs(&[]))).await;
		assert!(v.get("hostname").is_none());
	}

	#[tokio::test]
	async fn answers_head() {
		let app: axum::Router = router(Arc::new(sample()), addrs(&[]));
		let res = app
			.oneshot(Request::head(WELL_KNOWN_PATH).body(Body::empty()).unwrap())
			.await
			.unwrap();
		assert_eq!(res.status(), StatusCode::OK);
	}

	#[test]
	fn mdns_hostname_uses_the_first_label() {
		assert_eq!(mdns_hostname("mini").as_deref(), Some("mini.local"));
		assert_eq!(mdns_hostname("mini.lan").as_deref(), Some("mini.local"));
		assert_eq!(mdns_hostname("Mini.local.").as_deref(), Some("Mini.local"));
		assert_eq!(mdns_hostname("  "), None);
	}

	#[test]
	// `.local` here is a hostname suffix, not a file extension.
	#[allow(clippy::case_sensitive_file_extension_comparisons)]
	fn local_hostname_ends_in_local() {
		if let Some(h) = local_hostname() {
			assert!(h.ends_with(".local"), "{h}");
		}
	}
}
