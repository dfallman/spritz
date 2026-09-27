//! Manual check (spike S1):
//! `cargo run -p api --example bonjour_probe -- "Probe" 8099`
//! then `dns-sd -B _spritz._tcp` and `dns-sd -L "Probe" _spritz._tcp` in
//! another terminal. Add `twice` as a third argument to register the same
//! name twice and see the rename. Ctrl+C withdraws the service.

use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let mut args = std::env::args().skip(1);
	let name = args.next().unwrap_or_else(|| "Spritz Probe".into());
	let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(8099);
	let twice = args.next().as_deref() == Some("twice");

	let first = api::bonjour::advertise(&name, port, "uuid:probe", None)?;
	let status = api::bonjour::settle(&first.watch(), Duration::from_secs(3)).await;
	println!("first: {status:?}");
	let _second = if twice {
		let second = api::bonjour::advertise(&name, port + 1, "uuid:probe2", None)?;
		let status = api::bonjour::settle(&second.watch(), Duration::from_secs(5)).await;
		println!("second: {status:?}");
		Some(second)
	} else {
		None
	};
	println!("advertising; Ctrl+C to stop");
	tokio::signal::ctrl_c().await?;
	drop(first);
	println!("withdrawn");
	Ok(())
}
