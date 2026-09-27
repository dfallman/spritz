//! Which clients are talking to this server, and how far each got.
//!
//! A TV that never finds the server cannot show an error, but the server
//! can see it searching. The tracker records each peer's progress through
//! search (SSDP), connection (identity or description), browsing (SOAP) and
//! streaming, so the CLI and the macOS app can say where a client got stuck.
//!
//! Records do not expire on idle: a Player that watched something an hour
//! ago stays listed (the app greys it out) rather than vanishing and looking
//! like a fault. Only the cap evicts, least recently seen first.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Most peers remembered at once. The least recently seen is evicted first.
pub const MAX_CLIENTS: usize = 64;
/// A search with no connection after this long is reported.
pub const STUCK_AFTER: Duration = Duration::from_secs(15);
/// A connection this long before a search still counts, so one lost SSDP
/// reply does not report a working client as stuck.
pub const SEARCH_GRACE: Duration = Duration::from_mins(2);
/// A search older than this is no longer reported: the client gave up, and
/// an old failure should not hold a warning indefinitely.
pub const DIAGNOSIS_WINDOW: Duration = Duration::from_mins(10);
/// Longest client-supplied field, in characters.
pub const MAX_FIELD_CHARS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
	Searched,
	Found,
	Browsed,
	Streamed,
}

/// What a user agent says about a client. Empty strings mean unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Agent {
	/// `SpritzPlayer/1.2`, or the first `name/version` token of any other agent.
	pub product: String,
	/// `tvOS 26.0` (HTTP form) or `tvOS` (SSDP form). Spritz agents only.
	pub platform: String,
	/// `Apple TV`. Spritz agents only.
	pub model: String,
	/// Carries a `Spritz<Name>/<version>` token.
	pub spritz: bool,
}

/// Characters that must never reach a terminal or a label: C0 and C1
/// controls (ESC, CSI, BEL, newlines) and invisible formatting characters
/// that can reorder or hide text.
fn invisible(c: char) -> bool {
	c.is_control()
		|| matches!(
			c,
			'\u{200b}'..='\u{200f}'
				| '\u{202a}'..='\u{202e}'
				| '\u{2060}'..='\u{2069}'
				| '\u{061c}'
				| '\u{180e}'
				| '\u{2028}'
				| '\u{2029}'
				| '\u{feff}'
				| '\u{fff9}'..='\u{fffb}'
				| '\u{e0000}'..='\u{e007f}'
		)
}

/// Clean a client-supplied string for display: drop control and invisible
/// characters, trim, cap at [`MAX_FIELD_CHARS`] characters. `None` when
/// nothing is left.
#[must_use]
pub fn sanitize(raw: &str) -> Option<String> {
	let kept: String = raw.chars().filter(|c| !invisible(*c)).collect();
	let capped: String = kept.trim().chars().take(MAX_FIELD_CHARS).collect();
	let capped = capped.trim_end();
	(!capped.is_empty()).then(|| capped.to_string())
}

fn field(raw: &str) -> String {
	sanitize(raw).unwrap_or_default()
}

pub(crate) fn is_spritz_token(token: &str) -> bool {
	let Some((name, version)) = token.split_once('/') else {
		return false;
	};
	let Some(suffix) = name.strip_prefix("Spritz") else {
		return false;
	};
	!suffix.is_empty()
		&& suffix.chars().all(|c| c.is_ascii_alphabetic())
		&& version.starts_with(|c: char| c.is_ascii_digit())
}

