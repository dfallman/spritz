//! Network self-checks: why players on the LAN might not see this server.

use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Cheap enough (one `getifaddrs`, one UDP send) to run often, so plugging
/// in Ethernet shows up within seconds without interface-change listeners.
pub const CHECK_PERIOD: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
	Info,
	Warning,
	Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
	/// Stable identifier, e.g. `local-network-denied`.
	pub id: &'static str,
	pub severity: Severity,
	pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Iface {
	pub name: String,
	pub ip: Ipv4Addr,
	pub netmask: Ipv4Addr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LanProbe {
	Ok,
	/// macOS refused both a UDP send and a TCP connect to the gateway:
	/// Local Network access is off.
	Denied,
	/// Not run (not macOS, or no LAN interface).
	Skipped,
}

/// Raw outcome of probing one host, kept for the S2 harness and `classify`.
#[derive(Debug)]
pub struct ProbeResult {
	pub udp: std::io::Result<()>,
	/// Only attempted when UDP failed with `HostUnreachable`, or when asked.
	pub tcp: Option<std::io::Result<()>>,
}

#[derive(Clone, Debug)]
pub struct CheckInput {
	pub interfaces: Vec<Iface>,
	pub advertised: IpAddr,
	pub ssdp: bool,
	pub bonjour: bool,
	pub lan: LanProbe,
}

const TUNNEL_PREFIXES: [&str; 8] = [
	"utun",
	"ipsec",
	"ppp",
	"tun",
	"wg",
	"feth",
	"zt",
	"tailscale",
];

// `true` if `name` is exactly `prefix` followed by zero or more ASCII
// letters or digits: covers both a purely numeric suffix (`utun4`,
// `bridge100`) and the alphanumeric ones some drivers assign (ZeroTier's
// `ztc3q6pdsw`, Docker's `veth3a2b1c`).
fn has_alnum_suffix(name: &str, prefix: &str) -> bool {
	name.strip_prefix(prefix)
		.is_some_and(|rest| rest.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// VPN and overlay interfaces.
///
/// `utun*`, `ipsec*`, `ppp*`, `tun*`, `wg*`, `feth*` (`ZeroTier`, macOS),
/// `zt*` (`ZeroTier`, Linux), `tailscale*` (Linux).
#[must_use]
pub fn is_tunnel(name: &str) -> bool {
	TUNNEL_PREFIXES.iter().any(|p| has_alnum_suffix(name, p))
}

const VIRTUAL_PREFIXES: [&str; 7] = [
	"bridge", "vnic", "vmnet", "vmenet", "docker", "virbr", "veth",
];

// VM and container host-only bridges, not real LAN segments: `bridge*`
// (macOS Internet Sharing/VM), `vnic*` (Parallels), `vmnet*`/`vmenet*`
// (VMware/UTM), `docker*`, `virbr*` (libvirt), `veth*`. Not `br*`: many
// Linux distros use `br0` for the real LAN bridge.
fn is_virtual(name: &str) -> bool {
	VIRTUAL_PREFIXES.iter().any(|p| has_alnum_suffix(name, p))
}

fn is_lan(i: &Iface) -> bool {
	!i.ip.is_loopback()
		&& !i.ip.is_link_local()
		&& !i.ip.is_unspecified()
		&& !is_tunnel(&i.name)
		&& !is_virtual(&i.name)
}

fn network(i: &Iface) -> (u32, u32) {
	let mask = u32::from(i.netmask);
	(u32::from(i.ip) & mask, mask)
}

/// Every LAN IPv4 address (not loopback, link-local or tunnel), the
/// advertised one first, without duplicates. This is identity `addresses`.
#[must_use]
pub fn lan_addresses(ifaces: &[Iface], advertised: IpAddr) -> Vec<Ipv4Addr> {
	let mut out: Vec<Ipv4Addr> = Vec::new();
	if let IpAddr::V4(adv) = advertised
		&& ifaces.iter().any(|i| is_lan(i) && i.ip == adv)
	{
		out.push(adv);
	}
	for i in ifaces.iter().filter(|i| is_lan(i)) {
		if !out.contains(&i.ip) {
			out.push(i.ip);
		}
	}
	out
}

fn vpn_check(input: &CheckInput) -> Option<Check> {
	let tunnels: Vec<&Iface> = input
		.interfaces
		.iter()
		.filter(|i| is_tunnel(&i.name) && !i.ip.is_link_local())
		.collect();
	let first = tunnels.first()?;
	let on = |i: &&Iface| IpAddr::V4(i.ip) == input.advertised;
	if let Some(t) = tunnels.iter().copied().find(on) {
		return Some(Check {
			id: "vpn-active",
			severity: Severity::Warning,
			message: format!(
				"Spritz advertises {}, an address on VPN interface {}, so players on your local network may not reach it. Pause the VPN or bind to your LAN address.",
				input.advertised, t.name
			),
		});
	}
	let message = input
		.interfaces
		.iter()
		.filter(|i| is_lan(i))
		.find(on)
		.map_or_else(
			|| {
				format!(
					"VPN interface {} is active; Spritz advertises {}.",
					first.name, input.advertised
				)
			},
			|lan| {
				format!(
					"VPN interface {} is active; Spritz advertises {} on {}.",
					first.name, input.advertised, lan.name
				)
			},
		);
	Some(Check {
		id: "vpn-active",
		severity: Severity::Info,
		message,
	})
}

#[must_use]
pub fn evaluate(input: &CheckInput) -> Vec<Check> {
	let mut checks = Vec::new();
	let lan: Vec<&Iface> = input.interfaces.iter().filter(|i| is_lan(i)).collect();

	if lan.is_empty() {
		checks.push(Check {
			id: "no-lan-address",
			severity: Severity::Error,
			message: "This computer has no local network address. Connect it to Wi-Fi or Ethernet."
				.into(),
		});
	}
	if input.lan == LanProbe::Denied {
		checks.push(Check {
			id: "local-network-denied",
			severity: Severity::Error,
			message: "macOS is blocking this app from the local network. Allow it in System Settings › Privacy & Security › Local Network.".into(),
		});
	}
	if !input.ssdp {
		checks.push(Check {
			id: "ssdp-unavailable",
			severity: Severity::Warning,
			message: "Discovery unavailable: UDP port 1900 is held by another app. Spritz players can still find this server through Bonjour or its address.".into(),
		});
	}
	if !input.bonjour {
		checks.push(Check {
			id: "bonjour-failed",
			severity: Severity::Warning,
			message: "Bonjour advertising failed. Players can still find this server through DLNA discovery or its address.".into(),
		});
	}
	checks.extend(vpn_check(input));
	let mut subnets: Vec<(u32, u32)> = lan.iter().map(|i| network(i)).collect();
	subnets.sort_unstable();
	subnets.dedup();
	if subnets.len() > 1 {
		let list: Vec<String> = subnets
			.iter()
			.map(|&(net, mask)| {
				let mut names: Vec<&str> = lan
					.iter()
					.copied()
					.filter(|i| network(i) == (net, mask))
					.map(|i| i.name.as_str())
					.collect();
				names.sort_unstable();
				names.dedup();
				format!(
					"{}/{} ({})",
					Ipv4Addr::from(net),
					mask.count_ones(),
					names.join(", ")
				)
			})
			.collect();
		checks.push(Check {
			id: "multiple-subnets",
			severity: Severity::Info,
			message: format!(
				"This computer is on more than one network ({}). Players must be on one of them; the address shown to players is {}.",
				list.join(", "),
				input.advertised
			),
		});
	}
	checks.sort_by_key(|c| std::cmp::Reverse(c.severity));
	checks
}

/// Every IPv4 address of an interface that is up on this machine.
#[must_use]
pub fn interfaces() -> Vec<Iface> {
	if_addrs::get_if_addrs()
		.unwrap_or_default()
		.into_iter()
		.filter(if_addrs::Interface::is_oper_up)
		.filter_map(|i| match i.addr {
			if_addrs::IfAddr::V4(v4) => Some(Iface {
				name: i.name,
				ip: v4.ip,
				netmask: v4.netmask,
			}),
			if_addrs::IfAddr::V6(_) => None,
		})
		.collect()
}

/// The first host on `iface`'s subnet that is not `iface` itself; on
/// nearly every home network this is the gateway.
#[must_use]
pub fn probe_target(iface: &Iface) -> Option<Ipv4Addr> {
	let (net, mask) = network(iface);
	if mask >= 0xFFFF_FFFE {
		return None;
	}
	let first = Ipv4Addr::from(net + 1);
	Some(if first == iface.ip {
		Ipv4Addr::from(net + 2)
	} else {
		first
	})
}

/// Send one UDP byte from `from` to `target:9` (discard).
///
/// If that fails with `HostUnreachable` or `always_tcp` is set, also try a
/// TCP connect to the same address (1 s timeout). A refused or timed-out
/// connect is normal.
#[must_use]
pub fn probe_host(from: Ipv4Addr, target: Ipv4Addr, always_tcp: bool) -> ProbeResult {
	let udp = UdpSocket::bind((from, 0)).and_then(|s| s.send_to(&[0], (target, 9)).map(|_| ()));
	let udp_blocked = matches!(&udp, Err(e) if e.kind() == ErrorKind::HostUnreachable);
	let tcp = (udp_blocked || always_tcp).then(|| {
		TcpStream::connect_timeout(&SocketAddr::from((target, 9)), Duration::from_secs(1))
			.map(|_| ())
	});
	ProbeResult { udp, tcp }
}

/// `Denied` only when both UDP and TCP were blocked.
///
/// UDP must fail with `HostUnreachable` and TCP to the same host must fail
/// with `HostUnreachable` or `PermissionDenied`. A single UDP failure is
/// also what an absent host produces, so it is not enough on its own.
#[must_use]
pub fn classify(result: &ProbeResult) -> LanProbe {
	let udp_blocked = matches!(&result.udp, Err(e) if e.kind() == ErrorKind::HostUnreachable);
	let tcp_blocked = matches!(
		&result.tcp,
		Some(Err(e)) if matches!(e.kind(), ErrorKind::HostUnreachable | ErrorKind::PermissionDenied)
	);
	if udp_blocked && tcp_blocked {
		LanProbe::Denied
	} else {
		LanProbe::Ok
	}
}

/// Probe the gateway of the advertised interface (else the first LAN
/// interface). macOS only, per spike S2; elsewhere `Skipped`.
#[must_use]
pub fn lan_probe(ifaces: &[Iface], advertised: IpAddr) -> LanProbe {
	if !cfg!(target_os = "macos") {
		return LanProbe::Skipped;
	}
	let lan: Vec<&Iface> = ifaces.iter().filter(|i| is_lan(i)).collect();
	let chosen = lan
		.iter()
		.find(|i| IpAddr::V4(i.ip) == advertised)
		.or_else(|| lan.first());
	let Some((iface, target)) = chosen.and_then(|i| probe_target(i).map(|t| (*i, t))) else {
		return LanProbe::Skipped;
	};
	classify(&probe_host(iface.ip, target, false))
}

#[must_use]
pub fn gather(advertised: IpAddr, ssdp: bool, bonjour: bool) -> CheckInput {
	let interfaces = interfaces();
	let lan = lan_probe(&interfaces, advertised);
	CheckInput {
		interfaces,
		advertised,
		ssdp,
		bonjour,
		lan,
	}
}

/// Checks in `after` that were not in `before` (same id and message).
#[must_use]
pub fn new_checks<'a>(before: &[Check], after: &'a [Check]) -> Vec<&'a Check> {
	after.iter().filter(|c| !before.contains(c)).collect()
}

/// Run the checks now and every [`CHECK_PERIOD`].
///
/// `on_change(before, after)` runs whenever the list differs from the
/// previous run, including the first run when it is non-empty. Must be
/// called inside a tokio runtime.
///
/// `input` does blocking socket I/O (`getifaddrs`, a UDP send, and on macOS
/// sometimes a TCP connect with up to a 1 s timeout), so each tick runs it on
/// the blocking pool rather than the async worker a media stream may share.
/// If that blocking task panics, that cycle is skipped and the monitor tries
/// again at the next tick.
pub fn spawn_monitor(
	input: impl Fn() -> CheckInput + Send + Sync + 'static,
	on_change: impl Fn(&[Check], &[Check]) + Send + 'static,
) -> (Arc<Mutex<Vec<Check>>>, tokio::task::JoinHandle<()>) {
	let latest = Arc::new(Mutex::new(Vec::new()));
	let shared = Arc::clone(&latest);
	let input = Arc::new(input);
	let task = tokio::spawn(async move {
		let mut interval = tokio::time::interval(CHECK_PERIOD);
		loop {
			interval.tick().await;
			let input = Arc::clone(&input);
			let Ok(gathered) = tokio::task::spawn_blocking(move || input()).await else {
				continue;
			};
			let after = evaluate(&gathered);
			let before = std::mem::replace(
				&mut *shared.lock().unwrap_or_else(PoisonError::into_inner),
				after.clone(),
			);
			if before != after {
				on_change(&before, &after);
			}
		}
	});
	(latest, task)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::{Error, ErrorKind};

	fn iface(name: &str, ip: &str, mask: &str) -> Iface {
		Iface {
			name: name.into(),
			ip: ip.parse().unwrap(),
			netmask: mask.parse().unwrap(),
		}
	}

	fn input(interfaces: Vec<Iface>) -> CheckInput {
		CheckInput {
			interfaces,
			advertised: "192.168.1.23".parse().unwrap(),
			ssdp: true,
			bonjour: true,
			lan: LanProbe::Ok,
		}
	}

	fn ids(checks: &[Check]) -> Vec<&'static str> {
		checks.iter().map(|c| c.id).collect()
	}

	#[test]
	fn a_healthy_mac_has_no_checks() {
		let checks = evaluate(&input(vec![
			iface("lo0", "127.0.0.1", "255.0.0.0"),
			iface("en0", "192.168.1.23", "255.255.255.0"),
		]));
		assert!(checks.is_empty(), "{checks:?}");
	}

	#[test]
	fn no_lan_address_is_an_error() {
		let checks = evaluate(&input(vec![
			iface("lo0", "127.0.0.1", "255.0.0.0"),
			iface("en0", "169.254.3.4", "255.255.0.0"),
		]));
		assert_eq!(ids(&checks), vec!["no-lan-address"]);
		assert_eq!(checks[0].severity, Severity::Error);
	}

	#[test]
	fn denied_local_network_is_an_error() {
		let mut i = input(vec![iface("en0", "192.168.1.23", "255.255.255.0")]);
		i.lan = LanProbe::Denied;
		assert_eq!(ids(&evaluate(&i)), vec!["local-network-denied"]);
	}

	#[test]
	fn ssdp_and_bonjour_failures_are_warnings() {
		let mut i = input(vec![iface("en0", "192.168.1.23", "255.255.255.0")]);
		i.ssdp = false;
		i.bonjour = false;
		let checks = evaluate(&i);
		assert_eq!(ids(&checks), vec!["ssdp-unavailable", "bonjour-failed"]);
		assert!(
			checks[0]
				.message
				.starts_with("Discovery unavailable: UDP port 1900 is held by another app")
		);
	}

	#[test]
	fn advertising_a_tunnel_address_is_a_warning() {
		let mut i = input(vec![
			iface("en0", "192.168.1.23", "255.255.255.0"),
			iface("utun4", "10.8.0.2", "255.255.255.0"),
		]);
		i.advertised = "10.8.0.2".parse().unwrap();
		let checks = evaluate(&i);
		assert_eq!(ids(&checks), vec!["vpn-active"]);
		assert_eq!(checks[0].severity, Severity::Warning);
		assert!(checks[0].message.contains("utun4"));
		assert!(checks[0].message.contains("10.8.0.2"));
	}

	#[test]
	fn a_tunnel_beside_the_advertised_lan_is_a_note() {
		let checks = evaluate(&input(vec![
			iface("en0", "192.168.1.23", "255.255.255.0"),
			iface("utun4", "100.101.102.103", "255.255.255.255"),
		]));
		assert_eq!(ids(&checks), vec!["vpn-active"]);
		assert_eq!(checks[0].severity, Severity::Info);
		assert_eq!(
			checks[0].message,
			"VPN interface utun4 is active; Spritz advertises 192.168.1.23 on en0."
		);
	}

	#[test]
	fn a_tunnel_with_an_unknown_advertised_interface_is_still_a_note() {
		let mut i = input(vec![
			iface("lo0", "127.0.0.1", "255.0.0.0"),
			iface("en0", "192.168.1.23", "255.255.255.0"),
			iface("wg0", "10.9.0.1", "255.255.255.0"),
		]);
		i.advertised = "127.0.0.1".parse().unwrap();
		let checks = evaluate(&i);
		assert_eq!(checks[0].severity, Severity::Info);
		assert_eq!(
			checks[0].message, "VPN interface wg0 is active; Spritz advertises 127.0.0.1.",
			"must not name lo0 as the LAN interface just because it matches the advertised address"
		);
	}

	#[test]
	fn tunnel_names() {
		for name in [
			"utun0",
			"utun12",
			"ipsec0",
			"ppp0",
			"tun1",
			"wg0",
			"feth1234",
			"zt0",
			"tailscale0",
			"ztc3q6pdsw",
		] {
			assert!(is_tunnel(name), "{name}");
		}
		for name in ["en0", "bridge100", "awdl0", "llw0", "lo0", "br0", "eth0"] {
			assert!(!is_tunnel(name), "{name}");
		}
	}

	#[test]
	fn virtual_interface_names() {
		for name in [
			"bridge100",
			"vnic0",
			"vmnet1",
			"vmenet0",
			"docker0",
			"virbr0",
			"veth1234",
			"veth3a2b1c",
		] {
			assert!(is_virtual(name), "{name}");
		}
		for name in ["en0", "br0", "lo0", "utun4", "feth1234", "eth0"] {
			assert!(!is_virtual(name), "{name}");
		}
	}

	#[test]
	fn a_virtual_bridge_beside_the_lan_does_not_trigger_multiple_subnets() {
		let mut i = input(vec![
			iface("en0", "192.168.4.21", "255.255.252.0"),
			iface("bridge100", "192.168.139.3", "255.255.254.0"),
		]);
		i.advertised = "192.168.4.21".parse().unwrap();
		let checks = evaluate(&i);
		assert!(checks.is_empty(), "{checks:?}");
	}

	#[test]
	fn lan_addresses_exclude_virtual_bridges() {
		let ifaces = vec![
			iface("en0", "192.168.4.21", "255.255.252.0"),
			iface("bridge100", "192.168.139.3", "255.255.254.0"),
		];
		let got = lan_addresses(&ifaces, "192.168.4.21".parse().unwrap());
		assert_eq!(got, vec!["192.168.4.21".parse::<Ipv4Addr>().unwrap()]);
	}

	#[test]
	fn a_zerotier_macos_interface_is_a_vpn_note_and_excluded_from_addresses() {
		let ifaces = vec![
			iface("en0", "192.168.1.23", "255.255.255.0"),
			iface("feth1234", "10.147.1.2", "255.255.255.0"),
		];
		let checks = evaluate(&input(ifaces.clone()));
		assert_eq!(ids(&checks), vec!["vpn-active"]);
		assert_eq!(checks[0].severity, Severity::Info);
		let addrs = lan_addresses(&ifaces, "192.168.1.23".parse().unwrap());
		assert_eq!(addrs, vec!["192.168.1.23".parse::<Ipv4Addr>().unwrap()]);
	}

	#[test]
	fn br0_still_counts_as_lan() {
		let ifaces = vec![iface("br0", "192.168.1.23", "255.255.255.0")];
		assert!(evaluate(&input(ifaces.clone())).is_empty());
		assert_eq!(
			lan_addresses(&ifaces, "192.168.1.23".parse().unwrap()),
			vec!["192.168.1.23".parse::<Ipv4Addr>().unwrap()]
		);
	}

	#[test]
	fn two_interfaces_on_a_wide_shared_subnet_are_one_network() {
		let checks = evaluate(&input(vec![
			iface("en0", "192.168.4.21", "255.255.252.0"),
			iface("en7", "192.168.4.32", "255.255.252.0"),
		]));
		assert!(checks.is_empty(), "{checks:?}");
	}

	#[test]
	fn two_lan_subnets_are_noted_with_the_advertised_address() {
		let checks = evaluate(&input(vec![
			iface("en0", "192.168.1.23", "255.255.255.0"),
			iface("en7", "10.0.0.5", "255.255.255.0"),
		]));
		assert_eq!(ids(&checks), vec!["multiple-subnets"]);
		assert_eq!(checks[0].severity, Severity::Info);
		assert!(checks[0].message.contains("192.168.1.0/24 (en0)"));
		assert!(checks[0].message.contains("10.0.0.0/24 (en7)"));
		assert!(checks[0].message.contains("192.168.1.23"));
	}

	#[test]
	fn two_addresses_on_one_subnet_are_one_network() {
		let checks = evaluate(&input(vec![
			iface("en0", "192.168.1.23", "255.255.255.0"),
			iface("en1", "192.168.1.24", "255.255.255.0"),
		]));
		assert!(checks.is_empty(), "{checks:?}");
	}

	#[test]
	fn checks_are_most_severe_first() {
		let mut i = input(vec![
			iface("en0", "192.168.1.23", "255.255.255.0"),
			iface("en7", "10.0.0.5", "255.255.255.0"),
		]);
		i.lan = LanProbe::Denied;
		i.ssdp = false;
		let sev: Vec<Severity> = evaluate(&i).iter().map(|c| c.severity).collect();
		let mut sorted = sev.clone();
		sorted.sort_by(|a, b| b.cmp(a));
		assert_eq!(sev, sorted);
	}

	#[test]
	fn lan_addresses_put_the_advertised_one_first_and_skip_the_rest() {
		let ifaces = vec![
			iface("lo0", "127.0.0.1", "255.0.0.0"),
			iface("en7", "10.0.0.5", "255.255.255.0"),
			iface("en0", "192.168.1.23", "255.255.255.0"),
			iface("en0", "169.254.9.9", "255.255.0.0"),
			iface("utun4", "100.101.102.103", "255.255.255.255"),
			iface("en1", "192.168.1.23", "255.255.255.0"),
		];
		let got = lan_addresses(&ifaces, "192.168.1.23".parse().unwrap());
		let want: Vec<Ipv4Addr> =
			vec!["192.168.1.23".parse().unwrap(), "10.0.0.5".parse().unwrap()];
		assert_eq!(got, want);
	}

	#[test]
	fn lan_addresses_without_a_matching_advertised_address_keep_interface_order() {
		let ifaces = vec![
			iface("en0", "192.168.1.23", "255.255.255.0"),
			iface("en7", "10.0.0.5", "255.255.255.0"),
		];
		let got = lan_addresses(&ifaces, "::1".parse().unwrap());
		assert_eq!(got.len(), 2);
		assert_eq!(got[0], "192.168.1.23".parse::<Ipv4Addr>().unwrap());
	}

	#[test]
	fn probe_target_is_the_first_other_host() {
		assert_eq!(
			probe_target(&iface("en0", "192.168.1.23", "255.255.255.0")),
			Some("192.168.1.1".parse().unwrap())
		);
		assert_eq!(
			probe_target(&iface("en0", "192.168.1.1", "255.255.255.0")),
			Some("192.168.1.2".parse().unwrap())
		);
		assert_eq!(
			probe_target(&iface("en0", "10.0.0.1", "255.255.255.254")),
			None
		);
		assert_eq!(
			probe_target(&iface("en0", "10.0.0.1", "255.255.255.255")),
			None
		);
	}

	// `tcp` distinguishes "not attempted" (`None`) from "attempted and
	// succeeded" (`Some(None)`) from "attempted and failed" (`Some(Some(_))`),
	// mirroring `ProbeResult::tcp`'s own `Option<io::Result<()>>`.
	#[allow(clippy::option_option)]
	fn result(udp: Option<ErrorKind>, tcp: Option<Option<ErrorKind>>) -> ProbeResult {
		let r = |k: Option<ErrorKind>| k.map_or(Ok(()), |k| Err(Error::from(k)));
		ProbeResult {
			udp: r(udp),
			tcp: tcp.map(r),
		}
	}

	#[test]
	fn denial_needs_both_udp_and_tcp_to_be_blocked() {
		use ErrorKind::{ConnectionRefused, HostUnreachable, PermissionDenied, TimedOut};
		assert_eq!(
			classify(&result(Some(HostUnreachable), Some(Some(HostUnreachable)))),
			LanProbe::Denied
		);
		assert_eq!(
			classify(&result(Some(HostUnreachable), Some(Some(PermissionDenied)))),
			LanProbe::Denied
		);
		assert_eq!(
			classify(&result(
				Some(HostUnreachable),
				Some(Some(ConnectionRefused))
			)),
			LanProbe::Ok
		);
		assert_eq!(
			classify(&result(Some(HostUnreachable), Some(Some(TimedOut)))),
			LanProbe::Ok
		);
		assert_eq!(classify(&result(Some(HostUnreachable), None)), LanProbe::Ok);
		assert_eq!(
			classify(&result(None, Some(Some(HostUnreachable)))),
			LanProbe::Ok
		);
		assert_eq!(classify(&result(None, None)), LanProbe::Ok);
	}

	#[test]
	fn new_checks_are_the_ones_not_there_before() {
		let a = Check {
			id: "vpn-active",
			severity: Severity::Warning,
			message: "a".into(),
		};
		let b = Check {
			id: "bonjour-failed",
			severity: Severity::Warning,
			message: "b".into(),
		};
		let after = [a.clone(), b.clone()];
		assert_eq!(new_checks(&[a], &after), vec![&b]);
		assert!(new_checks(&after, &after).is_empty());
	}

	type Transition = (Vec<Check>, Vec<Check>);

	/// Wait until `input()` has been called `n` times. Each call runs on the
	/// blocking pool, a real OS thread the paused clock does not drive, so
	/// poll in real time (bounded) and yield so the monitor task can run.
	async fn wait_for_calls(calls: &std::sync::atomic::AtomicUsize, n: usize) {
		let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
		while calls.load(std::sync::atomic::Ordering::SeqCst) < n {
			assert!(
				std::time::Instant::now() < deadline,
				"input() was not called {n} times"
			);
			tokio::task::yield_now().await;
			std::thread::sleep(std::time::Duration::from_millis(1));
		}
	}

	#[tokio::test(start_paused = true)]
	async fn spawn_monitor_reports_only_on_change() {
		use std::sync::atomic::{AtomicUsize, Ordering};

		let healthy = || input(vec![iface("en0", "192.168.1.23", "255.255.255.0")]);
		let denied = move || {
			let mut i = healthy();
			i.lan = LanProbe::Denied;
			i
		};

		// Healthy, healthy, denied, denied, healthy, healthy: two transitions.
		let calls = Arc::new(AtomicUsize::new(0));
		let counter = Arc::clone(&calls);
		let seen: Arc<Mutex<Vec<Transition>>> = Arc::new(Mutex::new(Vec::new()));
		let seen_writer = Arc::clone(&seen);

		let (_latest, task) = spawn_monitor(
			move || match counter.fetch_add(1, Ordering::SeqCst) {
				2 | 3 => denied(),
				_ => healthy(),
			},
			move |before, after| {
				seen_writer
					.lock()
					.unwrap_or_else(PoisonError::into_inner)
					.push((before.to_vec(), after.to_vec()));
			},
		);

		wait_for_calls(&calls, 1).await; // the interval's first tick fires immediately
		for n in 2..=6 {
			tokio::time::advance(CHECK_PERIOD).await;
			wait_for_calls(&calls, n).await;
		}
		// The loop is sequential: a seventh call proves the sixth result was
		// compared and reported before the log is read.
		tokio::time::advance(CHECK_PERIOD).await;
		wait_for_calls(&calls, 7).await;
		task.abort();

		let log = seen.lock().unwrap_or_else(PoisonError::into_inner).clone();
		assert_eq!(log.len(), 2, "{log:?}");
		// The first non-empty run: healthy -> denied.
		assert!(log[0].0.is_empty());
		assert_eq!(ids(&log[0].1), vec!["local-network-denied"]);
		// Back to healthy: denied -> empty. No entry for the two repeats in between.
		assert_eq!(ids(&log[1].0), vec!["local-network-denied"]);
		assert!(log[1].1.is_empty());
	}
}
