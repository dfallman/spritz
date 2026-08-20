use crate::DlnaConfig;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

const MULTICAST_V4: &str = "239.255.255.250:1900";
const MULTICAST_V6: &str = "[FF02::C]:1900";

pub async fn run(
	config: Arc<DlnaConfig>,
	mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
	let v4_if = match config.local_ip {
		IpAddr::V4(ip) => ip,
		IpAddr::V6(_) => first_ipv4().unwrap_or(Ipv4Addr::UNSPECIFIED),
	};
	let socket_v4 = match create_socket(v4_if) {
		Ok(s) => Some(Arc::new(s)),
		Err(e) => {
			tracing::warn!("SSDP: IPv4 port 1900 unavailable ({e})");
			None
		}
	};
	let socket_v6 = match create_socket_v6() {
		Ok(s) => Some(Arc::new(s)),
		Err(e) => {
			tracing::warn!("SSDP: IPv6 port 1900 unavailable ({e})");
			None
		}
	};
	if socket_v4.is_none() && socket_v6.is_none() {
		tracing::warn!("SSDP: DLNA discovery unavailable, HTTP endpoints still active");
		return Ok(());
	}

	let mut alive_burst = Some(spawn_alive(socket_v4.clone(), socket_v6.clone(), &config));
	if socket_v4.is_some() {
		println!("SSDP: listening on 239.255.255.250:1900");
	}
	if socket_v6.is_some() {
		println!("SSDP: listening on [FF02::C]:1900");
	}

	let mut buf4 = vec![0u8; 2048];
	let mut buf6 = vec![0u8; 2048];
	let mut interval = tokio::time::interval(Duration::from_secs(180));
	interval.tick().await;

	loop {
		tokio::select! {
			_ = &mut shutdown => {
				if let Some(burst) = alive_burst.take() {
					burst.abort();
				}
				if let Some(s) = &socket_v4 {
					announce_byebye(s, &config, MULTICAST_V4).await;
				}
				if let Some(s) = &socket_v6 {
					announce_byebye(s, &config, MULTICAST_V6).await;
				}
				tracing::info!("SSDP: sent ssdp:byebye, shutting down");
				return Ok(());
			}
			_ = interval.tick() => {
				if let Some(burst) = alive_burst.take() {
					burst.abort();
				}
				alive_burst = Some(spawn_alive(socket_v4.clone(), socket_v6.clone(), &config));
			}
			result = recv_opt(&socket_v4, &mut buf4) => {
				if let Some(s) = &socket_v4 {
					handle_datagram(result, &buf4, s, &config).await;
				}
			}
			result = recv_opt(&socket_v6, &mut buf6) => {
				if let Some(s) = &socket_v6 {
					handle_datagram(result, &buf6, s, &config).await;
				}
			}
		}
	}
}

async fn recv_opt(
	socket: &Option<Arc<UdpSocket>>,
	buf: &mut [u8],
) -> std::io::Result<(usize, SocketAddr)> {
	match socket {
		Some(s) => s.recv_from(buf).await,
		None => std::future::pending().await,
	}
}

