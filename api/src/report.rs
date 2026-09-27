//! Terminal lines for the CLI: client progress and network checks. Every
//! client string in these lines went through `dlna::clients::sanitize`, so
//! none can carry a terminal escape sequence.

use crate::netcheck::{Check, Severity};
use dlna::clients::{ClientRecord, diagnosis};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

#[derive(Clone, Default, PartialEq, Eq)]
struct Seen {
	stages: [bool; 4],
	diagnosis: Option<String>,
	announced: bool,
}

/// Remembers what was already printed per client, so each stage and each
/// diagnosis is announced once.
#[derive(Default)]
pub struct ClientReporter {
	seen: HashMap<IpAddr, Seen>,
}

const VERBS: [&str; 4] = [
	"is searching for this server",
	"found this server",
	"is browsing",
	"is streaming",
];

impl ClientReporter {
	pub fn lines(&mut self, clients: &[ClientRecord], now: Instant, http_port: u16) -> Vec<String> {
		let mut out = Vec::new();
		for c in clients {
			let stages = [
				c.searched.is_some(),
				c.found.is_some(),
				c.browsed.is_some(),
				c.streamed.is_some(),
			];
			let seen = self.seen.entry(c.ip).or_default();
			if c.announced.is_some() && !seen.announced {
				out.push(format!(
					"Client: {} at {} is on the network",
					c.label(),
					c.ip
				));
			}
			seen.announced = c.announced.is_some();
			let newest = (0..4).rev().find(|&i| stages[i] && !seen.stages[i]);
			if let Some(i) = newest {
				out.push(format!("Client: {} at {} {}", c.label(), c.ip, VERBS[i]));
			}
			seen.stages = stages;
			let diag = diagnosis(c, now, http_port);
			if diag.is_some() && diag != seen.diagnosis {
				out.push(format!(
					"Client {}: {}",
					c.ip,
					diag.as_deref().unwrap_or_default()
				));
			}
			seen.diagnosis = diag;
		}
		self.seen
			.retain(|ip, _| clients.iter().any(|c| c.ip == *ip));
		out
	}
}

#[must_use]
pub fn check_line(check: &Check) -> String {
	let prefix = match check.severity {
		Severity::Error => "error",
		Severity::Warning => "warning",
		Severity::Info => "note",
	};
	format!("{prefix}: {}", check.message)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::netcheck::{Check, Severity};
	use dlna::clients::{ClientTracker, DIAGNOSIS_WINDOW, STUCK_AFTER, Stage};
	use std::time::Duration;

	const UA: &str = "SpritzPlayer/1.2 (tvOS 26.0; Apple TV)";

	#[test]
	fn announces_each_new_stage_once() {
		let t = ClientTracker::default();
		let mut r = ClientReporter::default();
		let now = std::time::Instant::now();
		let ip = "192.168.1.40".parse().unwrap();

		t.record_at(now, ip, Stage::Found, Some(UA), None);
		assert_eq!(
			r.lines(&t.snapshot_at(now), now, 8080),
			vec!["Client: Apple TV (tvOS 26.0) at 192.168.1.40 found this server"]
		);
		assert!(r.lines(&t.snapshot_at(now), now, 8080).is_empty());

		t.record_at(now, ip, Stage::Browsed, Some(UA), None);
		t.record_at(now, ip, Stage::Streamed, Some(UA), None);
		assert_eq!(
			r.lines(&t.snapshot_at(now), now, 8080),
			vec!["Client: Apple TV (tvOS 26.0) at 192.168.1.40 is streaming"]
		);
	}

	#[test]
	fn announces_a_diagnosis_once() {
		let t = ClientTracker::default();
		let mut r = ClientReporter::default();
		let start = std::time::Instant::now();
		let ip = "192.168.1.40".parse().unwrap();
		t.record_at(
			start,
			ip,
			Stage::Searched,
			Some("tvOS UPnP/1.1 SpritzPlayer/1.0"),
			None,
		);
		assert_eq!(
			r.lines(&t.snapshot_at(start), start, 8080),
			vec!["Client: tvOS at 192.168.1.40 is searching for this server"]
		);
		let later = start + STUCK_AFTER;
		let lines = r.lines(&t.snapshot_at(later), later, 8080);
		assert_eq!(
			lines,
			vec![
				"Client 192.168.1.40: tvOS searched for this server but never connected. Check the firewall on this Mac (port 8080)."
			]
		);
		assert!(r.lines(&t.snapshot_at(later), later, 8080).is_empty());
		let much_later = start + DIAGNOSIS_WINDOW + Duration::from_secs(1);
		assert!(
			r.lines(&t.snapshot_at(much_later), much_later, 8080)
				.is_empty()
		);
	}

	#[test]
	fn a_player_that_keeps_searching_is_diagnosed_once() {
		let t = ClientTracker::default();
		let mut r = ClientReporter::default();
		let start = std::time::Instant::now();
		let ip = "192.168.1.40".parse().unwrap();
		let mut diagnoses = 0;
		for s in 0..=300 {
			let now = start + Duration::from_secs(s);
			if s % 30 == 0 {
				t.record_at(
					now,
					ip,
					Stage::Searched,
					Some("tvOS UPnP/1.1 SpritzPlayer/1.0"),
					None,
				);
			}
			diagnoses += r
				.lines(&t.snapshot_at(now), now, 8080)
				.iter()
				.filter(|l| l.contains("never connected"))
				.count();
		}
		assert_eq!(diagnoses, 1);
	}

	#[test]
	fn announces_a_player_on_the_network_once() {
		use dlna::clients::Announcement;
		let t = ClientTracker::default();
		let mut r = ClientReporter::default();
		let now = std::time::Instant::now();
		let a = Announcement {
			instance: "Den".into(),
			product: "SpritzPlayer/1.2".into(),
			platform: "tvOS 26.0".into(),
			model: "Apple TV".into(),
		};
		t.announce_at(now, "192.168.1.40".parse().unwrap(), &a);
		assert_eq!(
			r.lines(&t.snapshot_at(now), now, 8080),
			vec!["Client: Den (tvOS 26.0) at 192.168.1.40 is on the network"]
		);
		assert!(r.lines(&t.snapshot_at(now), now, 8080).is_empty());
	}

	#[test]
	fn check_lines_carry_the_severity() {
		let c = |severity| Check {
			id: "x",
			severity,
			message: "m".into(),
		};
		assert_eq!(check_line(&c(Severity::Error)), "error: m");
		assert_eq!(check_line(&c(Severity::Warning)), "warning: m");
		assert_eq!(check_line(&c(Severity::Info)), "note: m");
	}
}
