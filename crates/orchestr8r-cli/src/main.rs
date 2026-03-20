mod client;
mod daemon;

use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};
use uuid::Uuid;

use orchestr8r_core::types::{DaemonCommand, StorageBackend, WORKFLOW_SCHEMA_VERSION};
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
    /// Stop a running workflow
    Stop { name: String },
    /// Print the status of all registered workflows
    Status,
    /// Manage installed workflows
    Workflow {
        #[command(subcommand)]
        command: WorkflowCommands,
    },
    /// Manage consumed workflow triggers
    Trigger {
        #[command(subcommand)]
        command: TriggersCommands,
    },
    /// Manage workflow runs
    Run {
        #[command(subcommand)]
        command: RunCommands,
    },
}

#[derive(Subcommand)]
enum WorkflowCommands {
    /// Install a workflow (.toml file or package directory with workflow.toml) into the config dir
    Install {
        /// Path to a .toml file or a workflow package directory
        path: PathBuf,
    },
    /// Remove an installed workflow by name
    Remove {
        /// Name of the workflow to remove
        name: String,
    },
}

#[derive(Subcommand)]
enum RunCommands {
    /// List runs (all workflows by default; filter with --workflow)
    List {
        /// Only show runs for this workflow
        #[arg(long, short)]
        workflow: Option<String>,
    },
    /// Stop a run's active processes (keeps run history)
    Stop { run_id: String },
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
        Some(Commands::Stop { name }) => {
            client::send_command_print(DaemonCommand::Stop { name }).await
        }
        Some(Commands::Status) => client::print_status().await,
        Some(Commands::Run { command }) => handle_runs_command(command).await,
        Some(Commands::Trigger { command }) => handle_triggers_command(command).await,
        Some(Commands::Workflow { command }) => handle_workflow_command(command).await,
    }
}

