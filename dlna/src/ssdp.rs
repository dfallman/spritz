use crate::DlnaConfig;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

const MULTICAST: &str = "239.255.255.250:1900";

pub async fn run(config: Arc<DlnaConfig>) -> anyhow::Result<()> {
	let local_ipv4 = match config.local_ip {
		IpAddr::V4(ip) => ip,
		IpAddr::V6(_) => {
			eprintln!("SSDP: IPv6 local address not supported — DLNA discovery unavailable");
			return Ok(());
		}
	};

	let socket = match create_socket(local_ipv4) {
		Ok(s) => s,
		Err(e) => {
			eprintln!(
				"SSDP: could not bind port 1900 ({e}) — DLNA discovery unavailable, HTTP endpoints still active"
			);
			return Ok(());
		}
	};

	announce_alive(&socket, &config).await;
	println!("SSDP: listening on 239.255.255.250:1900");

	let mut buf = vec![0u8; 2048];
	// Re-announce every 3 minutes. UPnP lets us go up to max-age/2 (~15min)
	// but on WiFi a single datagram can be dropped; re-advertising often
	// gives flaky clients (Apple TV / tvOS) more chances to hear us.
	let mut interval = tokio::time::interval(Duration::from_secs(180));
	interval.tick().await; // discard the immediate first tick

	let ctrl_c = tokio::signal::ctrl_c();
	tokio::pin!(ctrl_c);

	loop {
		tokio::select! {
			_ = &mut ctrl_c => {
				announce_byebye(&socket, &config).await;
				println!("\nSSDP: sent ssdp:byebye, shutting down");
				std::process::exit(0);
			}
			_ = interval.tick() => {
				announce_alive(&socket, &config).await;
			}
			result = socket.recv_from(&mut buf) => {
				if let Ok((len, src)) = result {
					let msg = std::str::from_utf8(&buf[..len]).unwrap_or("");
					if msg.starts_with("M-SEARCH") {
						respond_to_msearch(msg, src, &socket, &config).await;
					}
				}
			}
		}
	}
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

	// SAFETY: into_raw_fd/socket consumes the socket2::Socket, transferring
	// fd ownership. from_raw_fd/socket immediately takes that ownership.
	let std_socket: std::net::UdpSocket = unsafe {
		#[cfg(unix)]
		{
			use std::os::unix::io::{FromRawFd, IntoRawFd};
			std::net::UdpSocket::from_raw_fd(socket.into_raw_fd())
		}
		#[cfg(windows)]
		{
			use std::os::windows::io::{FromRawSocket, IntoRawSocket};
			std::net::UdpSocket::from_raw_socket(socket.into_raw_socket())
		}
	};

	Ok(UdpSocket::from_std(std_socket)?)
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

/// (NT, USN) pairs for all five notification types this device advertises.
fn nt_usn_pairs(uuid: &str) -> [(String, String); 5] {
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
	]
}

async fn announce_alive(socket: &UdpSocket, config: &DlnaConfig) {
	let multicast: SocketAddr = MULTICAST.parse().unwrap();
	let location = format!(
		"http://{}:{}/upnp/description.xml",
		config.local_ip, config.http_port
	);

	for (nt, usn) in nt_usn_pairs(&config.device_uuid) {
		let msg = format!(
			"NOTIFY * HTTP/1.1\r\n\
			 HOST: 239.255.255.250:1900\r\n\
			 CACHE-CONTROL: max-age=1800\r\n\
			 LOCATION: {location}\r\n\
			 NT: {nt}\r\n\
			 NTS: ssdp:alive\r\n\
			 SERVER: Linux/5.0 UPnP/1.0 Spritz/0.1\r\n\
			 USN: {usn}\r\n\
			 \r\n"
		);
		// Send each notification three times to survive packet loss
		for _ in 0..3 {
			let _ = socket.send_to(msg.as_bytes(), multicast).await;
			tokio::time::sleep(Duration::from_millis(200)).await;
		}
	}
}

async fn announce_byebye(socket: &UdpSocket, config: &DlnaConfig) {
	let multicast: SocketAddr = MULTICAST.parse().unwrap();

	for (nt, usn) in nt_usn_pairs(&config.device_uuid) {
		let msg = format!(
			"NOTIFY * HTTP/1.1\r\n\
			 HOST: 239.255.255.250:1900\r\n\
			 NT: {nt}\r\n\
			 NTS: ssdp:byebye\r\n\
			 USN: {usn}\r\n\
			 \r\n"
		);
		let _ = socket.send_to(msg.as_bytes(), multicast).await;
	}
}

async fn respond_to_msearch(msg: &str, src: SocketAddr, socket: &UdpSocket, config: &DlnaConfig) {
	let st = header_value(msg, "ST").unwrap_or_default();

	// UPnP 1.0 §1.2.3: client sends MX (1-5s) and the server must wait a
	// uniformly-random interval in [0, MX] before responding, so multiple
	// servers on a LAN don't flood the client's UDP buffer all at once.
	// Responding immediately can cause strict clients to drop some
	// responses, presenting as flaky discovery.
	let mx: u64 = header_value(msg, "MX")
		.and_then(|s| s.trim().parse().ok())
		.unwrap_or(0)
		.clamp(0, 5);
	if mx > 0 {
		let seed = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_nanos() as u64)
			.unwrap_or(0);
		let delay_ms = seed % (mx * 1000);
		tokio::time::sleep(Duration::from_millis(delay_ms)).await;
	}

	let location = format!(
		"http://{}:{}/upnp/description.xml",
		config.local_ip, config.http_port
	);
	let date = httpdate::fmt_http_date(std::time::SystemTime::now());

	for (nt, usn) in nt_usn_pairs(&config.device_uuid) {
		if st != "ssdp:all" && st != nt {
			continue;
		}
		let response = format!(
			"HTTP/1.1 200 OK\r\n\
			 CACHE-CONTROL: max-age=1800\r\n\
			 DATE: {date}\r\n\
			 EXT:\r\n\
			 LOCATION: {location}\r\n\
			 SERVER: Linux/5.0 UPnP/1.0 Spritz/0.1\r\n\
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