async fn handle_datagram(
	result: std::io::Result<(usize, SocketAddr)>,
	buf: &[u8],
	socket: &Arc<UdpSocket>,
	config: &Arc<DlnaConfig>,
) {
	match result {
		Ok((len, src)) => {
			let msg = std::str::from_utf8(&buf[..len]).unwrap_or("");
			if msg.starts_with("M-SEARCH") {
				respond_to_msearch(msg, src, Arc::clone(socket), Arc::clone(config));
			}
		}
		Err(e) if e.kind() == ErrorKind::Interrupted => {}
		Err(e) => {
			tracing::warn!("SSDP: recv error: {e}");
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}
}

fn spawn_alive(
	socket_v4: Option<Arc<UdpSocket>>,
	socket_v6: Option<Arc<UdpSocket>>,
	config: &Arc<DlnaConfig>,
) -> tokio::task::JoinHandle<()> {
	let config = Arc::clone(config);
	tokio::spawn(async move {
		if let Some(s) = socket_v4 {
			announce_alive(&s, &config, MULTICAST_V4).await;
		}
		if let Some(s) = socket_v6 {
			announce_alive(&s, &config, MULTICAST_V6).await;
		}
	})
}

fn create_socket(local_ip: Ipv4Addr) -> anyhow::Result<UdpSocket> {
	use socket2::{Domain, Protocol, Socket, Type};

	let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
	socket.set_reuse_address(true)?;
	socket.set_nonblocking(true)?;

	let bind_addr = socket2::SockAddr::from(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 1900u16)));
	socket.bind(&bind_addr)?;

	// Explicitly route outgoing multicast through the LAN interface.
	socket.set_multicast_if_v4(&local_ip)?;
	socket.set_multicast_loop_v4(false)?;

	// Join the multicast group on every non-loopback IPv4 interface.
	// WSL2 mirrored networking can route packets through any of several virtual
	// adapters, so joining on all of them ensures we both receive M-SEARCH
	// requests and that the kernel delivers our outgoing announcements correctly.
	let multicast_ip: Ipv4Addr = "239.255.255.250".parse()?;
	let joined = join_all_interfaces(&socket, &multicast_ip);
	if joined == 0 {
		// Fallback if interface enumeration failed
		socket.join_multicast_v4(&multicast_ip, &local_ip)?;
	}

	let std_socket: std::net::UdpSocket = socket.into();
	Ok(UdpSocket::from_std(std_socket)?)
}

fn create_socket_v6() -> anyhow::Result<UdpSocket> {
	use socket2::{Domain, Protocol, Socket, Type};

	let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
	socket.set_reuse_address(true)?;
	socket.set_only_v6(true)?;
	socket.set_nonblocking(true)?;

	let bind_addr = socket2::SockAddr::from(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 1900u16)));
	socket.bind(&bind_addr)?;
	socket.set_multicast_loop_v6(false)?;

	let multicast_ip: Ipv6Addr = "FF02::C".parse()?;
	let joined = join_all_ipv6_interfaces(&socket, &multicast_ip);
	if joined == 0 {
		socket.join_multicast_v6(&multicast_ip, 0)?;
	}

	let std_socket: std::net::UdpSocket = socket.into();
	Ok(UdpSocket::from_std(std_socket)?)
}

fn first_ipv4() -> Option<Ipv4Addr> {
	let ifaces = if_addrs::get_if_addrs().ok()?;
	for iface in ifaces {
		if iface.is_loopback() {
			continue;
		}
		if let if_addrs::IfAddr::V4(v4) = iface.addr {
			return Some(v4.ip);
		}
	}
	None
}

fn join_all_interfaces(socket: &socket2::Socket, multicast_addr: &Ipv4Addr) -> usize {
	let Ok(ifaces) = if_addrs::get_if_addrs() else {
		return 0;
	};
	let mut joined = 0;
	for iface in ifaces {
		if iface.is_loopback() {
			continue;
		}
		if let if_addrs::IfAddr::V4(v4) = iface.addr
			&& socket.join_multicast_v4(multicast_addr, &v4.ip).is_ok()
		{
			joined += 1;
		}
	}
	joined
}

fn join_all_ipv6_interfaces(socket: &socket2::Socket, multicast_addr: &Ipv6Addr) -> usize {
	let Ok(ifaces) = if_addrs::get_if_addrs() else {
		return 0;
	};
	let mut joined = 0;
	let mut seen = std::collections::BTreeSet::new();
	for iface in ifaces {
		if iface.is_loopback() {
			continue;
		}
		if let if_addrs::IfAddr::V6(_) = iface.addr {
			let idx = iface.index.unwrap_or(0);
			if !seen.insert(idx) {
				continue;
			}
			if socket.join_multicast_v6(multicast_addr, idx).is_ok() {
				joined += 1;
			}
		}
	}
	joined
}

