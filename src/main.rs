//! microdns — mDNS/DNS-SD advertisement daemon for BigFred OS.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use microdns::config::default_config_path;
use microdns::datadir;
use microdns::run;
use microdns::version;

#[derive(Parser, Debug)]
#[command(
    name = "microdns",
    about = "Advertise mDNS/DNS-SD services for BigFred OS",
    version = env!("CARGO_PKG_VERSION")
)]
struct Cli {
    /// Override DATA_DIR (absolute path) before start
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    /// Config file path (default $DATA_DIR/etc/microdns.json)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the advertisement daemon (default)
    Serve,
    /// Alias for serve
    Run,
    /// Print build / release metadata
    Info,
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cli = Cli::parse();

    if let Some(dir) = &cli.data_dir {
        datadir::set_root(dir);
    }

    let config_path = cli.config.unwrap_or_else(default_config_path);

    match cli.command.unwrap_or(Commands::Serve) {
        Commands::Serve | Commands::Run => match run::run(&config_path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                log::error!("{e}");
                ExitCode::FAILURE
            }
        },
        Commands::Info => {
            println!("{}", version::format_info(&version::info()));
            ExitCode::SUCCESS
        }
    }
}
