mod client;
mod daemon;
mod service;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use clap::{ArgAction, Parser, Subcommand};
use uuid::Uuid;

use otter_core::types::{DaemonCommand, StorageBackend, WORKFLOW_SCHEMA_VERSION};
use otter_secrets::{EncryptedSecretStore, KeyringKeyProvider, SecretStore};
use otter_storage::SqliteStorage;

// ─── CLI ────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(about = "Workflow automation dashboard — connects to the running daemon")]
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
    /// Manage secrets available to workflow steps
    Secret {
        #[command(subcommand)]
        command: SecretCommands,
    },
    /// Manage the otter background service
    Service {
        #[command(subcommand)]
        command: ServiceCommands,
    },
    /// Follow the daemon log
    Log,
    /// Run the daemon process (used internally by the service unit)
    #[command(hide = true, name = "_daemon")]
    #[allow(non_camel_case_types)]
    _Daemon,
}

#[derive(Subcommand)]
enum ServiceCommands {
    /// Install and enable automatic startup via systemd socket activation
    Enable,
    /// Disable automatic startup and stop the service
    Disable,
    /// Start the daemon for this session (without enabling on boot)
    Start,
    /// Stop the running daemon
    Stop,
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
    /// Enable auto-start for a workflow
    Enable {
        /// Name of the workflow to enable
        name: String,
    },
    /// Disable auto-start for a workflow
    Disable {
        /// Name of the workflow to disable
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
enum SecretCommands {
    /// Store a secret value (creates or overwrites)
    Set { name: String, value: String },
    /// Print the value of a secret
    Get { name: String },
    /// List all secret names
    List,
    /// Delete a secret
    Delete { name: String },
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

    // The daemon sets up its own file-based subscriber inside run_daemon().
    // Initialising the global subscriber here would conflict with that, so we
    // branch early before touching tracing.
    if matches!(cli.command, Some(Commands::_Daemon)) {
        return daemon::run_daemon().await;
    }

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
        Some(Commands::_Daemon) => unreachable!(),
        Some(Commands::Start { name }) => {
            client::send_command_print(DaemonCommand::Start { name }).await
        }
        Some(Commands::Stop { name }) => {
            client::send_command_print(DaemonCommand::Stop { name }).await
        }
        Some(Commands::Status) => {
            let enabled = service::platform_service_manager().is_enabled();
            client::print_status(enabled).await
        }
        Some(Commands::Run { command }) => handle_runs_command(command).await,
        Some(Commands::Trigger { command }) => handle_triggers_command(command).await,
        Some(Commands::Workflow { command }) => handle_workflow_command(command).await,
        Some(Commands::Secret { command }) => handle_secret_command(command),
        Some(Commands::Service { command }) => handle_service_command(command),
        Some(Commands::Log) => handle_log_command(),
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
                    otter_core::types::RunStatus::Running => "running",
                    otter_core::types::RunStatus::WaitingCheckpoint => "waiting",
                    otter_core::types::RunStatus::Completed => "completed",
                    otter_core::types::RunStatus::Failed => "failed",
                    otter_core::types::RunStatus::Stopped => "stopped",
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
                otter_core::types::DaemonResponse::ConsumedTriggersResponse { triggers } => {
                    if triggers.is_empty() {
                        println!("No consumed triggers for workflow '{}'", workflow);
                    } else {
                        println!("Consumed triggers for '{}':", workflow);
                        for trigger in triggers {
                            println!("  {}", trigger);
                        }
                    }
                }
                otter_core::types::DaemonResponse::Error { message } => {
                    eprintln!("Error: {}", message);
                    std::process::exit(1);
                }
                _ => eprintln!("Unexpected response from service"),
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
        WorkflowCommands::Enable { name } => handle_workflow_enable(name).await,
        WorkflowCommands::Disable { name } => handle_workflow_disable(name),
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
    let def: otter_core::types::WorkflowDef = toml::from_str(&toml_content)
        .map_err(|e| anyhow::anyhow!("Invalid workflow TOML: {}", e))?;

    if let Some(v) = def.schema {
        anyhow::ensure!(
            v <= WORKFLOW_SCHEMA_VERSION,
            "Workflow requires schema version {} but this otter supports up to {}",
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
            "Workflow '{}' is already installed. Remove it first with: otter workflow remove {}",
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
        let Ok(def) = toml::from_str::<otter_core::types::WorkflowDef>(&content) else { continue };
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

// ─── Enabled-workflows helpers ───────────────────────────────────────────────

pub(crate) fn enabled_workflows_path(config_dir: &Path) -> PathBuf {
    config_dir.join("enabled-workflows.json")
}

pub(crate) fn read_enabled(config_dir: &Path) -> anyhow::Result<HashSet<String>> {
    let path = enabled_workflows_path(config_dir);
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let s = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&s)?)
}

pub(crate) fn write_enabled(config_dir: &Path, set: &HashSet<String>) -> anyhow::Result<()> {
    let mut sorted: Vec<_> = set.iter().cloned().collect();
    sorted.sort();
    let s = serde_json::to_string_pretty(&sorted)?;
    std::fs::write(enabled_workflows_path(config_dir), s)?;
    Ok(())
}

async fn handle_workflow_enable(name: String) -> anyhow::Result<()> {
    let config_dir = dirs_config_dir();
    let mut set = read_enabled(&config_dir)?;
    set.insert(name.clone());
    write_enabled(&config_dir, &set)?;
    if client::send_command_once(DaemonCommand::Start { name: name.clone() }).await.is_ok() {
        println!("Enabled auto-start for '{name}' and started it.");
    } else {
        println!("Enabled auto-start for '{name}'. Takes effect on next daemon start.");
    }
    Ok(())
}

fn handle_workflow_disable(name: String) -> anyhow::Result<()> {
    let config_dir = dirs_config_dir();
    let mut set = read_enabled(&config_dir)?;
    set.remove(&name);
    write_enabled(&config_dir, &set)?;
    println!("Disabled auto-start for '{name}'.");
    Ok(())
}

fn handle_secret_command(command: SecretCommands) -> anyhow::Result<()> {
    let kp = KeyringKeyProvider::new();
    kp.probe().map_err(|e| anyhow::anyhow!("OS keyring unavailable: {e}\nSecrets management requires a working OS keyring (libsecret on Linux, Keychain on macOS)."))?;
    let store = EncryptedSecretStore::new(dirs_config_dir().join("secrets.age"), std::sync::Arc::new(kp));
    match command {
        SecretCommands::Set { name, value } => {
            store.set(&name, &value)?;
            println!("Secret '{}' saved.", name);
        }
        SecretCommands::Get { name } => {
            match store.resolve(&[name.clone()]) {
                Ok(pairs) => println!("{}", pairs[0].1),
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        SecretCommands::List => {
            let keys = store.list();
            if keys.is_empty() {
                println!("No secrets stored.");
            } else {
                for key in keys {
                    println!("{}", key);
                }
            }
        }
        SecretCommands::Delete { name } => {
            store.delete(&name)?;
            println!("Secret '{}' deleted.", name);
        }
    }
    Ok(())
}

fn handle_service_command(command: ServiceCommands) -> anyhow::Result<()> {
    let mgr = service::platform_service_manager();
    match command {
        ServiceCommands::Enable => mgr.enable(),
        ServiceCommands::Disable => mgr.disable(),
        ServiceCommands::Start => mgr.start(),
        ServiceCommands::Stop => mgr.stop(),
    }
}

fn handle_log_command() -> anyhow::Result<()> {
    let log_path = dirs_data_dir().join("daemon.log");
    if !log_path.exists() {
        anyhow::bail!(
            "Log file not found: {}\nStart the daemon first with: otter service start",
            log_path.display()
        );
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new("less")
            .arg("+F")
            .arg(&log_path)
            .exec();
        // exec() only returns on error
        return Err(err.into());
    }

    #[cfg(windows)]
    {
        std::process::Command::new("powershell")
            .args(["-Command", &format!("Get-Content -Wait '{}'", log_path.display())])
            .status()?;
        Ok(())
    }
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
        PathBuf::from(r"\\.\pipe\otter")
    }
    #[cfg(not(target_os = "windows"))]
    {
        dirs_data_dir().join("otter.sock")
    }
}

pub(crate) fn dirs_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("otter")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".local").join("share").join("otter")
    }
}

pub(crate) fn dirs_config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("otter")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("otter")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_enabled_missing_file_returns_empty() {
        // GIVEN a config dir with no enabled-workflows.json
        let dir = TempDir::new().unwrap();
        // WHEN reading the enabled set
        let set = read_enabled(dir.path()).unwrap();
        // THEN it is empty
        assert!(set.is_empty());
    }

