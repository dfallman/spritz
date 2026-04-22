use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "spritz")]
#[command(about = "Nano DLNA media server — run in any folder to share it on the network")]
struct Cli {
	/// Folders to serve (defaults to current directory)
	#[arg(value_name = "FOLDER")]
	folders: Vec<PathBuf>,

	/// Port to listen on
	#[arg(short, long, default_value_t = 8080)]
	port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let cli = Cli::parse();

	let folders = if cli.folders.is_empty() {
		vec![std::env::current_dir()?]
	} else {
		cli.folders
	};

	for folder in &folders {
		if !folder.is_dir() {
			anyhow::bail!("{} is not a directory", folder.display());
		}
	}

	if let Err(e) = api::start_server(cli.port, folders).await {
		eprintln!("Server error: {e}");
		std::process::exit(1);
	}

	Ok(())
}
