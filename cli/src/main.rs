use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "spritz")]
#[command(about = "VLC Apple TV Video Publisher")]
struct Cli {
	/// The folder containing the videos to publish
	#[arg(short, long)]
	folder: PathBuf,

	/// The port to bind the server to
	#[arg(short, long, default_value_t = 8080)]
	port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let cli = Cli::parse();

	if !cli.folder.exists() || !cli.folder.is_dir() {
		anyhow::bail!("The specified folder does not exist or is not a directory.");
	}

	println!("Starting spritz server targeting folder: {:?}", cli.folder);

	// Start the api server
	if let Err(e) = api::start_server(cli.port, cli.folder).await {
		eprintln!("Server error: {}", e);
		std::process::exit(1);
	}

	Ok(())
}
