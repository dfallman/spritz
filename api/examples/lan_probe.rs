//! Manual check for spike S2. Prints the interfaces, then the raw UDP and
//! TCP results of probing the gateway (and an optional extra target).
//!
//! `cargo run -p api --example lan_probe`                 gateway only
//! `cargo run -p api --example lan_probe -- 192.168.1.253` also a host that does not exist

fn describe(r: &std::io::Result<()>) -> String {
	match r {
		Ok(()) => "ok".into(),
		Err(e) => format!("{:?} (errno {:?}): {e}", e.kind(), e.raw_os_error()),
	}
}

fn show(label: &str, from: std::net::Ipv4Addr, target: std::net::Ipv4Addr) {
	let r = api::netcheck::probe_host(from, target, true);
	println!("{label} {target}: udp {}", describe(&r.udp));
	if let Some(tcp) = &r.tcp {
		println!("{label} {target}: tcp {}", describe(tcp));
	}
	println!(
		"{label} {target}: classify -> {:?}",
		api::netcheck::classify(&r)
	);
}

fn main() {
	let ifaces = api::netcheck::interfaces();
	for i in &ifaces {
		println!(
			"{} {} / {} -> target {:?}",
			i.name,
			i.ip,
			i.netmask,
			api::netcheck::probe_target(i)
		);
	}
	let lan = ifaces.iter().find(|i| {
		!i.ip.is_loopback() && !i.ip.is_link_local() && !api::netcheck::is_tunnel(&i.name)
	});
	let Some(lan) = lan else {
		println!("no LAN interface");
		return;
	};
	if let Some(gateway) = api::netcheck::probe_target(lan) {
		show("gateway", lan.ip, gateway);
	}
	if let Some(extra) = std::env::args().nth(1).and_then(|a| a.parse().ok()) {
		// Send twice: the first UDP send to an unresolved neighbour is often
		// accepted and only later ones fail once ARP gives up.
		show("absent (1)", lan.ip, extra);
		std::thread::sleep(std::time::Duration::from_secs(3));
		show("absent (2)", lan.ip, extra);
	}
	let input = api::netcheck::gather(std::net::IpAddr::V4(lan.ip), true, true);
	println!("lan_probe: {:?}", input.lan);
	for c in api::netcheck::evaluate(&input) {
		println!("{:?} {}: {}", c.severity, c.id, c.message);
	}
}
