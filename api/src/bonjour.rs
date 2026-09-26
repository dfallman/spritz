//! Bonjour (`_spritz._tcp`) advertisement.
//!
//! macOS goes through the system mDNSResponder (`dns_sd`), so the Bonjour
//! Sleep Proxy keeps a sleeping Mac visible. Elsewhere the pure-Rust
//! `mdns-sd` responder is used.

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
	/// Submitted; the responder has not answered yet.
	Pending,
	/// Registered under this instance name (the responder may have renamed it).
	Registered(String),
	/// The responder reported an error after the call returned.
	Failed(String),
}

/// A cloneable view of a registration's status, for code that must not own
/// the guard (the check monitor, the CLI's status line).
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

	pub(crate) type DnsServiceRef = *mut c_void;
	pub(crate) type DispatchQueue = *mut c_void;

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
		pub(crate) fn DNSServiceSetDispatchQueue(
			sd_ref: DnsServiceRef,
			queue: DispatchQueue,
		) -> i32;
		pub(crate) fn DNSServiceRefDeallocate(sd_ref: DnsServiceRef);
		pub(crate) fn dispatch_queue_create(
			label: *const c_char,
			attr: *const c_void,
		) -> DispatchQueue;
		pub(crate) fn dispatch_sync_f(
			queue: DispatchQueue,
			context: *mut c_void,
			work: extern "C" fn(*mut c_void),
		);
		pub(crate) fn dispatch_release(object: *mut c_void);
	}

	/// Runs on the registration's queue whenever the responder answers.
	extern "C" fn on_register(
		_sd_ref: DnsServiceRef,
		_flags: u32,
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
		let next = if error_code == 0 && !name.is_null() {
			// SAFETY: dns_sd passes a NUL-terminated name valid for the call.
			let name = unsafe { CStr::from_ptr(name) };
			BonjourStatus::Registered(name.to_string_lossy().into_owned())
		} else {
			BonjourStatus::Failed(format!("mDNSResponder reported error {error_code}"))
		};
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

	pub(crate) fn new_queue() -> DispatchQueue {
		// SAFETY: a static label and NULL attributes (a serial queue).
		unsafe { dispatch_queue_create(c"org.spritz.bonjour".as_ptr(), ptr::null()) }
	}

	pub(crate) fn register(
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
	use std::sync::{Arc, Mutex};

	pub struct Registration;

	pub(crate) fn register(
		_regtype: &str,
		_name: &str,
		_port: u16,
		_pairs: &[(&'static str, String)],
		_hostname: Option<&str>,
		_status: Arc<Mutex<BonjourStatus>>,
	) -> anyhow::Result<Registration> {
		anyhow::bail!("Bonjour is not implemented on this platform yet")
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
	#[tokio::test]
	#[ignore = "needs mDNSResponder; run by hand in spike S1"]
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