async fn handle_runs_command(command: RunCommands) -> anyhow::Result<()> {
    match command {
        RunCommands::List { workflow } => {
            let data_dir = dirs_data_dir();
            let storage: std::sync::Arc<dyn StorageBackend> = std::sync::Arc::new(SqliteStorage::open(&data_dir.join("state.db"))?);
            let runs = match &workflow {
                Some(name) => storage.load_workflow_runs(name)?,
                None => storage.load_all_runs()?,
            };

            if runs.is_empty() {
                match &workflow {
                    Some(name) => println!("No runs found for workflow '{}'", name),
                    None => println!("No runs found."),
                }
                return Ok(());
            }

            println!("RUN ID                               STARTED             STATUS       WORKFLOW             TRIGGER");
            println!("{}", "─".repeat(110));

            for run in runs {
                let run_id = run.id.to_string();
                let started = run.started_at.format("%Y-%m-%d %H:%M:%S");
                let status = match run.status {
                    orchestr8r_core::types::RunStatus::Running => "running",
                    orchestr8r_core::types::RunStatus::WaitingCheckpoint => "waiting",
                    orchestr8r_core::types::RunStatus::Completed => "completed",
                    orchestr8r_core::types::RunStatus::Failed => "failed",
                    orchestr8r_core::types::RunStatus::Stopped => "stopped",
                };
                let wf_name = if run.orphaned {
                    format!("{} [orphaned]", run.workflow_name)
                } else {
                    run.workflow_name.clone()
                };
                let trigger = run.trigger_payload.unwrap_or_else(|| "-".to_string());
                println!("{:<37} {:<19} {:<12} {:<20} {}", run_id, started, status, wf_name, trigger);
            }
        }
        RunCommands::Stop { run_id } => {
            let run_uuid = Uuid::parse_str(&run_id)?;
            client::send_command_print(DaemonCommand::StopRun { run_id: run_uuid }).await?;
            println!("Run {} stopped.", run_id);
        }
        RunCommands::Delete { run_id } => {
            let run_uuid = Uuid::parse_str(&run_id)?;
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

async fn handle_workflow_command(command: WorkflowCommands) -> anyhow::Result<()> {
    match command {
        WorkflowCommands::Install { path } => handle_workflow_install(path).await,
        WorkflowCommands::Remove { name } => handle_workflow_remove(name).await,
    }
}

async fn handle_workflow_install(path: PathBuf) -> anyhow::Result<()> {
    let path = path.canonicalize()
        .map_err(|e| anyhow::anyhow!("Cannot access '{}': {}", path.display(), e))?;

    let (toml_path, is_package) = if path.is_dir() {
        let tp = path.join("workflow.toml");
        anyhow::ensure!(tp.exists(), "No workflow.toml found in '{}'", path.display());
        (tp, true)
    } else if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("toml") {
        (path.clone(), false)
    } else {
        anyhow::bail!("'{}' is not a .toml file or directory", path.display());
    };

    // Parse just enough to get the name and check schema_version
    let toml_content = std::fs::read_to_string(&toml_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", toml_path.display(), e))?;
    let def: orchestr8r_core::types::WorkflowDef = toml::from_str(&toml_content)
        .map_err(|e| anyhow::anyhow!("Invalid workflow TOML: {}", e))?;

    if let Some(v) = def.schema {
        anyhow::ensure!(
            v <= WORKFLOW_SCHEMA_VERSION,
            "Workflow requires schema version {} but this orchestr8r supports up to {}",
            v,
            WORKFLOW_SCHEMA_VERSION
        );
    }

    let workflows_dir = dirs_config_dir().join("workflows");
    std::fs::create_dir_all(&workflows_dir)?;

    // Check for conflicts in both forms regardless of install type
    let dir_dest = workflows_dir.join(&def.name);
    let file_dest = workflows_dir.join(format!("{}.toml", def.name));
    if dir_dest.exists() || file_dest.exists() {
        anyhow::bail!(
            "Workflow '{}' is already installed. Remove it first with: orchestr8r workflow remove {}",
            def.name,
            def.name
        );
    }

    if is_package {
        copy_dir_all(&path, &dir_dest)?;
        println!("Installed workflow '{}' to '{}'.", def.name, dir_dest.display());
    } else {
        std::fs::copy(&path, &file_dest)?;
        println!("Installed workflow '{}' to '{}'.", def.name, file_dest.display());
    }

    // Notify the daemon to reload
    if client::send_command_once(DaemonCommand::ReloadWorkflows).await.is_ok() {
        println!("Daemon reloaded.");
    }

    Ok(())
}

fn find_workflow_by_name(workflows_dir: &std::path::Path, name: &str) -> anyhow::Result<Option<std::path::PathBuf>> {
    let Ok(entries) = std::fs::read_dir(workflows_dir) else {
        return Ok(None);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let toml_path = if path.is_dir() {
            path.join("workflow.toml")
        } else if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            path.clone()
        } else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&toml_path) else { continue };
        let Ok(def) = toml::from_str::<orchestr8r_core::types::WorkflowDef>(&content) else { continue };
        if def.name == name {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

async fn handle_workflow_remove(name: String) -> anyhow::Result<()> {
    let workflows_dir = dirs_config_dir().join("workflows");
    let dir_path = workflows_dir.join(&name);
    let file_path = workflows_dir.join(format!("{}.toml", name));

    let dest = if dir_path.exists() {
        dir_path
    } else if file_path.exists() {
        file_path
    } else {
        // Fall back: scan directory for any entry whose workflow name field matches
        find_workflow_by_name(&workflows_dir, &name)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Workflow '{}' is not installed",
                name,
            )
        })?
    };

    // Ask the daemon to stop the workflow first (ignore errors — it may already be dormant)
    let _ = client::send_command_once(DaemonCommand::Stop { name: name.clone() }).await;

    if dest.is_dir() {
        std::fs::remove_dir_all(&dest)
            .map_err(|e| anyhow::anyhow!("Failed to remove '{}': {}", dest.display(), e))?;
    } else {
        std::fs::remove_file(&dest)
            .map_err(|e| anyhow::anyhow!("Failed to remove '{}': {}", dest.display(), e))?;
    }
    println!("Removed workflow '{}'.", name);

    // Notify the daemon to reload
    if client::send_command_once(DaemonCommand::ReloadWorkflows).await.is_ok() {
        println!("Daemon reloaded.");
    }

    Ok(())
}

fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

// ─── Shared path helpers ─────────────────────────────────────────────────────

pub(crate) fn socket_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        PathBuf::from(r"\\.\pipe\orchestr8r")
    }
    #[cfg(not(target_os = "windows"))]
    {
        dirs_data_dir().join("orchestr8r.sock")
    }
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