/// Parse either spritz form, `<os> UPnP/1.1 SpritzPlayer/<v>` (SSDP) or
/// `SpritzPlayer/<v> (<os> <ver>; <model>)` (HTTP), or any other agent.
#[must_use]
pub fn parse_agent(ua: &str) -> Agent {
	let (head, comment) = match (ua.find('('), ua.rfind(')')) {
		(Some(open), Some(close)) if close > open => (
			format!("{} {}", &ua[..open], &ua[close + 1..]),
			Some(&ua[open + 1..close]),
		),
		_ => (ua.to_string(), None),
	};
	let tokens: Vec<&str> = head.split_whitespace().collect();
	let spritz = tokens.iter().copied().find(|t| is_spritz_token(t));
	let product = spritz
		.or_else(|| tokens.iter().copied().find(|t| t.contains('/')))
		.map(field)
		.unwrap_or_default();

	let (mut platform, mut model) = (String::new(), String::new());
	if spritz.is_some() {
		if let Some(comment) = comment {
			let mut parts = comment.split(';');
			platform = field(parts.next().unwrap_or(""));
			model = field(parts.next().unwrap_or(""));
		} else if let Some(first) = tokens.first().filter(|t| !t.contains('/')) {
			platform = field(first);
		}
	}
	Agent {
		product,
		platform,
		model,
		spritz: spritz.is_some(),
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientRecord {
	pub ip: IpAddr,
	pub agent: Agent,
	/// From `X-Spritz-Device-Name` (or a Bonjour instance name); empty when unknown.
	pub device_name: String,
	pub first_seen: Instant,
	pub last_seen: Instant,
	/// When the peer's `_spritz-player._tcp` advertisement appeared; `None`
	/// when not advertised or withdrawn. Set only by `ClientTracker::announce`
	/// (built only if spike S3 puts the Player advertisement in v1).
	pub announced: Option<Instant>,
	pub searched: Option<Instant>,
	pub found: Option<Instant>,
	pub browsed: Option<Instant>,
	pub streamed: Option<Instant>,
	/// The first search with no HTTP stage within [`SEARCH_GRACE`] before
	/// it; cleared by any HTTP stage. Keying the diagnosis on this rather
	/// than the latest search keeps it steady for a client that keeps
	/// searching.
	pub unanswered_since: Option<Instant>,
	/// Made at least one HTTP request.
	pub http: bool,
}

impl ClientRecord {
	/// Shown to the user: Spritz clients always, others once they used HTTP.
	#[must_use]
	pub const fn listed(&self) -> bool {
		self.agent.spritz || self.http
	}

	/// `Living Room (tvOS 26.0)`, `Apple TV (tvOS 26.0)`, `Samsung`, `Unknown device`.
	/// A Spritz client is named by its device, never by the app, so one known
	/// only from SSDP is labelled by its platform (`tvOS`).
	#[must_use]
	pub fn label(&self) -> String {
		let product_name = self.agent.product.split('/').next().unwrap_or("");
		let fallback = if self.agent.spritz { "" } else { product_name };
		let name = [
			self.device_name.as_str(),
			self.agent.model.as_str(),
			fallback,
		]
		.into_iter()
		.find(|s| !s.is_empty());
		match (name, self.agent.platform.is_empty()) {
			(Some(name), false) => format!("{name} ({})", self.agent.platform),
			(Some(name), true) => name.to_string(),
			(None, false) => self.agent.platform.clone(),
			(None, true) => "Unknown device".to_string(),
		}
	}

	/// Latest HTTP-stage timestamp.
	#[must_use]
	pub fn last_http(&self) -> Option<Instant> {
		[self.found, self.browsed, self.streamed]
			.into_iter()
			.flatten()
			.max()
	}
}

/// What a `_spritz-player._tcp` advertisement says: its instance name and TXT.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Announcement {
	pub instance: String,
	/// `product=` TXT value, e.g. `SpritzPlayer/1.2`.
	pub product: String,
	/// `platform=` TXT value, e.g. `tvOS 26.0`.
	pub platform: String,
	/// `model=` TXT value, e.g. `Apple TV`.
	pub model: String,
}

/// Shared, cloneable record of client progress. One per server; carried in
/// every `DlnaConfig` snapshot so a library reload keeps it.
#[derive(Clone, Default)]
pub struct ClientTracker {
	inner: Arc<Mutex<HashMap<IpAddr, ClientRecord>>>,
}

pub(crate) fn normalise(ip: IpAddr) -> IpAddr {
	match ip {
		IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
		IpAddr::V4(_) => ip,
	}
}

pub(crate) const fn blank(ip: IpAddr, now: Instant) -> ClientRecord {
	ClientRecord {
		ip,
		agent: Agent {
			product: String::new(),
			platform: String::new(),
			model: String::new(),
			spritz: false,
		},
		device_name: String::new(),
		first_seen: now,
		last_seen: now,
		announced: None,
		searched: None,
		found: None,
		browsed: None,
		streamed: None,
		unanswered_since: None,
		http: false,
	}
}

impl ClientTracker {
	pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, ClientRecord>> {
		self.inner.lock().unwrap_or_else(PoisonError::into_inner)
	}

	pub fn record(&self, ip: IpAddr, stage: Stage, agent: Option<&str>, device_name: Option<&str>) {
		self.record_at(Instant::now(), ip, stage, agent, device_name);
	}

	// The lock must span the whole check-modify-evict sequence below: `record`
	// borrows from `map`, so clippy's early-drop suggestion does not type-check.
	#[allow(clippy::significant_drop_tightening)]
	pub fn record_at(
		&self,
		now: Instant,
		ip: IpAddr,
		stage: Stage,
		agent: Option<&str>,
		device_name: Option<&str>,
	) {
		let ip = normalise(ip);
		let parsed = agent.map(parse_agent).unwrap_or_default();
		let mut map = self.lock();
		if stage == Stage::Searched && !parsed.spritz && !map.contains_key(&ip) {
			return;
		}
		let record = map.entry(ip).or_insert_with(|| blank(ip, now));
		// A Spritz agent beats any other; between Spritz agents the detailed
		// HTTP form (with a model) beats the bare SSDP form. Other agents only
		// fill an empty record, so AVPlayer's agent never renames the Player.
		let replace = if parsed.spritz {
			!record.agent.spritz || !parsed.model.is_empty() || record.agent.platform.is_empty()
		} else {
			!record.agent.spritz && record.agent.product.is_empty()
		};
		if replace {
			record.agent = parsed;
		}
		if let Some(name) = device_name.and_then(sanitize) {
			record.device_name = name;
		}
		record.last_seen = now;
		if stage == Stage::Searched {
			let cutoff = now.checked_sub(SEARCH_GRACE).unwrap_or(now);
			let answered = record.last_http().is_some_and(|t| t >= cutoff);
			if record.unanswered_since.is_none() && !answered {
				record.unanswered_since = Some(now);
			}
		} else {
			record.unanswered_since = None;
		}
		match stage {
			Stage::Searched => record.searched = Some(now),
			Stage::Found => record.found = Some(now),
			Stage::Browsed => record.browsed = Some(now),
			Stage::Streamed => record.streamed = Some(now),
		}
		if stage != Stage::Searched {
			record.http = true;
		}
		evict(&mut map);
	}

	pub fn announce(&self, ip: IpAddr, a: &Announcement) {
		self.announce_at(Instant::now(), ip, a);
	}

	/// A Spritz player advertised itself from `ip`. Non-Spritz products are
	/// ignored. The first appearance time is kept across repeats.
	// The lock must span the whole check-modify-evict sequence below, as in
	// `record_at`.
	#[allow(clippy::significant_drop_tightening)]
	pub fn announce_at(&self, now: Instant, ip: IpAddr, a: &Announcement) {
		let product = field(&a.product);
		if !is_spritz_token(&product) {
			return;
		}
		let agent = Agent {
			product,
			platform: field(&a.platform),
			model: field(&a.model),
			spritz: true,
		};
		let ip = normalise(ip);
		let mut map = self.lock();
		let record = map.entry(ip).or_insert_with(|| blank(ip, now));
		// The TXT form is as detailed as the HTTP one; it replaces anything
		// but a Spritz agent that already names a model.
		if !record.agent.spritz || record.agent.model.is_empty() {
			record.agent = agent;
		}
		if record.device_name.is_empty()
			&& let Some(name) = sanitize(&a.instance).filter(|n| *n != record.agent.model)
		{
			record.device_name = name;
		}
		if record.announced.is_none() {
			record.announced = Some(now);
		}
		record.last_seen = now;
		evict(&mut map);
	}

	/// The advertisement from `ip` went away (the app was suspended or quit).
	pub fn withdraw(&self, ip: IpAddr) {
		if let Some(r) = self.lock().get_mut(&normalise(ip)) {
			r.announced = None;
		}
	}

	#[must_use]
	pub fn snapshot(&self) -> Vec<ClientRecord> {
		self.snapshot_at(Instant::now())
	}

	/// Listed records, most recently seen first. Listing does not depend on
	/// time any more (records do not expire); `_now` stays so callers that
	/// pass one clock to both this and [`diagnosis`] keep working.
	#[must_use]
	pub fn snapshot_at(&self, _now: Instant) -> Vec<ClientRecord> {
		let mut list: Vec<ClientRecord> = self
			.lock()
			.values()
			.filter(|r| r.listed())
			.cloned()
			.collect();
		list.sort_by_key(|r| std::cmp::Reverse(r.last_seen));
		list
	}
}

pub(crate) fn evict(map: &mut HashMap<IpAddr, ClientRecord>) {
	while map.len() > MAX_CLIENTS {
		let Some(oldest) = map.values().min_by_key(|r| r.last_seen).map(|r| r.ip) else {
			break;
		};
		map.remove(&oldest);
	}
}

/// One sentence explaining where `record` got stuck, or `None`. A stuck
/// search is reported first; an advertised player that never connects next.
///
/// Only Spritz clients are diagnosed: other peers are tracked once they
/// connected, and TVs send routine searches without reconnecting. The
/// search diagnosis runs from [`STUCK_AFTER`] past the first unanswered
/// search until the latest search is older than [`DIAGNOSIS_WINDOW`], so a
/// client that keeps searching gets one steady diagnosis.
#[must_use]
pub fn diagnosis(record: &ClientRecord, now: Instant, http_port: u16) -> Option<String> {
	search_diagnosis(record, now, http_port).or_else(|| announce_diagnosis(record, now, http_port))
}

fn search_diagnosis(record: &ClientRecord, now: Instant, http_port: u16) -> Option<String> {
	if !record.agent.spritz {
		return None;
	}
	let stuck = now.saturating_duration_since(record.unanswered_since?);
	let searched = now.saturating_duration_since(record.searched?);
	if stuck < STUCK_AFTER || searched > DIAGNOSIS_WINDOW {
		return None;
	}
	Some(format!(
		"{} searched for this server but never connected. Check the firewall on this Mac (port {http_port}).",
		record.label()
	))
}

/// An advertised player is stuck when it has been on the network for
/// [`STUCK_AFTER`] and no HTTP stage happened in the [`SEARCH_GRACE`] before
/// the advertisement appeared: a lost first request still counts as fine.
fn announce_diagnosis(record: &ClientRecord, now: Instant, http_port: u16) -> Option<String> {
	let announced = record.announced?;
	let cutoff = announced.checked_sub(SEARCH_GRACE).unwrap_or(announced);
	let connected = record.last_http().is_some_and(|t| t >= cutoff);
	if connected || now.saturating_duration_since(announced) < STUCK_AFTER {
		return None;
	}
	Some(format!(
		"{} is on the network but has not connected to this server. Check the firewall on this Mac (port {http_port}).",
		record.label()
	))
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	fn ip(s: &str) -> IpAddr {
		s.parse().unwrap()
	}

	const PLAYER_SSDP: &str = "tvOS UPnP/1.1 SpritzPlayer/1.0";
	const PLAYER_HTTP: &str = "SpritzPlayer/1.2 (tvOS 26.0; Apple TV)";
	const AVPLAYER: &str =
		"AppleCoreMedia/1.0.0.23J (Apple TV; U; CPU OS 26_0 like Mac OS X; en_us)";

	#[test]
	fn sanitize_strips_controls_and_escapes() {
		assert_eq!(
			sanitize("\x1b[2JLiving\x07 Room\u{9b}").as_deref(),
			Some("[2JLiving Room")
		);
		assert_eq!(
			sanitize("\u{202e}Kitchen\u{200b}").as_deref(),
			Some("Kitchen")
		);
		assert_eq!(sanitize("  Den \n").as_deref(), Some("Den"));
	}

	#[test]
	fn sanitize_strips_other_invisible_characters() {
		for c in [
			'\u{061c}',
			'\u{2028}',
			'\u{2029}',
			'\u{180e}',
			'\u{e0000}',
			'\u{e0041}',
			'\u{e007f}',
		] {
			assert_eq!(
				sanitize(&format!("Den{c}Room")).as_deref(),
				Some("DenRoom"),
				"{c:?}"
			);
		}
	}

	#[test]
	fn sanitize_treats_empty_as_absent() {
		for raw in ["", "   ", "\t\r\n", "\x1b\x1b", "\u{200b}"] {
			assert_eq!(sanitize(raw), None, "{raw:?}");
		}
	}

	#[test]
	fn sanitize_caps_on_a_character_boundary() {
		let s = sanitize(&"é".repeat(100)).unwrap();
		assert_eq!(s.chars().count(), MAX_FIELD_CHARS);
		assert!(s.chars().all(|c| c == 'é'));
	}

	#[test]
	fn parses_the_ssdp_form() {
		let a = parse_agent(PLAYER_SSDP);
		assert_eq!(a.product, "SpritzPlayer/1.0");
		assert_eq!(a.platform, "tvOS");
		assert_eq!(a.model, "");
		assert!(a.spritz);
	}

	#[test]
	fn parses_the_http_form() {
		let a = parse_agent(PLAYER_HTTP);
		assert_eq!(a.product, "SpritzPlayer/1.2");
		assert_eq!(a.platform, "tvOS 26.0");
		assert_eq!(a.model, "Apple TV");
		assert!(a.spritz);
	}

	#[test]
	fn any_spritz_name_counts() {
		assert!(parse_agent("SpritzPad/2.0 (iPadOS 26.0; iPad)").spritz);
	}

	#[test]
	fn other_agents_keep_only_a_product() {
		let a = parse_agent(AVPLAYER);
		assert_eq!(a.product, "AppleCoreMedia/1.0.0.23J");
		assert_eq!(a.platform, "");
		assert_eq!(a.model, "");
		assert!(!a.spritz);
		assert_eq!(
			parse_agent("SEC_HHP_[TV] Samsung/1.0").product,
			"Samsung/1.0"
		);
	}

	#[test]
	fn malformed_spritz_tokens_are_not_spritz() {
		for ua in [
			"Spritz/1.0",
			"SpritzPlayer/",
			"SpritzPlayer/\u{200b}",
			"SpritzPlayer/beta",
			"Spritz2/1.0",
			"xSpritzPlayer/1.0",
			"",
			"   ",
		] {
			assert!(!parse_agent(ua).spritz, "{ua:?}");
		}
	}

	#[test]
	fn fields_are_capped() {
		let ua = format!(
			"SpritzPlayer/{} ({}; {})",
			"9".repeat(500),
			"x".repeat(500),
			"y".repeat(500)
		);
		let a = parse_agent(&ua);
		assert!(a.spritz);
		assert!(a.product.chars().count() <= MAX_FIELD_CHARS);
		assert!(a.platform.chars().count() <= MAX_FIELD_CHARS);
		assert!(a.model.chars().count() <= MAX_FIELD_CHARS);
	}

	#[test]
	fn agent_fields_are_sanitised() {
		let a = parse_agent("SpritzPlayer/1.0 (tvOS\x1b[31m 26.0; Apple\u{7} TV)");
		assert_eq!(a.platform, "tvOS[31m 26.0");
		assert_eq!(a.model, "Apple TV");
	}

	#[test]
	fn non_spritz_searches_from_strangers_are_ignored() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.record_at(
			now,
			ip("192.168.1.9"),
			Stage::Searched,
			Some("Android UPnP/1.0 Chrome"),
			None,
		);
		t.record_at(now, ip("192.168.1.10"), Stage::Searched, None, None);
		assert!(t.snapshot_at(now).is_empty());
	}

	#[test]
	fn a_spritz_search_is_listed() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.record_at(
			now,
			ip("192.168.1.40"),
			Stage::Searched,
			Some(PLAYER_SSDP),
			None,
		);
		let list = t.snapshot_at(now);
		assert_eq!(list.len(), 1);
		assert_eq!(list[0].searched, Some(now));
		assert_eq!(list[0].announced, None);
		assert!(list[0].agent.spritz);
		assert!(!list[0].http);
	}

	#[test]
	fn any_http_request_lists_a_peer() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.record_at(
			now,
			ip("192.168.1.50"),
			Stage::Browsed,
			Some("SEC_HHP_[TV] Samsung/1.0"),
			None,
		);
		let list = t.snapshot_at(now);
		assert_eq!(list.len(), 1);
		assert_eq!(list[0].agent.product, "Samsung/1.0");
		assert!(list[0].http);
	}

	#[test]
	fn a_stream_agent_does_not_rename_a_spritz_client() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.record_at(
			now,
			ip("192.168.1.40"),
			Stage::Found,
			Some(PLAYER_HTTP),
			None,
		);
		t.record_at(
			now,
			ip("192.168.1.40"),
			Stage::Streamed,
			Some(AVPLAYER),
			None,
		);
		let r = &t.snapshot_at(now)[0];
		assert_eq!(r.agent.product, "SpritzPlayer/1.2");
		assert!(r.agent.spritz);
		assert_eq!(r.streamed, Some(now));
	}

	#[test]
	fn a_later_ssdp_search_keeps_the_detailed_agent() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.record_at(
			now,
			ip("192.168.1.40"),
			Stage::Found,
			Some(PLAYER_HTTP),
			None,
		);
		t.record_at(
			now,
			ip("192.168.1.40"),
			Stage::Searched,
			Some(PLAYER_SSDP),
			None,
		);
		assert_eq!(t.snapshot_at(now)[0].label(), "Apple TV (tvOS 26.0)");
	}

	#[test]
	fn ipv4_mapped_peers_merge_with_ipv4() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.record_at(
			now,
			ip("192.168.1.40"),
			Stage::Searched,
			Some(PLAYER_SSDP),
			None,
		);
		t.record_at(
			now,
			ip("::ffff:192.168.1.40"),
			Stage::Found,
			Some(PLAYER_HTTP),
			None,
		);
		let list = t.snapshot_at(now);
		assert_eq!(list.len(), 1);
		assert_eq!(list[0].ip, ip("192.168.1.40"));
		assert!(list[0].found.is_some());
	}

	#[test]
	fn device_name_wins_the_label() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.record_at(
			now,
			ip("192.168.1.40"),
			Stage::Found,
			Some(PLAYER_HTTP),
			Some("Living Room"),
		);
		assert_eq!(t.snapshot_at(now)[0].label(), "Living Room (tvOS 26.0)");
	}

	#[test]
	fn a_hostile_or_blank_device_name_is_cleaned_or_ignored() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.record_at(
			now,
			ip("192.168.1.40"),
			Stage::Found,
			Some(PLAYER_HTTP),
			Some("\x1b[2J\x1b[H"),
		);
		assert_eq!(t.snapshot_at(now)[0].device_name, "[2J[H");
		t.record_at(
			now,
			ip("192.168.1.41"),
			Stage::Found,
			Some(PLAYER_HTTP),
			Some("   "),
		);
		let blank = t
			.snapshot_at(now)
			.into_iter()
			.find(|r| r.ip == ip("192.168.1.41"))
			.unwrap();
		assert_eq!(blank.device_name, "");
		assert_eq!(blank.label(), "Apple TV (tvOS 26.0)");
	}

	#[test]
	fn label_falls_back_to_model_then_product_name() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.record_at(now, ip("10.0.0.1"), Stage::Found, Some(PLAYER_HTTP), None);
		t.record_at(now, ip("10.0.0.2"), Stage::Found, Some("Samsung/1.0"), None);
		t.record_at(now, ip("10.0.0.3"), Stage::Found, None, None);
		let labels: Vec<String> = t.snapshot_at(now).iter().map(ClientRecord::label).collect();
		assert!(labels.contains(&"Apple TV (tvOS 26.0)".to_string()));
		assert!(labels.contains(&"Samsung".to_string()));
		assert!(labels.contains(&"Unknown device".to_string()));
	}

	#[test]
	fn idle_records_stay_listed() {
		let t = ClientTracker::default();
		let then = Instant::now();
		t.record_at(then, ip("10.0.0.1"), Stage::Found, Some(PLAYER_HTTP), None);
		assert_eq!(t.snapshot_at(then + Duration::from_hours(6)).len(), 1);
	}

	#[test]
	#[allow(clippy::cast_possible_truncation)]
	fn the_least_recently_seen_is_evicted_past_the_cap() {
		let t = ClientTracker::default();
		let start = Instant::now();
		for i in 0..=MAX_CLIENTS {
			let when = start + Duration::from_millis(i as u64);
			let addr = IpAddr::from([10, 0, (i / 256) as u8, (i % 256) as u8]);
			t.record_at(when, addr, Stage::Found, None, None);
		}
		let list = t.snapshot_at(start + Duration::from_secs(1));
		assert_eq!(list.len(), MAX_CLIENTS);
		assert!(!list.iter().any(|r| r.ip == ip("10.0.0.0")));
	}

	#[test]
	fn snapshot_is_most_recent_first() {
		let t = ClientTracker::default();
		let start = Instant::now();
		t.record_at(start, ip("10.0.0.1"), Stage::Found, None, None);
		t.record_at(
			start + Duration::from_secs(1),
			ip("10.0.0.2"),
			Stage::Found,
			None,
			None,
		);
		let list = t.snapshot_at(start + Duration::from_secs(2));
		assert_eq!(list[0].ip, ip("10.0.0.2"));
	}

	fn searched_only(at: Instant) -> ClientRecord {
		let t = ClientTracker::default();
		t.record_at(
			at,
			ip("192.168.1.40"),
			Stage::Searched,
			Some(PLAYER_SSDP),
			None,
		);
		t.snapshot_at(at).remove(0)
	}

	#[test]
	fn no_diagnosis_before_the_search_is_old_enough() {
		let start = Instant::now();
		let r = searched_only(start);
		assert_eq!(
			diagnosis(
				&r,
				(start + STUCK_AFTER)
					.checked_sub(Duration::from_secs(1))
					.unwrap(),
				8080
			),
			None
		);
	}

	#[test]
	fn a_search_with_no_connection_is_diagnosed() {
		let start = Instant::now();
		let r = searched_only(start);
		assert_eq!(
			diagnosis(&r, start + STUCK_AFTER, 8080).as_deref(),
			Some(
				"tvOS searched for this server but never connected. Check the firewall on this Mac (port 8080)."
			)
		);
	}

	#[test]
	fn the_diagnosis_lasts_until_the_window_closes() {
		let start = Instant::now();
		let r = searched_only(start);
		assert!(diagnosis(&r, start + DIAGNOSIS_WINDOW, 8080).is_some());
		assert_eq!(
			diagnosis(&r, start + DIAGNOSIS_WINDOW + Duration::from_secs(1), 8080),
			None
		);
		assert_eq!(diagnosis(&r, start + Duration::from_hours(1), 8080), None);
	}

	#[test]
	fn a_connection_after_the_search_clears_the_diagnosis() {
		let t = ClientTracker::default();
		let start = Instant::now();
		t.record_at(
			start,
			ip("192.168.1.40"),
			Stage::Searched,
			Some(PLAYER_SSDP),
			None,
		);
		t.record_at(
			start + Duration::from_secs(1),
			ip("192.168.1.40"),
			Stage::Found,
			None,
			None,
		);
		let r = t.snapshot_at(start + Duration::from_mins(1)).remove(0);
		assert_eq!(diagnosis(&r, start + Duration::from_mins(1), 8080), None);
	}

	#[test]
	fn a_lost_reply_inside_the_grace_is_not_diagnosed() {
		let t = ClientTracker::default();
		let start = Instant::now();
		t.record_at(
			start,
			ip("192.168.1.40"),
			Stage::Browsed,
			Some(PLAYER_HTTP),
			None,
		);
		let search = start + Duration::from_secs(90);
		t.record_at(
			search,
			ip("192.168.1.40"),
			Stage::Searched,
			Some(PLAYER_SSDP),
			None,
		);
		let now = search + Duration::from_mins(1);
		let r = t.snapshot_at(now).remove(0);
		assert_eq!(diagnosis(&r, now, 8080), None);
	}

	#[test]
	fn a_connection_older_than_the_grace_does_not_count() {
		let t = ClientTracker::default();
		let start = Instant::now();
		t.record_at(
			start,
			ip("192.168.1.40"),
			Stage::Browsed,
			Some(PLAYER_HTTP),
			None,
		);
		let search = start + SEARCH_GRACE + Duration::from_secs(5);
		t.record_at(
			search,
			ip("192.168.1.40"),
			Stage::Searched,
			Some(PLAYER_SSDP),
			None,
		);
		let now = search + STUCK_AFTER;
		let r = t.snapshot_at(now).remove(0);
		assert!(
			diagnosis(&r, now, 8080)
				.unwrap()
				.starts_with("Apple TV (tvOS 26.0) searched")
		);
	}

	fn search_at(t: &ClientTracker, at: Instant) {
		t.record_at(
			at,
			ip("192.168.1.40"),
			Stage::Searched,
			Some(PLAYER_SSDP),
			None,
		);
	}

	fn diagnosed_at(t: &ClientTracker, now: Instant) -> bool {
		diagnosis(&t.snapshot_at(now).remove(0), now, 8080).is_some()
	}

	/// Searches every `every` seconds for five minutes, checking each second.
	fn polling(every: u64) -> Vec<bool> {
		let t = ClientTracker::default();
		let start = Instant::now();
		(0..=300)
			.map(|s| {
				let now = start + Duration::from_secs(s);
				if s % every == 0 {
					search_at(&t, now);
				}
				diagnosed_at(&t, now)
			})
			.collect()
	}

	#[test]
	fn a_player_polling_every_30s_stays_diagnosed() {
		let seen = polling(30);
		assert!(seen[..15].iter().all(|d| !d), "{seen:?}");
		assert!(seen[15..].iter().all(|d| *d), "{seen:?}");
	}

	#[test]
	fn a_player_searching_every_10s_is_diagnosed() {
		let seen = polling(10);
		assert!(seen[..15].iter().all(|d| !d), "{seen:?}");
		assert!(seen[15..].iter().all(|d| *d), "{seen:?}");
	}

	#[test]
	fn an_http_stage_clears_the_unanswered_search() {
		let t = ClientTracker::default();
		let start = Instant::now();
		search_at(&t, start);
		assert!(diagnosed_at(&t, start + Duration::from_secs(20)));
		t.record_at(
			start + Duration::from_secs(25),
			ip("192.168.1.40"),
			Stage::Found,
			Some(PLAYER_HTTP),
			None,
		);
		assert_eq!(t.snapshot_at(start).remove(0).unanswered_since, None);
		assert!(!diagnosed_at(&t, start + Duration::from_secs(26)));
		// The next search falls inside the grace after that connection.
		search_at(&t, start + Duration::from_secs(30));
		assert!(!diagnosed_at(&t, start + Duration::from_mins(1)));
	}

	#[test]
	fn only_spritz_clients_are_diagnosed() {
		let t = ClientTracker::default();
		let start = Instant::now();
		let tv = ip("192.168.1.50");
		t.record_at(
			start,
			tv,
			Stage::Browsed,
			Some("SEC_HHP_[TV] Samsung/1.0"),
			None,
		);
		let search = start + Duration::from_mins(10);
		t.record_at(search, tv, Stage::Searched, Some("Samsung/1.0"), None);
		let now = search + Duration::from_secs(20);
		let r = t.snapshot_at(now).remove(0);
		assert!(r.unanswered_since.is_some());
		assert_eq!(diagnosis(&r, now, 8080), None);
	}

	fn player(instance: &str) -> Announcement {
		Announcement {
			instance: instance.into(),
			product: "SpritzPlayer/1.2".into(),
			platform: "tvOS 26.0".into(),
			model: "Apple TV".into(),
		}
	}

	#[test]
	fn an_announcement_lists_a_spritz_player() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.announce_at(now, ip("192.168.1.40"), &player("Living Room"));
		let r = t.snapshot_at(now).remove(0);
		assert_eq!(r.announced, Some(now));
		assert!(r.agent.spritz);
		assert_eq!(r.label(), "Living Room (tvOS 26.0)");
		assert!(!r.http);
	}

	#[test]
	fn an_instance_named_after_the_model_is_not_a_device_name() {
		let t = ClientTracker::default();
		let now = Instant::now();
		t.announce_at(now, ip("192.168.1.40"), &player("Apple TV"));
		assert_eq!(t.snapshot_at(now)[0].device_name, "");
	}

	#[test]
	fn non_spritz_announcements_are_ignored() {
		let t = ClientTracker::default();
		let now = Instant::now();
		let mut a = player("Speaker");
		a.product = "Sonos/1.0".into();
		t.announce_at(now, ip("192.168.1.70"), &a);
		assert!(t.snapshot_at(now).is_empty());
	}

	#[test]
	fn a_repeated_announcement_keeps_the_first_time_and_withdraw_clears_it() {
		let t = ClientTracker::default();
		let start = Instant::now();
		t.announce_at(start, ip("192.168.1.40"), &player("Den"));
		t.announce_at(
			start + Duration::from_secs(30),
			ip("192.168.1.40"),
			&player("Den"),
		);
		assert_eq!(t.snapshot_at(start)[0].announced, Some(start));
		t.withdraw(ip("::ffff:192.168.1.40"));
		assert_eq!(t.snapshot_at(start)[0].announced, None);
	}

	#[test]
	fn an_announced_player_that_never_connects_is_diagnosed() {
		let t = ClientTracker::default();
		let start = Instant::now();
		t.announce_at(start, ip("192.168.1.40"), &player("Den"));
		let r = t.snapshot_at(start).remove(0);
		assert_eq!(
			diagnosis(
				&r,
				(start + STUCK_AFTER)
					.checked_sub(Duration::from_secs(1))
					.unwrap(),
				8080
			),
			None
		);
		assert_eq!(
			diagnosis(&r, start + STUCK_AFTER, 8080).as_deref(),
			Some(
				"Den (tvOS 26.0) is on the network but has not connected to this server. Check the firewall on this Mac (port 8080)."
			)
		);
	}

	#[test]
	fn an_announced_player_that_connected_is_fine_for_hours() {
		let t = ClientTracker::default();
		let start = Instant::now();
		t.announce_at(start, ip("192.168.1.40"), &player("Den"));
		t.record_at(
			start + Duration::from_secs(2),
			ip("192.168.1.40"),
			Stage::Browsed,
			None,
			None,
		);
		let r = t.snapshot_at(start).remove(0);
		assert_eq!(diagnosis(&r, start + Duration::from_hours(3), 8080), None);
	}

	#[test]
	fn a_withdrawn_player_is_not_diagnosed() {
		let t = ClientTracker::default();
		let start = Instant::now();
		t.announce_at(start, ip("192.168.1.40"), &player("Den"));
		t.withdraw(ip("192.168.1.40"));
		let r = t.snapshot_at(start).remove(0);
		assert_eq!(diagnosis(&r, start + Duration::from_mins(1), 8080), None);
	}

	#[test]
	fn the_search_rule_wins_when_both_apply() {
		let t = ClientTracker::default();
		let start = Instant::now();
		t.announce_at(start, ip("192.168.1.40"), &player("Den"));
		t.record_at(
			start,
			ip("192.168.1.40"),
			Stage::Searched,
			Some(PLAYER_SSDP),
			None,
		);
		let r = t.snapshot_at(start).remove(0);
		assert!(
			diagnosis(&r, start + STUCK_AFTER, 8080)
				.unwrap()
				.contains("searched for this server")
		);
	}
}
