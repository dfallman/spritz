//! Bonjour: the `_spritz._tcp` advertisement, and browsing for Spritz
//! players that advertise `_spritz-player._tcp`.
//!
//! macOS goes through the system mDNSResponder (`dns_sd`), so the Bonjour
//! Sleep Proxy keeps a sleeping Mac visible. Elsewhere the pure-Rust
//! `mdns-sd` responder is used.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// DNS-SD service type for spritz protocol v1.
pub const SERVICE_TYPE: &str = "_spritz._tcp";

/// TXT pairs for protocol v1.
#[must_use]
pub fn txt_pairs(udn: &str) -> Vec<(&'static str, String)> {
	vec![
		("proto", "1".to_string()),
		("uuid", udn.to_string()),
		("desc", "/upnp/description.xml".to_string()),
	]
}

/// DNS-SD TXT wire format: each entry is one length byte plus `key=value`.
/// Entries longer than 255 bytes are skipped whole.
#[must_use]
pub fn txt_record(pairs: &[(&'static str, String)]) -> Vec<u8> {
	let mut out = Vec::new();
	for (key, value) in pairs {
		let entry = format!("{key}={value}");
		let Ok(len) = u8::try_from(entry.len()) else {
			continue;
		};
		out.push(len);
		out.extend_from_slice(entry.as_bytes());
	}
	out
}

/// DNS-SD service type Spritz players advertise (spec §3.6).
pub const PLAYER_SERVICE_TYPE: &str = "_spritz-player._tcp";

/// Decode DNS-SD TXT wire format. A key without `=` has an empty value;
/// a zero-length or truncated entry ends the record; bytes are decoded
/// lossily.
#[must_use]
pub fn parse_txt(bytes: &[u8]) -> Vec<(String, String)> {
	let mut out = Vec::new();
	let mut rest = bytes;
	while let Some((&len, tail)) = rest.split_first() {
		let len = usize::from(len);
		if len == 0 || len > tail.len() {
			break;
		}
		let entry = String::from_utf8_lossy(&tail[..len]);
		let (k, v) = entry.split_once('=').unwrap_or((&entry, ""));
		out.push((k.to_string(), v.to_string()));
		rest = &tail[len..];
	}
	out
}

/// The tracker's view of one player advertisement. The tracker sanitises
/// every field.
#[must_use]
pub fn announcement(instance: &str, txt: &[(String, String)]) -> dlna::clients::Announcement {
	let get = |key: &str| {
		txt.iter()
			.find(|(k, _)| k == key)
			.map(|(_, v)| v.clone())
			.unwrap_or_default()
	};
	dlna::clients::Announcement {
		instance: instance.to_string(),
		product: get("product"),
		platform: get("platform"),
		model: get("model"),
	}
}

/// Which IPv4 address each advertised player was announced under, so that a
/// removal withdraws exactly that address.
#[derive(Default)]
struct Announced(HashMap<String, IpAddr>);

impl Announced {
	/// Records `instance` at `ip`. Returns the address to withdraw when the
	/// instance moved from one no other instance still uses.
	fn insert(&mut self, instance: &str, ip: IpAddr) -> Option<IpAddr> {
		let old = self.0.insert(instance.to_string(), ip)?;
		(old != ip && !self.holds(old)).then_some(old)
	}

	/// Forgets `instance`. Returns its address unless another instance still
	/// uses it.
	fn remove(&mut self, instance: &str) -> Option<IpAddr> {
		let ip = self.0.remove(instance)?;
		(!self.holds(ip)).then_some(ip)
	}

	fn holds(&self, ip: IpAddr) -> bool {
		self.0.values().any(|v| *v == ip)
	}
}

/// Keeps the player browser running. Dropping it stops browsing.
///
/// On macOS the drop waits for the browser's private dispatch queue, so it
/// must not happen on that queue; nothing outside this module runs there.
pub struct BrowseGuard {
	_browser: browse::Browser,
}

/// Browse `_spritz-player._tcp` and feed appearances and removals into
/// `tracker`. Failure is not fatal for the caller: log and carry on.
///
/// Players are keyed by their IPv4 address, the one their HTTP requests
/// arrive from; an IPv6 link-local key would never match them.
///
/// # Errors
/// When the responder refuses to start browsing.
pub fn browse_players(tracker: dlna::clients::ClientTracker) -> anyhow::Result<BrowseGuard> {
	Ok(BrowseGuard {
		_browser: browse::start(tracker)?,
	})
}

/// Longest DNS-SD instance name, in bytes (one DNS label).
pub const MAX_INSTANCE_BYTES: usize = 63;

/// `friendly` cut to at most [`MAX_INSTANCE_BYTES`] bytes without splitting a
/// character. The friendly name is capped at 64 characters, which can be
/// far more than 63 bytes of UTF-8.
#[must_use]
pub fn instance_name(friendly: &str) -> String {
	let mut end = friendly.len().min(MAX_INSTANCE_BYTES);
	while !friendly.is_char_boundary(end) {
		end -= 1;
	}
	friendly[..end].to_string()
}

/// Where a registration stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BonjourStatus {
	/// Submitted and not confirmed yet: the responder has not answered, or it
	/// withdrew the name after a conflict and the renamed one is still pending.
	Pending,
	/// Registered under this instance name (the responder may have renamed it).
	Registered(String),
	/// The responder reported an error after the call returned.
	Failed(String),
}

/// A cloneable view of a registration's status, for code that must not own
/// the guard (the check monitor, the CLI's status line).
///
/// The status can change after [`settle`]: mDNSResponder may rename an
/// existing registration on a later conflict, so read it live rather than once.
#[derive(Clone)]
pub struct BonjourWatch(Arc<Mutex<BonjourStatus>>);

impl BonjourWatch {
	#[must_use]
	pub fn status(&self) -> BonjourStatus {
		self.0
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}
}

/// Wait until the status leaves `Pending`, at most `timeout`.
pub async fn settle(watch: &BonjourWatch, timeout: Duration) -> BonjourStatus {
	let deadline = tokio::time::Instant::now() + timeout;
	loop {
		let status = watch.status();
		if status != BonjourStatus::Pending || tokio::time::Instant::now() >= deadline {
			return status;
		}
		tokio::time::sleep(Duration::from_millis(25)).await;
	}
}

/// Keeps the service registered. Dropping it withdraws the advertisement.
pub struct BonjourGuard {
	status: Arc<Mutex<BonjourStatus>>,
	_registration: imp::Registration,
}

impl BonjourGuard {
	/// The responder's latest answer: pending, the registered name, or an error.
	/// It can change at any time (a later conflict renames the service), so
	/// read it live rather than once.
	#[must_use]
	pub fn status(&self) -> BonjourStatus {
		self.watch().status()
	}

	#[must_use]
	pub fn watch(&self) -> BonjourWatch {
		BonjourWatch(Arc::clone(&self.status))
	}
}

/// Advertise `name` as `_spritz._tcp` on `port`.
///
/// The name is cut to one DNS label by [`instance_name`]. `hostname` is the
/// `.local` name to advertise on platforms without a system responder; macOS
/// ignores it and uses the system's own.
///
/// # Errors
/// When the responder refuses the registration synchronously. Later
/// failures show up as [`BonjourStatus::Failed`].
pub fn advertise(
	name: &str,
	port: u16,
	udn: &str,
	hostname: Option<&str>,
) -> anyhow::Result<BonjourGuard> {
	let status = Arc::new(Mutex::new(BonjourStatus::Pending));
	let registration = imp::register(
		SERVICE_TYPE,
		&instance_name(name),
		port,
		&txt_pairs(udn),
		hostname,
		Arc::clone(&status),
	)?;
	Ok(BonjourGuard {
		status,
		_registration: registration,
	})
}

#[cfg(target_os = "macos")]
pub(crate) mod imp {
	use super::BonjourStatus;
	use std::ffi::{CStr, CString, c_char, c_void};
	use std::ptr;
	use std::sync::{Arc, Mutex, PoisonError};

	pub type DnsServiceRef = *mut c_void;
	pub type DispatchQueue = *mut c_void;

	type RegisterReply = extern "C" fn(
		sd_ref: DnsServiceRef,
		flags: u32,
		error_code: i32,
		name: *const c_char,
		regtype: *const c_char,
		domain: *const c_char,
		context: *mut c_void,
	);

	// dns_sd.h and dispatch/queue.h, both in libSystem.
	unsafe extern "C" {
		fn DNSServiceRegister(
			sd_ref: *mut DnsServiceRef,
			flags: u32,
			interface_index: u32,
			name: *const c_char,
			regtype: *const c_char,
			domain: *const c_char,
			host: *const c_char,
			port_network_order: u16,
			txt_len: u16,
			txt_record: *const c_void,
			callback: Option<RegisterReply>,
			context: *mut c_void,
		) -> i32;
		pub fn DNSServiceSetDispatchQueue(sd_ref: DnsServiceRef, queue: DispatchQueue) -> i32;
		pub fn DNSServiceRefDeallocate(sd_ref: DnsServiceRef);
		pub fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
		pub fn dispatch_sync_f(
			queue: DispatchQueue,
			context: *mut c_void,
			work: extern "C" fn(*mut c_void),
		);
		pub fn dispatch_release(object: *mut c_void);
	}

	/// `kDNSServiceFlagsAdd`: set when the name is registered, clear when the
	/// responder has withdrawn it (a conflict; a renamed Add follows). Browse
	/// and address replies use it the same way: set for an appearance, clear
	/// for a removal.
	pub const FLAGS_ADD: u32 = 0x2;

	/// What one register callback means for the status.
	pub fn status_from_callback(flags: u32, error: i32, name: Option<&str>) -> BonjourStatus {
		if error != 0 {
			return BonjourStatus::Failed(format!("mDNSResponder reported error {error}"));
		}
		if flags & FLAGS_ADD == 0 {
			return BonjourStatus::Pending;
		}
		name.map_or_else(
			|| BonjourStatus::Failed("mDNSResponder registered without a name".into()),
			|name| BonjourStatus::Registered(name.to_string()),
		)
	}

	/// Runs on the registration's queue whenever the responder answers.
	extern "C" fn on_register(
		_sd_ref: DnsServiceRef,
		flags: u32,
		error_code: i32,
		name: *const c_char,
		_regtype: *const c_char,
		_domain: *const c_char,
		context: *mut c_void,
	) {
		// SAFETY: `context` is the `Arc<Mutex<BonjourStatus>>` leaked in
		// `register`; it is released only after the ref is deallocated on
		// this same queue, so it is alive for every callback.
		let status = unsafe { &*context.cast_const().cast::<Mutex<BonjourStatus>>() };
		// dns_sd leaves the other arguments undefined when `error_code` is set,
		// so the name is read only on success.
		let name = if error_code == 0 && !name.is_null() {
			// SAFETY: on success dns_sd passes a non-NULL, NUL-terminated name
			// that is valid for the duration of this call.
			Some(unsafe { CStr::from_ptr(name) }.to_string_lossy())
		} else {
			None
		};
		let next = status_from_callback(flags, error_code, name.as_deref());
		*status.lock().unwrap_or_else(PoisonError::into_inner) = next;
	}

	extern "C" fn deallocate_on_queue(sd_ref: *mut c_void) {
		// SAFETY: called once, from `Drop`, on the ref's own queue.
		unsafe { DNSServiceRefDeallocate(sd_ref) }
	}

	pub struct Registration {
		sd_ref: DnsServiceRef,
		queue: DispatchQueue,
		context: *const Mutex<BonjourStatus>,
	}

	// SAFETY: once registered, every access to the DNSServiceRef happens on
	// the private serial queue: dns_sd delivers callbacks there, and `Drop`
	// deallocates there through `dispatch_sync_f`. The struct itself only
	// carries the pointers, so moving or sharing it across threads is sound;
	// this is what makes `BonjourGuard` `Send`.
	unsafe impl Send for Registration {}
	unsafe impl Sync for Registration {}

	impl Drop for Registration {
		fn drop(&mut self) {
			// SAFETY: `sd_ref` came from a successful DNSServiceRegister and is
			// scheduled on `queue`; after the synchronous deallocation no more
			// callbacks run, so releasing the context and the queue is safe.
			unsafe {
				dispatch_sync_f(self.queue, self.sd_ref, deallocate_on_queue);
				dispatch_release(self.queue);
				drop(Arc::from_raw(self.context));
			}
		}
	}

	pub fn new_queue() -> DispatchQueue {
		// SAFETY: a static label and NULL attributes (a serial queue).
		unsafe { dispatch_queue_create(c"org.spritz.bonjour".as_ptr(), ptr::null()) }
	}

	pub fn register(
		regtype: &str,
		name: &str,
		port: u16,
		pairs: &[(&'static str, String)],
		_hostname: Option<&str>,
		status: Arc<Mutex<BonjourStatus>>,
	) -> anyhow::Result<Registration> {
		let name = CString::new(name)?;
		let regtype = CString::new(regtype)?;
		let txt = super::txt_record(pairs);
		let txt_len = u16::try_from(txt.len())?;
		let context = Arc::into_raw(status);
		let queue = new_queue();
		let mut sd_ref: DnsServiceRef = ptr::null_mut();
		// SAFETY: all pointers are valid for the duration of the call; dns_sd
		// copies what it needs. Flags 0 allow auto-rename on a conflict.
		let err = unsafe {
			DNSServiceRegister(
				&raw mut sd_ref,
				0,
				0,
				name.as_ptr(),
				regtype.as_ptr(),
				ptr::null(),
				ptr::null(),
				port.to_be(),
				txt_len,
				txt.as_ptr().cast(),
				Some(on_register),
				context.cast_mut().cast(),
			)
		};
		if err != 0 || sd_ref.is_null() {
			// SAFETY: nothing else holds the context or the queue yet.
			unsafe {
				drop(Arc::from_raw(context));
				dispatch_release(queue);
			}
			anyhow::bail!("DNSServiceRegister failed ({err})");
		}
		// SAFETY: `sd_ref` is valid and not yet scheduled anywhere.
		let err = unsafe { DNSServiceSetDispatchQueue(sd_ref, queue) };
		if err != 0 {
			// SAFETY: the ref is not scheduled, so it can be deallocated here.
			unsafe {
				DNSServiceRefDeallocate(sd_ref);
				drop(Arc::from_raw(context));
				dispatch_release(queue);
			}
			anyhow::bail!("DNSServiceSetDispatchQueue failed ({err})");
		}
		Ok(Registration {
			sd_ref,
			queue,
			context,
		})
	}
}

#[cfg(not(target_os = "macos"))]
pub(crate) mod imp {
	use super::BonjourStatus;
	use mdns_sd::{ServiceDaemon, ServiceInfo};
	use std::sync::{Arc, Mutex, PoisonError};

	pub struct Registration {
		daemon: ServiceDaemon,
		fullname: String,
	}

	impl Drop for Registration {
		fn drop(&mut self) {
			if let Ok(done) = self.daemon.unregister(&self.fullname) {
				let _ = done.recv_timeout(std::time::Duration::from_secs(1));
			}
			let _ = self.daemon.shutdown();
		}
	}

	// Same signature as the macOS `register`, which hands `status` to the
	// dns_sd callback and so needs it by value; `advertise` calls either.
	#[allow(clippy::needless_pass_by_value)]
	pub fn register(
		regtype: &str,
		name: &str,
		port: u16,
		pairs: &[(&'static str, String)],
		hostname: Option<&str>,
		status: Arc<Mutex<BonjourStatus>>,
	) -> anyhow::Result<Registration> {
		let host = hostname.unwrap_or("spritz.local");
		let host = format!("{}.", host.trim_end_matches('.'));
		let props: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (*k, v.as_str())).collect();
		let daemon = ServiceDaemon::new()?;
		let info = ServiceInfo::new(
			&format!("{regtype}.local."),
			name,
			&host,
			"",
			port,
			&props[..],
		)?
		.enable_addr_auto();
		let fullname = info.get_fullname().to_string();
		daemon.register(info)?;
		*status.lock().unwrap_or_else(PoisonError::into_inner) =
			BonjourStatus::Registered(name.to_string());
		Ok(Registration { daemon, fullname })
	}
}

#[cfg(target_os = "macos")]
mod browse {
	use super::Announced;
	use super::imp::{
		DNSServiceRefDeallocate, DNSServiceSetDispatchQueue, DispatchQueue, DnsServiceRef,
		FLAGS_ADD, dispatch_release, dispatch_sync_f, new_queue,
	};
	use dlna::clients::{Announcement, ClientTracker};
	use std::collections::{HashMap, HashSet};
	use std::ffi::{CStr, CString, c_char, c_void};
	use std::net::{IpAddr, Ipv4Addr};
	use std::ptr;
	use std::sync::{Mutex, MutexGuard, PoisonError};

	/// `kDNSServiceProtocol_IPv4`: players are keyed by IPv4 only.
	const PROTOCOL_IPV4: u32 = 0x01;
	/// `AF_INET` on Darwin.
	const AF_INET: u8 = 2;
	/// `sizeof(struct sockaddr_in)`.
	const SOCKADDR_IN_LEN: usize = 16;

	type BrowseReply = extern "C" fn(
		sd_ref: DnsServiceRef,
		flags: u32,
		interface_index: u32,
		error_code: i32,
		name: *const c_char,
		regtype: *const c_char,
		domain: *const c_char,
		context: *mut c_void,
	);
	type ResolveReply = extern "C" fn(
		sd_ref: DnsServiceRef,
		flags: u32,
		interface_index: u32,
		error_code: i32,
		fullname: *const c_char,
		hosttarget: *const c_char,
		port_network_order: u16,
		txt_len: u16,
		txt_record: *const u8,
		context: *mut c_void,
	);
	type AddrInfoReply = extern "C" fn(
		sd_ref: DnsServiceRef,
		flags: u32,
		interface_index: u32,
		error_code: i32,
		hostname: *const c_char,
		address: *const u8,
		ttl: u32,
		context: *mut c_void,
	);

	// dns_sd.h, in libSystem.
	unsafe extern "C" {
		fn DNSServiceBrowse(
			sd_ref: *mut DnsServiceRef,
			flags: u32,
			interface_index: u32,
			regtype: *const c_char,
			domain: *const c_char,
			callback: Option<BrowseReply>,
			context: *mut c_void,
		) -> i32;
		fn DNSServiceResolve(
			sd_ref: *mut DnsServiceRef,
			flags: u32,
			interface_index: u32,
			name: *const c_char,
			regtype: *const c_char,
			domain: *const c_char,
			callback: Option<ResolveReply>,
			context: *mut c_void,
		) -> i32;
		fn DNSServiceGetAddrInfo(
			sd_ref: *mut DnsServiceRef,
			flags: u32,
			interface_index: u32,
			protocol: u32,
			hostname: *const c_char,
			callback: Option<AddrInfoReply>,
			context: *mut c_void,
		) -> i32;
	}

	/// The address a player is keyed under, from a Darwin socket address
	/// (`sa_len`, `sa_family`, then the family's fields): IPv4 only, and never
	/// loopback, which a player on this Mac also answers on but never
	/// connects from.
	pub fn player_ipv4(bytes: &[u8]) -> Option<Ipv4Addr> {
		match bytes {
			[_, AF_INET, _, _, a, b, c, d, ..] => {
				Some(Ipv4Addr::new(*a, *b, *c, *d)).filter(|ip| !ip.is_loopback())
			}
			_ => None,
		}
	}

	/// The lookup a player is waiting on: first its SRV/TXT, then its host's
	/// IPv4 address.
	enum Step {
		Resolving,
		Addressing(Announcement),
	}

	struct Lookup {
		sd_ref: DnsServiceRef,
		step: Step,
	}

	#[derive(Default)]
	struct Player {
		/// Interfaces the browse has reported the instance on. It is gone
		/// once the last one is removed.
		interfaces: HashSet<u32>,
		lookup: Option<Lookup>,
	}

	/// Everything the callbacks share. Every ref uses this as its context and
	/// every callback runs on `queue`, which is serial; the mutexes only give
	/// safe interior mutability.
	struct State {
		tracker: ClientTracker,
		queue: DispatchQueue,
		players: Mutex<HashMap<String, Player>>,
		announced: Mutex<Announced>,
	}

	fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
		m.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Puts a ref just created by a `dns_sd` call that returned `err` on
	/// `queue`, or deallocates it when that call or the scheduling failed.
	///
	/// # Safety
	/// `sd_ref` is NULL or a fresh ref not yet scheduled anywhere.
	unsafe fn schedule(
		err: i32,
		sd_ref: DnsServiceRef,
		queue: DispatchQueue,
	) -> Option<DnsServiceRef> {
		if sd_ref.is_null() {
			return None;
		}
		// SAFETY: per the contract, `sd_ref` is a valid, unscheduled ref.
		if err == 0 && unsafe { DNSServiceSetDispatchQueue(sd_ref, queue) } == 0 {
			return Some(sd_ref);
		}
		// SAFETY: the ref is not scheduled, so it can be deallocated here.
		unsafe { DNSServiceRefDeallocate(sd_ref) };
		None
	}

	/// Deallocates a lookup's ref.
	///
	/// # Safety
	/// Must run on the state's queue, with a ref taken out of a `Lookup`.
	unsafe fn cancel(sd_ref: DnsServiceRef) {
		// SAFETY: a lookup's ref is valid and scheduled on the queue we are
		// on; dns_sd allows deallocating it here, even from inside its own
		// callback, and delivers no callbacks for it afterwards.
		unsafe { DNSServiceRefDeallocate(sd_ref) };
	}

	/// The player whose lookup is `sd_ref`, with its lookup taken out.
	fn take_lookup(state: &State, sd_ref: DnsServiceRef) -> Option<(String, Lookup)> {
		let mut players = lock(&state.players);
		players.iter_mut().find_map(|(name, player)| {
			if player.lookup.as_ref().is_some_and(|l| l.sd_ref == sd_ref) {
				player.lookup.take().map(|l| (name.clone(), l))
			} else {
				None
			}
		})
	}

	// The lock spans the check and the start of the resolve: the new lookup
	// is stored in the entry borrowed from it.
	#[allow(clippy::significant_drop_tightening)]
	extern "C" fn on_browse(
		_sd_ref: DnsServiceRef,
		flags: u32,
		interface_index: u32,
		error_code: i32,
		name: *const c_char,
		regtype: *const c_char,
		domain: *const c_char,
		context: *mut c_void,
	) {
		// dns_sd leaves the other arguments undefined when `error_code` is
		// set, so nothing else is read then.
		if error_code != 0 {
			tracing::warn!("Bonjour browse: mDNSResponder reported error {error_code}");
			return;
		}
		if name.is_null() || regtype.is_null() || domain.is_null() {
			return;
		}
		// SAFETY: every ref's context is the `State` boxed in `start`; it is
		// freed only after every ref is deallocated on this queue.
		let state = unsafe { &*context.cast_const().cast::<State>() };
		// SAFETY: on success dns_sd passes a NUL-terminated name, valid for
		// the duration of this call.
		let instance = unsafe { CStr::from_ptr(name) }
			.to_string_lossy()
			.into_owned();
		let mut players = lock(&state.players);
		if flags & FLAGS_ADD == 0 {
			let Some(player) = players.get_mut(&instance) else {
				return;
			};
			player.interfaces.remove(&interface_index);
			if !player.interfaces.is_empty() {
				return;
			}
			if let Some(lookup) = players.remove(&instance).and_then(|p| p.lookup) {
				// SAFETY: we are on the queue.
				unsafe { cancel(lookup.sd_ref) };
			}
			drop(players);
			let gone = lock(&state.announced).remove(&instance);
			if let Some(ip) = gone {
				state.tracker.withdraw(ip);
			}
			return;
		}
		let player = players.entry(instance).or_default();
		let known = !player.interfaces.is_empty();
		player.interfaces.insert(interface_index);
		if known {
			return;
		}
		let mut sd_ref: DnsServiceRef = ptr::null_mut();
		// SAFETY: the strings come from dns_sd and are valid for this call;
		// dns_sd copies them. `context` stays valid as described above.
		let err = unsafe {
			DNSServiceResolve(
				&raw mut sd_ref,
				0,
				interface_index,
				name,
				regtype,
				domain,
				Some(on_resolve),
				context,
			)
		};
		// SAFETY: `sd_ref` was just created by `DNSServiceResolve`.
		player.lookup = unsafe { schedule(err, sd_ref, state.queue) }.map(|sd_ref| Lookup {
			sd_ref,
			step: Step::Resolving,
		});
	}

	extern "C" fn on_resolve(
		sd_ref: DnsServiceRef,
		_flags: u32,
		_interface_index: u32,
		error_code: i32,
		_fullname: *const c_char,
		hosttarget: *const c_char,
		_port: u16,
		txt_len: u16,
		txt_record: *const u8,
		context: *mut c_void,
	) {
		// SAFETY: as in `on_browse`.
		let state = unsafe { &*context.cast_const().cast::<State>() };
		let Some((instance, lookup)) = take_lookup(state, sd_ref) else {
			return;
		};
		// The callback's arguments are copied before its ref is deallocated.
		let copied = (error_code == 0 && !hosttarget.is_null()).then(|| {
			// SAFETY: on success dns_sd passes a NUL-terminated host name and
			// `txt_len` bytes of TXT, both valid for this call.
			let host = unsafe { CStr::from_ptr(hosttarget) }.to_owned();
			let txt = if txt_record.is_null() {
				Vec::new()
			} else {
				// SAFETY: as above, `txt_record` holds `txt_len` bytes.
				let bytes = unsafe { std::slice::from_raw_parts(txt_record, usize::from(txt_len)) };
				super::parse_txt(bytes)
			};
			(host, super::announcement(&instance, &txt))
		});
		// SAFETY: we are on the queue.
		unsafe { cancel(lookup.sd_ref) };
		let Some((host, announcement)) = copied else {
			return;
		};
		let mut addr_ref: DnsServiceRef = ptr::null_mut();
		// Any interface: the browse reports a player on this Mac on loopback
		// first, where its host only has 127.0.0.1.
		// SAFETY: `host` is a valid C string for the call; dns_sd copies it.
		// `context` stays valid as described in `on_browse`.
		let err = unsafe {
			DNSServiceGetAddrInfo(
				&raw mut addr_ref,
				0,
				0,
				PROTOCOL_IPV4,
				host.as_ptr(),
				Some(on_address),
				context,
			)
		};
		// SAFETY: `addr_ref` was just created by `DNSServiceGetAddrInfo`.
		let lookup = unsafe { schedule(err, addr_ref, state.queue) }.map(|sd_ref| Lookup {
			sd_ref,
			step: Step::Addressing(announcement),
		});
		// Only callbacks on this queue remove players, so the player whose
		// lookup was just taken is still listed.
		let mut players = lock(&state.players);
		if let Some(player) = players.get_mut(&instance) {
			player.lookup = lookup;
		}
	}

	extern "C" fn on_address(
		sd_ref: DnsServiceRef,
		flags: u32,
		_interface_index: u32,
		error_code: i32,
		_hostname: *const c_char,
		address: *const u8,
		_ttl: u32,
		context: *mut c_void,
	) {
		// SAFETY: as in `on_browse`.
		let state = unsafe { &*context.cast_const().cast::<State>() };
		let ip = if error_code == 0 && flags & FLAGS_ADD != 0 && !address.is_null() {
			// SAFETY: on success with Add set, dns_sd passes a valid socket
			// address, and every socket address starts with `sa_len` and
			// `sa_family`. Only an `AF_INET` one (a 16-byte `sockaddr_in`) is
			// read in full.
			unsafe {
				(*address.add(1) == AF_INET)
					.then(|| std::slice::from_raw_parts(address, SOCKADDR_IN_LEN))
					.and_then(player_ipv4)
			}
		} else {
			None
		};
		// Neither an error nor an IPv6 or loopback answer yields an address;
		// on an error the lookup is dropped (a later appearance retries),
		// otherwise it keeps waiting for a usable answer.
		if error_code == 0 && ip.is_none() {
			return;
		}
		let Some((instance, lookup)) = take_lookup(state, sd_ref) else {
			return;
		};
		// SAFETY: we are on the queue.
		unsafe { cancel(lookup.sd_ref) };
		let (Some(ip), Step::Addressing(announcement)) = (ip, lookup.step) else {
			return;
		};
		let ip = IpAddr::V4(ip);
		let moved = lock(&state.announced).insert(&instance, ip);
		if let Some(old) = moved {
			state.tracker.withdraw(old);
		}
		state.tracker.announce(ip, &announcement);
	}

	pub struct Browser {
		sd_ref: DnsServiceRef,
		state: *const State,
	}

	// SAFETY: once browsing, every access to the refs and to `State` happens on
	// the private serial queue: dns_sd delivers callbacks there, and `Drop`
	// tears down there through `dispatch_sync_f`. The struct itself only
	// carries the pointers and has no `&self` methods, so moving or sharing it
	// across threads is sound; this is what makes `BrowseGuard` `Send`.
	unsafe impl Send for Browser {}
	unsafe impl Sync for Browser {}

	extern "C" fn teardown(context: *mut c_void) {
		// SAFETY: runs once, on the queue, through `dispatch_sync_f` from
		// `Drop`, with the `Browser` being dropped; no callback runs meanwhile.
		let browser = unsafe { &*context.cast_const().cast::<Browser>() };
		// SAFETY: `state` is alive until `Drop` frees it after this returns.
		let state = unsafe { &*browser.state };
		// SAFETY: the browse ref is valid and scheduled on this queue.
		unsafe { DNSServiceRefDeallocate(browser.sd_ref) };
		for lookup in lock(&state.players).drain().filter_map(|(_, p)| p.lookup) {
			// SAFETY: we are on the queue.
			unsafe { cancel(lookup.sd_ref) };
		}
	}

	impl Drop for Browser {
		fn drop(&mut self) {
			// SAFETY: `state` came from `Box::into_raw` in `start`. After the
			// synchronous teardown on the queue no ref is left, so no callback
			// can use the state or the queue, and both can be released. This
			// would deadlock if run on the queue itself, which only runs this
			// module's callbacks.
			unsafe {
				let queue = (*self.state).queue;
				dispatch_sync_f(queue, (&raw mut *self).cast(), teardown);
				drop(Box::from_raw(self.state.cast_mut()));
				dispatch_release(queue);
			}
		}
	}

	pub fn start(tracker: ClientTracker) -> anyhow::Result<Browser> {
		let regtype = CString::new(super::PLAYER_SERVICE_TYPE)?;
		let queue = new_queue();
		let state = Box::into_raw(Box::new(State {
			tracker,
			queue,
			players: Mutex::default(),
			announced: Mutex::default(),
		}));
		let mut sd_ref: DnsServiceRef = ptr::null_mut();
		// SAFETY: all pointers are valid for the call; dns_sd copies the
		// strings. `state` outlives the ref (see `Drop`). The NULL domain
		// browses the default domains.
		let err = unsafe {
			DNSServiceBrowse(
				&raw mut sd_ref,
				0,
				0,
				regtype.as_ptr(),
				ptr::null(),
				Some(on_browse),
				state.cast(),
			)
		};
		// SAFETY: `sd_ref` was just created by `DNSServiceBrowse`.
		let Some(sd_ref) = (unsafe { schedule(err, sd_ref, queue) }) else {
			// SAFETY: no ref uses the state or the queue.
			unsafe {
				drop(Box::from_raw(state));
				dispatch_release(queue);
			}
			anyhow::bail!("DNSServiceBrowse failed ({err})");
		};
		Ok(Browser { sd_ref, state })
	}
}

#[cfg(not(target_os = "macos"))]
mod browse {
	use super::Announced;
	use dlna::clients::ClientTracker;
	use mdns_sd::{ServiceDaemon, ServiceEvent};
	use std::net::IpAddr;

	pub struct Browser {
		daemon: ServiceDaemon,
		thread: Option<std::thread::JoinHandle<()>>,
	}

	impl Drop for Browser {
		fn drop(&mut self) {
			// Shutting down closes the event channel, which ends the thread.
			let _ = self.daemon.shutdown();
			if let Some(thread) = self.thread.take() {
				let _ = thread.join();
			}
		}
	}

	fn instance_of(fullname: &str) -> String {
		let suffix = format!(".{}.local.", super::PLAYER_SERVICE_TYPE);
		fullname
			.strip_suffix(&suffix)
			.unwrap_or(fullname)
			.to_string()
	}

	pub fn start(tracker: ClientTracker) -> anyhow::Result<Browser> {
		let daemon = ServiceDaemon::new()?;
		let events = daemon.browse(&format!("{}.local.", super::PLAYER_SERVICE_TYPE))?;
		let thread = std::thread::spawn(move || {
			let mut announced = Announced::default();
			while let Ok(event) = events.recv() {
				match event {
					ServiceEvent::ServiceResolved(info) => {
						// IPv4 only, never loopback; the lowest address keeps the
						// choice stable.
						let Some(ip) = info
							.get_addresses_v4()
							.into_iter()
							.filter(|ip| !ip.is_loopback())
							.min()
						else {
							continue;
						};
						let ip = IpAddr::V4(ip);
						let instance = instance_of(info.get_fullname());
						let txt: Vec<(String, String)> = info
							.get_properties()
							.iter()
							.map(|p| (p.key().to_string(), p.val_str().to_string()))
							.collect();
						if let Some(old) = announced.insert(&instance, ip) {
							tracker.withdraw(old);
						}
						tracker.announce(ip, &super::announcement(&instance, &txt));
					}
					ServiceEvent::ServiceRemoved(_, fullname) => {
						if let Some(ip) = announced.remove(&instance_of(&fullname)) {
							tracker.withdraw(ip);
						}
					}
					_ => {}
				}
			}
		});
		Ok(Browser {
			daemon,
			thread: Some(thread),
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn txt_pairs_carry_protocol_uuid_and_description() {
		assert_eq!(
			txt_pairs("uuid:abc"),
			vec![
				("proto", "1".to_string()),
				("uuid", "uuid:abc".to_string()),
				("desc", "/upnp/description.xml".to_string()),
			]
		);
	}

	#[test]
	fn txt_record_is_length_prefixed() {
		let bytes = txt_record(&[("proto", "1".into()), ("uuid", "u".into())]);
		assert_eq!(bytes, b"\x07proto=1\x06uuid=u".to_vec());
	}

	#[test]
	fn txt_record_skips_entries_over_255_bytes() {
		let long = "é".repeat(200); // 400 bytes of UTF-8
		let bytes = txt_record(&[("uuid", long), ("proto", "1".into())]);
		assert_eq!(bytes, b"\x07proto=1".to_vec());
	}

	#[test]
	fn instance_names_fit_one_dns_label() {
		assert_eq!(instance_name("Daniel's Mac mini"), "Daniel's Mac mini");
		let ascii = "a".repeat(64);
		assert_eq!(instance_name(&ascii), "a".repeat(63));
		let nordic = "Å".repeat(64); // 128 bytes
		let cut = instance_name(&nordic);
		assert!(cut.len() <= MAX_INSTANCE_BYTES, "{}", cut.len());
		assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
		assert_eq!(cut, "Å".repeat(31));
		let wide = "é".repeat(64); // 128 bytes
		let cut = instance_name(&wide);
		assert!(cut.len() <= MAX_INSTANCE_BYTES, "{}", cut.len());
		assert_eq!(cut, "é".repeat(31)); // 62 bytes; a 32nd would make 64
		let emoji = "🍹".repeat(64); // 4 bytes each
		assert_eq!(instance_name(&emoji), "🍹".repeat(15));
	}

	#[tokio::test]
	async fn settle_returns_once_registered() {
		let shared = Arc::new(Mutex::new(BonjourStatus::Pending));
		let watch = BonjourWatch(Arc::clone(&shared));
		let setter = tokio::spawn(async move {
			tokio::time::sleep(Duration::from_millis(50)).await;
			*shared.lock().unwrap() = BonjourStatus::Registered("Mini (2)".into());
		});
		let status = settle(&watch, Duration::from_secs(2)).await;
		assert_eq!(status, BonjourStatus::Registered("Mini (2)".into()));
		setter.await.unwrap();
	}

	#[tokio::test]
	async fn settle_gives_up_while_pending() {
		let watch = BonjourWatch(Arc::new(Mutex::new(BonjourStatus::Pending)));
		assert_eq!(
			settle(&watch, Duration::from_millis(60)).await,
			BonjourStatus::Pending
		);
	}

	#[test]
	fn the_guard_can_move_between_threads() {
		fn assert_send<T: Send>() {}
		assert_send::<BonjourGuard>();
		assert_send::<BonjourWatch>();
		assert_send::<BrowseGuard>();
	}

	#[test]
	fn parse_txt_reads_length_prefixed_pairs() {
		let bytes =
			b"\x07proto=1\x18product=SpritzPlayer/1.2\x12platform=tvOS 26.0\x0emodel=Apple TV";
		assert_eq!(
			parse_txt(bytes),
			vec![
				("proto".to_string(), "1".to_string()),
				("product".to_string(), "SpritzPlayer/1.2".to_string()),
				("platform".to_string(), "tvOS 26.0".to_string()),
				("model".to_string(), "Apple TV".to_string()),
			]
		);
	}

	#[test]
	fn parse_txt_survives_garbage() {
		assert!(parse_txt(b"").is_empty());
		assert!(parse_txt(b"\x00").is_empty());
		assert_eq!(parse_txt(b"\x0atruncated"), vec![]); // claims 10 bytes, has 9
		assert_eq!(
			parse_txt(b"\x04flag"),
			vec![("flag".to_string(), String::new())]
		);
		assert_eq!(parse_txt(b"\x04\xff\xfe=x").len(), 1);
	}

	#[test]
	fn announcement_takes_the_known_keys() {
		let txt = vec![
			("proto".to_string(), "1".to_string()),
			("product".to_string(), "SpritzPlayer/1.2".to_string()),
			("platform".to_string(), "tvOS 26.0".to_string()),
			("model".to_string(), "Apple TV".to_string()),
			("extra".to_string(), "ignored".to_string()),
		];
		let a = announcement("Living Room", &txt);
		assert_eq!(a.instance, "Living Room");
		assert_eq!(a.product, "SpritzPlayer/1.2");
		assert_eq!(a.platform, "tvOS 26.0");
		assert_eq!(a.model, "Apple TV");
	}

	#[test]
	fn a_removed_player_withdraws_exactly_its_address() {
		let tv: std::net::IpAddr = "192.168.1.40".parse().unwrap();
		let phone: std::net::IpAddr = "192.168.1.41".parse().unwrap();
		let mut announced = Announced::default();
		assert_eq!(announced.insert("Living Room", tv), None);
		assert_eq!(announced.insert("Phone", phone), None);
		assert_eq!(announced.remove("Living Room"), Some(tv));
		assert_eq!(announced.remove("Living Room"), None);
		// A player that moved withdraws its old address.
		assert_eq!(announced.insert("Phone", tv), Some(phone));
		// Two instances on one address: only the last one out withdraws it.
		assert_eq!(announced.insert("Bedroom", tv), None);
		assert_eq!(announced.remove("Phone"), None);
		assert_eq!(announced.remove("Bedroom"), Some(tv));
	}

	#[cfg(target_os = "macos")]
	#[test]
	fn players_are_keyed_by_a_non_loopback_ipv4_address() {
		// sockaddr_in: sin_len 16, AF_INET, port, 192.168.1.40, zero padding.
		let v4 = [16, 2, 0, 0, 192, 168, 1, 40, 0, 0, 0, 0, 0, 0, 0, 0];
		assert_eq!(
			browse::player_ipv4(&v4),
			Some(std::net::Ipv4Addr::new(192, 168, 1, 40))
		);
		let loopback = [16, 2, 0, 0, 127, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
		assert_eq!(browse::player_ipv4(&loopback), None);
		// sockaddr_in6 for fe80::1 (spike S3 saw the Apple TV here too).
		let mut v6 = [0_u8; 28];
		v6[0] = 28;
		v6[1] = 30;
		v6[8] = 0xfe;
		v6[9] = 0x80;
		v6[23] = 1;
		assert_eq!(browse::player_ipv4(&v6), None);
		assert_eq!(browse::player_ipv4(&v4[..4]), None);
	}

	#[cfg(target_os = "macos")]
	#[tokio::test]
	#[ignore = "needs mDNSResponder; run by hand"]
	async fn browsing_sees_a_local_player_advertisement() {
		let tracker = dlna::clients::ClientTracker::default();
		let _browse = browse_players(tracker.clone()).unwrap();
		let pairs = [
			("proto", "1".to_string()),
			("product", "SpritzPlayer/9.9".to_string()),
			("platform", "tvOS 26.0".to_string()),
			("model", "Apple TV".to_string()),
		];
		let status = Arc::new(Mutex::new(BonjourStatus::Pending));
		let player =
			imp::register(PLAYER_SERVICE_TYPE, "Browse Test", 9, &pairs, None, status).unwrap();
		let deadline = std::time::Instant::now() + Duration::from_secs(10);
		let ip = loop {
			if let Some(r) = tracker
				.snapshot()
				.into_iter()
				.find(|r| r.device_name == "Browse Test" && r.announced.is_some())
			{
				break r.ip;
			}
			assert!(
				std::time::Instant::now() < deadline,
				"no announcement: {:?}",
				tracker.snapshot()
			);
			tokio::time::sleep(Duration::from_millis(100)).await;
		};
		// This Mac's LAN address, not loopback or IPv6 link-local.
		assert!(ip.is_ipv4() && !ip.is_loopback(), "{ip}");
		drop(player);
		let deadline = std::time::Instant::now() + Duration::from_secs(10);
		while tracker
			.snapshot()
			.iter()
			.any(|r| r.ip == ip && r.announced.is_some())
		{
			assert!(std::time::Instant::now() < deadline, "not withdrawn");
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}

	#[cfg(target_os = "macos")]
	#[tokio::test]
	#[ignore = "needs mDNSResponder; run by hand in spike S1"]
	async fn registers_and_reports_the_name() {
		let guard = advertise("Spritz S1 Test", 8097, "uuid:s1", None).unwrap();
		let status = settle(&guard.watch(), Duration::from_secs(3)).await;
		assert_eq!(status, BonjourStatus::Registered("Spritz S1 Test".into()));
	}

	#[cfg(target_os = "macos")]
	#[tokio::test]
	#[ignore = "needs mDNSResponder; run by hand in spike S1"]
	async fn a_long_multibyte_name_registers_truncated() {
		let name = "Vardagsrummets mediaserver på övervåningen, ändå längre än så här".to_string();
		let name: String = name.chars().cycle().take(64).collect();
		assert!(name.len() > MAX_INSTANCE_BYTES);
		let guard = advertise(&name, 8094, "uuid:long", None).unwrap();
		let status = settle(&guard.watch(), Duration::from_secs(3)).await;
		assert_eq!(status, BonjourStatus::Registered(instance_name(&name)));
	}

	#[cfg(target_os = "macos")]
	#[test]
	fn callbacks_map_to_status() {
		use imp::status_from_callback;
		assert_eq!(
			status_from_callback(0x2, 0, Some("Mini (2)")),
			BonjourStatus::Registered("Mini (2)".into())
		);
		// Add cleared: the name was lost to a conflict; the renamed Add follows.
		assert_eq!(
			status_from_callback(0, 0, Some("Mini")),
			BonjourStatus::Pending
		);
		assert_eq!(
			status_from_callback(0x2, -65548, Some("Mini")),
			BonjourStatus::Failed("mDNSResponder reported error -65548".into())
		);
		assert_eq!(
			status_from_callback(0, -65548, None),
			BonjourStatus::Failed("mDNSResponder reported error -65548".into())
		);
		assert!(matches!(
			status_from_callback(0x2, 0, None),
			BonjourStatus::Failed(_)
		));
	}

	#[cfg(target_os = "macos")]
	#[tokio::test]
	#[ignore = "needs mDNSResponder; run by hand in spike S1"]
	// mDNSResponder picks which of two same-named registrations gets renamed
	// (spike S1: it looks like the higher SRV port wins). The second one uses
	// the lower port here, so it is the one renamed; swap the ports and this
	// test becomes flaky.
	async fn a_conflict_is_renamed_and_reported() {
		let first = advertise("Spritz S1 Twin", 8096, "uuid:a", None).unwrap();
		settle(&first.watch(), Duration::from_secs(3)).await;
		let second = advertise("Spritz S1 Twin", 8095, "uuid:b", None).unwrap();
		let status = settle(&second.watch(), Duration::from_secs(5)).await;
		let BonjourStatus::Registered(name) = status else {
			panic!("{status:?}");
		};
		assert_ne!(
			name, "Spritz S1 Twin",
			"the second registration should be renamed"
		);
		drop(first);
	}
}