/// (NT, USN) pairs for every notification type this device advertises.
fn nt_usn_pairs(uuid: &str) -> [(String, String); 6] {
	[
		(
			"upnp:rootdevice".into(),
			format!("uuid:{uuid}::upnp:rootdevice"),
		),
		(format!("uuid:{uuid}"), format!("uuid:{uuid}")),
		(
			"urn:schemas-upnp-org:device:MediaServer:1".into(),
			format!("uuid:{uuid}::urn:schemas-upnp-org:device:MediaServer:1"),
		),
		(
			"urn:schemas-upnp-org:service:ContentDirectory:1".into(),
			format!("uuid:{uuid}::urn:schemas-upnp-org:service:ContentDirectory:1"),
		),
		(
			"urn:schemas-upnp-org:service:ConnectionManager:1".into(),
			format!("uuid:{uuid}::urn:schemas-upnp-org:service:ConnectionManager:1"),
		),
		(
			"urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1".into(),
			format!("uuid:{uuid}::urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1"),
		),
	]
}

/// True when this M-SEARCH ST is one we advertise (or `ssdp:all`).
/// Checked *before* the MX delay so unrelated LAN searches (Chromecast,
/// IGD, DIAL, …) do not stall the SSDP receive loop.
fn st_is_relevant(st: &str, uuid: &str) -> bool {
	st == "ssdp:all" || nt_usn_pairs(uuid).iter().any(|(nt, _)| st == nt)
}

/// UPnP 1.0 §1.2.3: MX is a client-requested delay cap, 1–5 seconds.
/// Missing/garbage → 0 (respond immediately). Values above 5 are clamped.
fn parse_mx(msg: &str) -> u64 {
	header_value(msg, "MX")
		.and_then(|s| s.parse().ok())
		.unwrap_or(0)
		.clamp(0, 5)
}

async fn announce_alive(socket: &UdpSocket, config: &DlnaConfig, multicast_host: &str) {
	let multicast: SocketAddr = multicast_host.parse().unwrap();
	let ip = location_ip_for_family(multicast.is_ipv6(), config);
	let location = format!(
		"http://{}/upnp/description.xml",
		spritz_core::format_http_authority(ip, config.http_port)
	);

	for (nt, usn) in nt_usn_pairs(&config.device_uuid) {
		let server = crate::SERVER;
		let msg = format!(
			"NOTIFY * HTTP/1.1\r\n\
			 HOST: {multicast_host}\r\n\
			 CACHE-CONTROL: max-age=1800\r\n\
			 LOCATION: {location}\r\n\
			 NT: {nt}\r\n\
			 NTS: ssdp:alive\r\n\
			 SERVER: {server}\r\n\
			 USN: {usn}\r\n\
			 \r\n"
		);
		for _ in 0..3 {
			let _ = socket.send_to(msg.as_bytes(), multicast).await;
			tokio::time::sleep(Duration::from_millis(200)).await;
		}
	}
}

