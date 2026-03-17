mod client;
mod daemon;

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};
use uuid::Uuid;

use orchestr8r_core::types::{DaemonCommand, StorageBackend};
use orchestr8r_storage::SqliteStorage;

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
    /// Open the TUI dashboard (default when no subcommand is given)
    Ui,
    /// Launch the headless background daemon
    Daemon,
    /// Start a dormant workflow
    Start { name: String },
    /// Pause a running looping workflow between iterations
    Pause { name: String },
    /// Stop a running workflow
    Stop { name: String },
    /// Resume a paused workflow
    Resume { name: String },
    /// Print the status of all registered workflows
    Status,
    /// Manage workflow runs
    Runs {
        #[command(subcommand)]
        command: RunsCommands,
    },
    /// Manage consumed workflow triggers
    Triggers {
        #[command(subcommand)]
        command: TriggersCommands,
    },
}

#[derive(Subcommand)]
enum RunsCommands {
    /// List all runs for a workflow
    List { workflow: String },
    /// Delete a run by ID
    Delete { run_id: String },
}

#[derive(Subcommand)]
enum TriggersCommands {
    /// List consumed triggers for a polling workflow
    ListConsumed { workflow: String },
    /// Delete a consumed trigger so it is re-processed on the next poll
    DeleteConsumed { workflow: String, trigger: String },
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
        None | Some(Commands::Ui) => client::run_ui().await,
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
        Some(Commands::Runs { command }) => handle_runs_command(command).await,
        Some(Commands::Triggers { command }) => handle_triggers_command(command).await,
    }
}

async fn handle_runs_command(command: RunsCommands) -> anyhow::Result<()> {
    match command {
        RunsCommands::List { workflow } => {
            let data_dir = dirs_data_dir();
            let storage: std::sync::Arc<dyn StorageBackend> = std::sync::Arc::new(SqliteStorage::open(&data_dir.join("state.db"))?);
            let runs = storage.load_workflow_runs(&workflow)?;

            if runs.is_empty() {
                println!("No runs found for workflow '{}'", workflow);
                return Ok(());
            }

            // Print header
            println!("RUN ID      STARTED             STATUS       TRIGGER");
            println!("{}", "─".repeat(70));

            // Print runs
            for run in runs {
                let run_id_short = run.id.to_string()[..8].to_string();
                let started = run.started_at.format("%Y-%m-%d %H:%M:%S");
                let status = match run.status {
                    orchestr8r_core::types::RunStatus::Running => "running",
                    orchestr8r_core::types::RunStatus::WaitingCheckpoint => "waiting",
                    orchestr8r_core::types::RunStatus::Completed => "completed",
                    orchestr8r_core::types::RunStatus::Failed => "failed",
                };
                let trigger = run.trigger_payload.unwrap_or_else(|| "-".to_string());
                println!("{:<11} {:<19} {:<12} {}", run_id_short, started, status, trigger);
            }
        }
        RunsCommands::Delete { run_id } => {
            // Parse the run ID
            let run_uuid = Uuid::parse_str(&run_id)?;

            // Send the DeleteRun command through the daemon socket
            client::send_command_print(DaemonCommand::DeleteRun { run_id: run_uuid }).await?;
            println!("Run {} deleted.", run_id);
        }
    }
    Ok(())
}

async fn handle_triggers_command(command: TriggersCommands) -> anyhow::Result<()> {
    match command {
        TriggersCommands::ListConsumed { workflow } => {
            let resp = client::send_command_once(DaemonCommand::ListConsumedTriggers { workflow: workflow.clone() }).await?;
            match resp {
                orchestr8r_core::types::DaemonResponse::ConsumedTriggersResponse { triggers } => {
                    if triggers.is_empty() {
                        println!("No consumed triggers for workflow '{}'", workflow);
                    } else {
                        println!("Consumed triggers for '{}':", workflow);
                        for trigger in triggers {
                            println!("  {}", trigger);
                        }
                    }
                }
                orchestr8r_core::types::DaemonResponse::Error { message } => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                _ => eprintln!("Unexpected response from daemon"),
            }
        }
        TriggersCommands::DeleteConsumed { workflow, trigger } => {
            client::send_command_print(DaemonCommand::DeleteConsumedTrigger { workflow, trigger }).await?;
        }
    }
    Ok(())
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
