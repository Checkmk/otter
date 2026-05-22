mod client;
mod config;
mod daemon;
mod service;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use clap::{ArgAction, Parser, Subcommand};
use otter_core::requirements::{
    expand_tilde, validate_value_chars, validate_workflow, RequireEntry, Requirements,
};
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
    /// Re-prompt for `[require]` values of an installed workflow and rewrite
    /// it. Use this to update params or secrets without reinstalling.
    Configure {
        /// Name of the installed workflow
        name: String,
        /// Also prompt to overwrite each declared sensitive entry.
        #[arg(long)]
        reset_secrets: bool,
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
            let storage: std::sync::Arc<dyn StorageBackend> =
                std::sync::Arc::new(SqliteStorage::open(&data_dir.join("state.db"))?);
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
                println!(
                    "{:<37} {:<19} {:<12} {:<20} {}",
                    run_id, started, status, wf_name, trigger
                );
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
            let resp = client::send_command_once(DaemonCommand::ListConsumedTriggers {
                workflow: workflow.clone(),
            })
            .await?;
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
            client::send_command_print(DaemonCommand::DeleteConsumedTrigger { workflow, trigger })
                .await?;
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
        WorkflowCommands::Configure {
            name,
            reset_secrets,
        } => handle_workflow_configure(name, reset_secrets).await,
    }
}

async fn handle_workflow_install(path: PathBuf) -> anyhow::Result<()> {
    let path = path
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Cannot access '{}': {}", path.display(), e))?;

    let (toml_path, is_package) = if path.is_dir() {
        let tp = path.join("workflow.toml");
        anyhow::ensure!(
            tp.exists(),
            "No workflow.toml found in '{}'",
            path.display()
        );
        (tp, true)
    } else if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("toml") {
        (path.clone(), false)
    } else {
        anyhow::bail!("'{}' is not a .toml file or directory", path.display());
    };

    let raw_toml = std::fs::read_to_string(&toml_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", toml_path.display(), e))?;

    // Full validation up front so we fail before touching the filesystem.
    let def = validate_workflow(&raw_toml).map_err(|e| anyhow::anyhow!("{e}"))?;

    let v = def.schema.expect("required by validate_workflow");
    anyhow::ensure!(
        v <= WORKFLOW_SCHEMA_VERSION,
        "Workflow requires schema version {} but this otter supports up to {}",
        v,
        WORKFLOW_SCHEMA_VERSION
    );

    let workflows_dir = dirs_config_dir().join("workflows");
    std::fs::create_dir_all(&workflows_dir)?;

    // Refuse re-install when either form already exists. Use `otter workflow
    // configure` to update values without reinstalling.
    let dir_dest = workflows_dir.join(&def.name);
    let file_dest = workflows_dir.join(format!("{}.toml", def.name));
    if dir_dest.exists() || file_dest.exists() {
        anyhow::bail!(
            "Workflow '{}' is already installed. Use `otter workflow configure {}` to update \
             values, or remove first with: otter workflow remove {}",
            def.name,
            def.name,
            def.name
        );
    }

    // Pre-flight: gather all interactive input BEFORE touching the filesystem,
    // so a failed prompt (TTY check, missing keyring, user abort) doesn't
    // leave a partial install behind.
    let has_manifest = def.require.as_ref().map(|m| !m.is_empty()).unwrap_or(false);
    let resolved_values = if has_manifest {
        let manifest = def.require.as_ref().expect("checked above");
        ensure_tty(manifest)?;
        let needs_keyring = manifest.values().any(|e| e.sensitive);
        let store = if needs_keyring {
            Some(open_secret_store_or_bail()?)
        } else {
            None
        };

        let mut values: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
        for (name, entry) in manifest.iter() {
            if entry.sensitive {
                let store = store
                    .as_ref()
                    .expect("keyring opened when any entry is sensitive");
                prompt_sensitive(name, entry, store.as_ref(), false)?;
            } else {
                let v = prompt_param(name, entry, None)?;
                values.insert(name.clone(), v);
            }
        }
        Some(values)
    } else {
        None
    };

    // Now commit to disk: copy source → write template → write values.toml.
    std::fs::create_dir_all(&dir_dest)?;
    if is_package {
        copy_dir_excluding_state(&path, &dir_dest)?;
    }
    std::fs::write(dir_dest.join("workflow.toml"), &raw_toml)?;
    if let Some(values) = resolved_values {
        let state_dir = dir_dest.join(".otter-state");
        std::fs::create_dir_all(&state_dir)?;
        std::fs::write(state_dir.join("values.toml"), values_toml(&values))?;
    }

    println!(
        "Installed workflow '{}' to '{}'.",
        def.name,
        dir_dest.display()
    );

    if client::send_command_once(DaemonCommand::ReloadWorkflows)
        .await
        .is_ok()
    {
        println!("Daemon reloaded.");
    }

    Ok(())
}

fn values_toml(values: &indexmap::IndexMap<String, String>) -> String {
    let mut s = String::from(
        "# Generated by `otter workflow install`. Does NOT contain secrets —\n\
         # those live in the OS keyring. Re-run `otter workflow configure <name>`\n\
         # to change these values.\n\n",
    );
    for (k, v) in values {
        s.push_str(&format!("{k} = \"{v}\"\n"));
    }
    s
}

fn ensure_tty(manifest: &Requirements) -> anyhow::Result<()> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Ok(());
    }
    eprintln!("This workflow declares the following inputs (interactive install required):");
    for (name, entry) in manifest.iter() {
        let kind = if entry.sensitive { "secret" } else { "param" };
        eprintln!("  - {name} ({kind}): {}", entry.description);
    }
    anyhow::bail!("install requires a TTY for prompts; run in an interactive shell");
}

