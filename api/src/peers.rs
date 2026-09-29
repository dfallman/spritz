//! Who may talk to the server. By default only devices on the local network:
//! DLNA has no authentication, so reachability is the only access control.

use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};

/// Refused peers are logged once each; past this many the list starts over.
const LOGGED_CAP: usize = 256;

/// Local by range alone: loopback, private IPv4, `100.64.0.0/10` (Tailscale,
/// carrier-grade NAT), link-local, and IPv6 unique-local. IPv4-mapped IPv6 is
/// judged as IPv4.
#[must_use]
pub fn is_local_range(ip: IpAddr) -> bool {
	match normalize(ip) {
		IpAddr::V4(v4) => {
			let [a, b, ..] = v4.octets();
			v4.is_loopback()
				|| v4.is_private()
				|| v4.is_link_local()
				|| (a == 100 && b & 0xc0 == 64)
		}
		IpAddr::V6(v6) => {
			let first = v6.segments()[0];
			v6.is_loopback() || first & 0xffc0 == 0xfe80 || first & 0xfe00 == 0xfc00
		}
	}
}

/// On one of `nets` (interface address and netmask pairs), such as a global
/// IPv6 address on this machine's own /64.
#[must_use]
pub fn on_attached_subnet(ip: IpAddr, nets: &[(IpAddr, IpAddr)]) -> bool {
	let ip = normalize(ip);
	nets.iter()
		.any(|&(net, mask)| match (ip, normalize(net), mask) {
			(IpAddr::V4(a), IpAddr::V4(n), IpAddr::V4(m)) => {
				let m = u32::from(m);
				u32::from(a) & m == u32::from(n) & m
			}
			(IpAddr::V6(a), IpAddr::V6(n), IpAddr::V6(m)) => {
				let m = u128::from(m);
				u128::from(a) & m == u128::from(n) & m
			}
			_ => false,
		})
}

/// Address and netmask of every interface that is up.
#[must_use]
pub fn attached_subnets() -> Vec<(IpAddr, IpAddr)> {
	if_addrs::get_if_addrs()
		.unwrap_or_default()
		.into_iter()
		.filter(if_addrs::Interface::is_oper_up)
		.map(|i| match i.addr {
			if_addrs::IfAddr::V4(v4) => (IpAddr::V4(v4.ip), IpAddr::V4(v4.netmask)),
			if_addrs::IfAddr::V6(v6) => (IpAddr::V6(v6.ip), IpAddr::V6(v6.netmask)),
		})
		.collect()
}

/// This machine's addresses that are not local by range: where someone
/// outside the local network could reach it, firewalls permitting.
#[must_use]
pub fn public_addresses() -> Vec<IpAddr> {
	let mut out: Vec<IpAddr> = attached_subnets()
		.into_iter()
		.map(|(ip, _)| ip)
		.filter(|ip| !ip.is_unspecified() && !is_local_range(*ip))
		.collect();
	out.sort_unstable();
	out.dedup();
	out
}

/// A peer the server answers: local by range, or on a directly attached
/// subnet. Only public addresses cost an interface lookup.
#[must_use]
pub fn is_local_peer(ip: IpAddr) -> bool {
	is_local_range(ip) || on_attached_subnet(ip, &attached_subnets())
}

fn normalize(ip: IpAddr) -> IpAddr {
	match ip {
		IpAddr::V6(v6) => v6
			.to_ipv4_mapped()
			.map_or(IpAddr::V6(v6), |v4: Ipv4Addr| IpAddr::V4(v4)),
		v4 @ IpAddr::V4(_) => v4,
	}
}

/// State for [`local_only`]: which refused peers were already logged.
#[derive(Clone, Default)]
pub struct PeerFilter {
	logged: Arc<Mutex<HashSet<IpAddr>>>,
}

impl PeerFilter {
	fn first_refusal(&self, ip: IpAddr) -> bool {
		let mut logged = self.logged.lock().unwrap_or_else(PoisonError::into_inner);
		if logged.len() >= LOGGED_CAP {
			logged.clear();
		}
		logged.insert(ip)
	}
}

/// Middleware: `403 Forbidden` for peers outside the local network. Needs the
/// app served with `ConnectInfo<SocketAddr>`; without a peer address it
/// refuses, so a wiring mistake fails closed.
pub async fn local_only(State(filter): State<PeerFilter>, req: Request, next: Next) -> Response {
	let peer = req
		.extensions()
		.get::<ConnectInfo<SocketAddr>>()
		.map(|c| c.0.ip());
	match peer {
		Some(ip) if is_local_peer(ip) => next.run(req).await,
		Some(ip) => {
			if filter.first_refusal(ip) {
				tracing::warn!(
					"Refused {ip}: not on the local network (allow it with --allow-remote, or in Spritz Server's settings)"
				);
			}
			StatusCode::FORBIDDEN.into_response()
		}
		None => StatusCode::FORBIDDEN.into_response(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::body::Body;
	use axum::extract::ConnectInfo;
	use axum::http::{Request, StatusCode};
	use axum::routing::get;
	use axum::{Router, middleware};
	use std::net::SocketAddr;
	use tower::ServiceExt;

	fn ip(s: &str) -> IpAddr {
		s.parse().unwrap()
	}

	#[test]
	fn local_ranges() {
		for local in [
			"127.0.0.1",
			"10.1.2.3",
			"172.16.0.1",
			"172.31.255.255",
			"192.168.1.1",
			"100.64.0.1",
			"100.127.255.255",
			"169.254.1.1",
			"::1",
			"fe80::1",
			"fd12:3456::1",
			"::ffff:192.168.1.2",
		] {
			assert!(is_local_range(ip(local)), "{local} should be local");
		}
		for public in [
			"8.8.8.8",
			"172.32.0.1",
			"100.128.0.1",
			"203.0.113.5",
			"2001:db8::1",
			"::ffff:8.8.8.8",
		] {
			assert!(!is_local_range(ip(public)), "{public} should not be local");
		}
	}

	#[test]
	fn attached_subnets_cover_global_addresses_next_door() {
		let nets = [
			(ip("2001:db8:1::5"), ip("ffff:ffff:ffff:ffff::")),
			(ip("203.0.113.5"), ip("255.255.255.0")),
		];
		assert!(on_attached_subnet(ip("2001:db8:1::99"), &nets));
		assert!(!on_attached_subnet(ip("2001:db8:2::1"), &nets));
		assert!(on_attached_subnet(ip("203.0.113.77"), &nets));
		assert!(on_attached_subnet(ip("::ffff:203.0.113.77"), &nets));
		assert!(!on_attached_subnet(ip("203.0.114.1"), &nets));
	}

	async fn status_from(peer: Option<&str>) -> StatusCode {
		let app = Router::new().route("/x", get(|| async { "ok" })).layer(
			middleware::from_fn_with_state(PeerFilter::default(), local_only),
		);
		let mut req = Request::get("/x").body(Body::empty()).unwrap();
		if let Some(peer) = peer {
			req.extensions_mut()
				.insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
		}
		app.oneshot(req).await.unwrap().status()
	}

	#[tokio::test]
	async fn only_local_peers_get_through() {
		assert_eq!(status_from(Some("192.168.1.5:5000")).await, StatusCode::OK);
		assert_eq!(status_from(Some("[fd12::5]:5000")).await, StatusCode::OK);
		assert_eq!(
			status_from(Some("8.8.8.8:5000")).await,
			StatusCode::FORBIDDEN
		);
		assert_eq!(
			status_from(None).await,
			StatusCode::FORBIDDEN,
			"fail closed"
		);
	}
}
