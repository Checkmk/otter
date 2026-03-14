mod client;
mod daemon;

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};

use orchestr8r_core::types::DaemonCommand;

// ─── CLI ────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "orchestr8r", about = "Workflow automation dashboard — connects to the running daemon")]
struct Cli {
    /// Increase log verbosity (-v = debug, -vv = trace)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    /// Suppress all log output
    #[arg(short, long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Launch the headless background daemon
    Daemon,
    /// Start a dormant workflow
    Start { name: String },
    /// Pause a running indefinite workflow between iterations
    Pause { name: String },
    /// Stop a running workflow
    Stop { name: String },
    /// Resume a paused workflow
    Resume { name: String },
    /// Print the status of all registered workflows
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let level = if cli.quiet {
        tracing::Level::ERROR
    } else {
        match cli.verbose {
            0 => tracing::Level::INFO,
            1 => tracing::Level::DEBUG,
            _ => tracing::Level::TRACE,
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(level.into()),
        )
        .init();

    match cli.command {
        None => client::run_ui().await,
        Some(Commands::Daemon) => daemon::run_daemon().await,
        Some(Commands::Start { name }) => {
            client::send_command_print(DaemonCommand::Start { name }).await
        }
        Some(Commands::Pause { name }) => {
            client::send_command_print(DaemonCommand::Pause { name }).await
        }
        Some(Commands::Stop { name }) => {
            client::send_command_print(DaemonCommand::Stop { name }).await
        }
        Some(Commands::Resume { name }) => {
            client::send_command_print(DaemonCommand::Resume { name }).await
        }
        Some(Commands::Status) => client::print_status().await,
    }
}

// ─── Shared path helpers ─────────────────────────────────────────────────────

pub(crate) fn socket_path() -> PathBuf {
    dirs_data_dir().join("orchestr8r.sock")
}

pub(crate) fn dirs_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("orchestr8r")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".local").join("share").join("orchestr8r")
    }
}

pub(crate) fn dirs_config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("orchestr8r")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("orchestr8r")
    }
}