/// Prompt for a non-sensitive param. `current` (if `Some`) is shown as the
/// default and used when the user presses Enter on an empty line.
fn prompt_param(name: &str, entry: &RequireEntry, current: Option<&str>) -> anyhow::Result<String> {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let default = current.or(entry.default.as_deref());
    loop {
        println!("\n{name} — {}", entry.description);
        match default {
            Some(d) => print!("  [{d}] > "),
            None => print!("  > "),
        }
        stdout.flush()?;
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let trimmed = line.trim_end_matches(['\n', '\r']);
        let raw = if trimmed.is_empty() {
            match default {
                Some(d) => d.to_string(),
                None => {
                    eprintln!("  (value required)");
                    continue;
                }
            }
        } else {
            trimmed.to_string()
        };
        if let Err(c) = validate_value_chars(&raw) {
            eprintln!("  (rejected: value may not contain {c:?})");
            continue;
        }
        return Ok(expand_tilde(&raw));
    }
}

/// Prompt for a sensitive entry. Writes directly to the keyring. Skips when
/// the entry is already set unless `force` is true.
fn prompt_sensitive(
    name: &str,
    entry: &RequireEntry,
    store: &EncryptedSecretStore,
    force: bool,
) -> anyhow::Result<()> {
    if !force && store.list().iter().any(|k| k == name) {
        println!("\n{name} — {}", entry.description);
        println!("  ✓ already set (use --reset-secrets to overwrite)");
        return Ok(());
    }
    println!("\n{name} — {}", entry.description);
    loop {
        let value = dialoguer::Password::new()
            .with_prompt(format!("  {name}"))
            .interact()
            .map_err(|e| anyhow::anyhow!("prompt failed: {e}"))?;
        if let Err(c) = validate_value_chars(&value) {
            eprintln!("  (rejected: value may not contain {c:?})");
            continue;
        }
        store.set(name, &value)?;
        println!("  ✓ stored");
        return Ok(());
    }
}

