//! microdns — mDNS/DNS-SD advertisement daemon for BigFred OS.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

use microdns::config::default_config_path;
use microdns::ctl;
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

    /// Control socket (default $DATA_DIR/run/microdns.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

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
    /// Query a running daemon
    Services {
        #[command(subcommand)]
        command: ServicesCommands,
    },
}

#[derive(Subcommand, Debug)]
enum ServicesCommands {
    /// List services the daemon is currently advertising
    List {
        /// Output format
        #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
    },
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum OutputFormat {
    #[default]
    Human,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Some(dir) = &cli.data_dir {
        datadir::set_root(dir);
    }

    let config_path = cli.config.unwrap_or_else(default_config_path);
    let socket = cli.socket.unwrap_or_else(ctl::default_socket);

    match cli.command.unwrap_or(Commands::Serve) {
        Commands::Serve | Commands::Run => {
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                .init();
            match run::run_with_socket(&config_path, &socket) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    log::error!("{e}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Info => {
            println!("{}", version::format_info(&version::info()));
            ExitCode::SUCCESS
        }
        Commands::Services { command } => match command {
            ServicesCommands::List { output } => match ctl::services_list(&socket) {
                Ok(services) => {
                    let result = match output {
                        OutputFormat::Human => ctl::print_human(&mut std::io::stdout(), &services),
                        OutputFormat::Json => ctl::print_json(&mut std::io::stdout(), &services),
                    };
                    match result {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => {
                            eprintln!("{e}");
                            ExitCode::FAILURE
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::FAILURE
                }
            },
        },
    }
}