    #[test]
    fn round_trip_write_and_read() {
        // GIVEN a set of workflow names written to disk
        let dir = TempDir::new().unwrap();
        let mut set = HashSet::new();
        set.insert("alpha".to_string());
        set.insert("beta".to_string());
        write_enabled(dir.path(), &set).unwrap();
        // WHEN reading back
        let loaded = read_enabled(dir.path()).unwrap();
        // THEN the same names are present
        assert_eq!(loaded, set);
    }

    #[test]
    fn enable_is_idempotent() {
        // GIVEN a workflow already in the enabled set
        let dir = TempDir::new().unwrap();
        let mut set = HashSet::new();
        set.insert("my-workflow".to_string());
        write_enabled(dir.path(), &set).unwrap();
        // WHEN enabling it again
        set.insert("my-workflow".to_string());
        write_enabled(dir.path(), &set).unwrap();
        // THEN it appears exactly once
        let loaded = read_enabled(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains("my-workflow"));
    }

    #[test]
    fn disable_when_absent_is_noop() {
        // GIVEN an enabled set that does not contain the target workflow
        let dir = TempDir::new().unwrap();
        let mut set = HashSet::new();
        set.insert("other".to_string());
        write_enabled(dir.path(), &set).unwrap();
        // WHEN disabling a workflow that was never enabled
        set.remove("nonexistent");
        write_enabled(dir.path(), &set).unwrap();
        // THEN the existing entry is unchanged
        let loaded = read_enabled(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains("other"));
    }

    #[test]
    fn persisted_file_is_sorted_json_array() {
        // GIVEN names inserted in arbitrary order
        let dir = TempDir::new().unwrap();
        let mut set = HashSet::new();
        set.insert("zebra".to_string());
        set.insert("alpha".to_string());
        set.insert("middle".to_string());
        write_enabled(dir.path(), &set).unwrap();
        // WHEN reading the raw file
        let raw = std::fs::read_to_string(enabled_workflows_path(dir.path())).unwrap();
        let parsed: Vec<String> = serde_json::from_str(&raw).unwrap();
        // THEN names are sorted
        assert_eq!(parsed, vec!["alpha", "middle", "zebra"]);
    }
}