async fn handle_workflow_configure(name: String, reset_secrets: bool) -> anyhow::Result<()> {
    let workflows_dir = dirs_config_dir().join("workflows");
    let dir_dest = workflows_dir.join(&name);
    if !dir_dest.is_dir() {
        let flat = workflows_dir.join(format!("{}.toml", name));
        if flat.exists() {
            anyhow::bail!(
                "Workflow '{name}' predates the package layout — please reinstall to configure"
            );
        }
        anyhow::bail!("Workflow '{name}' is not installed");
    }
    let template_path = dir_dest.join("workflow.toml");
    let template = std::fs::read_to_string(&template_path)?;
    let def = validate_workflow(&template).map_err(|e| anyhow::anyhow!("{e}"))?;
    let manifest = match &def.require {
        Some(m) if !m.is_empty() => m,
        _ => anyhow::bail!("Workflow '{name}' has no [require] manifest; nothing to configure"),
    };

    ensure_tty(manifest)?;

    let state_dir = dir_dest.join(".otter-state");
    let values_path = state_dir.join("values.toml");
    let previous = otter_core::requirements::load_values_toml(&values_path)?;

    let needs_keyring = manifest.values().any(|e| e.sensitive);
    let store = if needs_keyring {
        Some(open_secret_store_or_bail()?)
    } else {
        None
    };

    let mut new_values: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
    for (name, entry) in manifest.iter() {
        if entry.sensitive {
            let store = store.as_ref().expect("keyring opened above");
            let already_set = store.list().iter().any(|k| k == name);
            // Edge case: declared sensitive but missing from keyring (user
            // deleted it). Always prompt in that case, regardless of flag.
            let force = reset_secrets || !already_set;
            prompt_sensitive(name, entry, store.as_ref(), force)?;
        } else {
            let current = previous.get(name).map(String::as_str);
            let v = prompt_param(name, entry, current)?;
            new_values.insert(name.clone(), v);
        }
    }

    std::fs::create_dir_all(&state_dir)?;
    atomic_write(&values_path, values_toml(&new_values).as_bytes())?;

    println!("Updated workflow '{name}'.");

    if client::send_command_once(DaemonCommand::ReloadWorkflows)
        .await
        .is_ok()
    {
        println!("Daemon reloaded.");
    }

    Ok(())
}

/// Write `bytes` to `path` via a sibling temp file + rename so concurrent
/// daemon reloads can never observe a partial file.
fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("out")
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn find_workflow_by_name(
    workflows_dir: &std::path::Path,
    name: &str,
) -> anyhow::Result<Option<std::path::PathBuf>> {
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
        let Ok(content) = std::fs::read_to_string(&toml_path) else {
            continue;
        };
        let Ok(def) = toml::from_str::<otter_core::types::WorkflowDef>(&content) else {
            continue;
        };
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
        find_workflow_by_name(&workflows_dir, &name)?
            .ok_or_else(|| anyhow::anyhow!("Workflow '{}' is not installed", name,))?
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
    if client::send_command_once(DaemonCommand::ReloadWorkflows)
        .await
        .is_ok()
    {
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
    if client::send_command_once(DaemonCommand::Start { name: name.clone() })
        .await
        .is_ok()
    {
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

fn open_secret_store_or_bail() -> anyhow::Result<std::sync::Arc<EncryptedSecretStore>> {
    let kp = KeyringKeyProvider::new();
    kp.probe().map_err(|e| anyhow::anyhow!("OS keyring unavailable: {e}\nSecrets management requires a working OS keyring (libsecret on Linux, Keychain on macOS)."))?;
    Ok(std::sync::Arc::new(EncryptedSecretStore::new(
        dirs_config_dir().join("secrets.age"),
        std::sync::Arc::new(kp),
    )))
}

fn handle_secret_command(command: SecretCommands) -> anyhow::Result<()> {
    let store = open_secret_store_or_bail()?;
    match command {
        SecretCommands::Set { name, value } => {
            store.set(&name, &value)?;
            println!("Secret '{}' saved.", name);
        }
        SecretCommands::Get { name } => match store.resolve(std::slice::from_ref(&name)) {
            Ok(pairs) => println!("{}", pairs[0].1),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        },
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
            .arg("-R")
            // LESS and LESSKEY are overwritten to ignore user configs that
            // could interfere with log formatting
            .env("LESS", "-R")
            .env("LESSKEY", "/dev/null")
            .arg(&log_path)
            .exec();
        // exec() only returns on error
        Err(err.into())
    }

    #[cfg(windows)]
    {
        std::process::Command::new("powershell")
            .args([
                "-Command",
                &format!("Get-Content -Wait '{}'", log_path.display()),
            ])
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

fn copy_dir_excluding_state(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name() == ".otter-state" {
            continue;
        }
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
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("otter")
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