async fn announce_byebye(socket: &UdpSocket, config: &DlnaConfig, multicast_host: &str) {
	let multicast: SocketAddr = multicast_host.parse().unwrap();

	for round in 0..3 {
		for (nt, usn) in nt_usn_pairs(&config.device_uuid) {
			let msg = format!(
				"NOTIFY * HTTP/1.1\r\n\
				 HOST: {multicast_host}\r\n\
				 NT: {nt}\r\n\
				 NTS: ssdp:byebye\r\n\
				 USN: {usn}\r\n\
				 \r\n"
			);
			let _ = socket.send_to(msg.as_bytes(), multicast).await;
		}
		if round < 2 {
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}
}

fn respond_to_msearch(msg: &str, src: SocketAddr, socket: Arc<UdpSocket>, config: Arc<DlnaConfig>) {
	let st = header_value(msg, "ST").unwrap_or_default();
	if !st_is_relevant(&st, &config.device_uuid) {
		return;
	}

	// UPnP 1.0 §1.2.3: client sends MX (1-5s) and the server must wait a
	// uniformly-random interval in [0, MX] before responding, so multiple
	// servers on a LAN don't flood the client's UDP buffer all at once.
	// Responding immediately can cause strict clients to drop some
	// responses, presenting as flaky discovery.
	//
	// The sleep (and the 3× retry gaps) run on a spawned task so this
	// function returns immediately and the receive loop stays unblocked.
	let mx = parse_mx(msg);
	tokio::spawn(async move {
		if mx > 0 {
			let seed = std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.map(|d| d.as_nanos() as u64)
				.unwrap_or(0);
			let delay_ms = seed % (mx * 1000);
			tokio::time::sleep(Duration::from_millis(delay_ms)).await;
		}

		let location = location_for_peer(src, &config);
		let date = httpdate::fmt_http_date(std::time::SystemTime::now());

		for (nt, usn) in nt_usn_pairs(&config.device_uuid) {
			if st != "ssdp:all" && st != nt {
				continue;
			}
			let server = crate::SERVER;
			let response = format!(
				"HTTP/1.1 200 OK\r\n\
				 CACHE-CONTROL: max-age=1800\r\n\
				 DATE: {date}\r\n\
				 EXT:\r\n\
				 LOCATION: {location}\r\n\
				 SERVER: {server}\r\n\
				 ST: {nt}\r\n\
				 USN: {usn}\r\n\
				 \r\n"
			);
			// Send 3× with small gaps — single UDP datagrams on WiFi get dropped
			// often enough that tvOS clients miss the first one.
			for _ in 0..3 {
				let _ = socket.send_to(response.as_bytes(), src).await;
				tokio::time::sleep(Duration::from_millis(100)).await;
			}
		}
	});
}

fn location_for_peer(peer: SocketAddr, config: &DlnaConfig) -> String {
	let ip =
		local_ip_for_peer(peer).unwrap_or_else(|| location_ip_for_family(peer.is_ipv6(), config));
	format!(
		"http://{}/upnp/description.xml",
		spritz_core::format_http_authority(ip, config.http_port)
	)
}

fn location_ip_for_family(want_v6: bool, config: &DlnaConfig) -> IpAddr {
	if want_v6 {
		advertised_ipv6().unwrap_or(config.local_ip)
	} else if config.local_ip.is_ipv4() {
		config.local_ip
	} else {
		first_ipv4().map(IpAddr::V4).unwrap_or(config.local_ip)
	}
}

fn advertised_ipv6() -> Option<IpAddr> {
	let ifaces = if_addrs::get_if_addrs().ok()?;
	for iface in ifaces {
		if iface.is_loopback() {
			continue;
		}
		if let if_addrs::IfAddr::V6(v6) = iface.addr
			&& usable_ipv6(v6.ip)
		{
			return Some(IpAddr::V6(v6.ip));
		}
	}
	None
}

fn usable_ipv6(ip: Ipv6Addr) -> bool {
	!ip.is_loopback() && !ip.is_multicast() && !ip.is_unicast_link_local()
}

/// Prefer the local address on the same subnet as the searching client.
/// `local_ip()` is often a VPN/Docker iface; multicast still reaches the TV.
fn local_ip_for_peer(peer: SocketAddr) -> Option<IpAddr> {
	let ifaces = if_addrs::get_if_addrs().ok()?;
	match peer.ip() {
		IpAddr::V4(peer_v4) => {
			for iface in ifaces {
				if iface.is_loopback() {
					continue;
				}
				if let if_addrs::IfAddr::V4(v4) = iface.addr {
					let mask = u32::from(v4.netmask);
					if mask == 0 {
						continue;
					}
					if (u32::from(v4.ip) & mask) == (u32::from(peer_v4) & mask) {
						return Some(IpAddr::V4(v4.ip));
					}
				}
			}
		}
		IpAddr::V6(peer_v6) => {
			for iface in ifaces {
				if iface.is_loopback() {
					continue;
				}
				if let if_addrs::IfAddr::V6(v6) = iface.addr
					&& usable_ipv6(v6.ip)
					&& ipv6_same_net(v6.ip, peer_v6, v6.netmask)
				{
					return Some(IpAddr::V6(v6.ip));
				}
			}
		}
	}
	None
}

fn ipv6_same_net(a: Ipv6Addr, b: Ipv6Addr, mask: Ipv6Addr) -> bool {
	let m = u128::from(mask);
	if m == 0 {
		return false;
	}
	(u128::from(a) & m) == (u128::from(b) & m)
}

/// Case-insensitive extraction of an HTTP-over-UDP header value.
fn header_value(msg: &str, name: &str) -> Option<String> {
	let prefix = format!("{}:", name.to_ascii_lowercase());
	for line in msg.lines() {
		if line.to_ascii_lowercase().starts_with(&prefix) {
			return Some(line[prefix.len()..].trim().to_string());
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	const UUID: &str = "11111111-2222-3333-4444-555555555555";

	#[test]
	fn st_is_relevant_accepts_ssdp_all() {
		assert!(st_is_relevant("ssdp:all", UUID));
	}

	#[test]
	fn st_is_relevant_accepts_each_advertised_nt() {
		for (nt, _) in nt_usn_pairs(UUID) {
			assert!(st_is_relevant(&nt, UUID), "should accept ST={nt}");
		}
	}

	#[test]
	fn st_is_relevant_rejects_unrelated_search_targets() {
		for st in [
			"",
			"ssdp:discover",
			"urn:dial-multiscreen-org:service:dial:1",
			"urn:schemas-upnp-org:device:InternetGatewayDevice:1",
			"urn:schemas-upnp-org:device:MediaServer:2",
			"uuid:someone-else",
		] {
			assert!(!st_is_relevant(st, UUID), "should reject ST={st}");
		}
	}

	#[test]
	fn parse_mx_clamps_to_zero_through_five() {
		assert_eq!(parse_mx(""), 0);
		assert_eq!(parse_mx("M-SEARCH * HTTP/1.1\r\nMX: 3\r\n"), 3);
		assert_eq!(parse_mx("M-SEARCH * HTTP/1.1\r\nMX: 0\r\n"), 0);
		assert_eq!(parse_mx("M-SEARCH * HTTP/1.1\r\nMX: 99\r\n"), 5);
		assert_eq!(parse_mx("M-SEARCH * HTTP/1.1\r\nmx: 2\r\n"), 2);
		assert_eq!(parse_mx("M-SEARCH * HTTP/1.1\r\nMX: no\r\n"), 0);
	}

	#[test]
	fn header_value_is_case_insensitive() {
		let msg = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nST: ssdp:all\r\n";
		assert_eq!(header_value(msg, "ST").as_deref(), Some("ssdp:all"));
		assert_eq!(header_value(msg, "st").as_deref(), Some("ssdp:all"));
		assert_eq!(
			header_value(msg, "Host").as_deref(),
			Some("239.255.255.250:1900")
		);
		assert_eq!(header_value(msg, "MX"), None);
	}

	#[test]
	fn ipv6_same_net_uses_prefix_mask() {
		let a: Ipv6Addr = "2001:db8:1::1".parse().unwrap();
		let b: Ipv6Addr = "2001:db8:1::9".parse().unwrap();
		let other: Ipv6Addr = "2001:db8:2::1".parse().unwrap();
		let mask: Ipv6Addr = "ffff:ffff:ffff::".parse().unwrap();
		assert!(ipv6_same_net(a, b, mask));
		assert!(!ipv6_same_net(a, other, mask));
	}

	#[test]
	fn usable_ipv6_skips_link_local() {
		assert!(usable_ipv6("2001:db8::1".parse().unwrap()));
		assert!(!usable_ipv6("fe80::1".parse().unwrap()));
		assert!(!usable_ipv6("::1".parse().unwrap()));
	}
}
