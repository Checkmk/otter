mod client;
mod config;
mod daemon;
mod service;
mod updater;

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
#[command(
    name = "otter",
    version,
    about = "Workflow automation dashboard — connects to the running daemon"
)]
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
    /// Manage registered workflow marketplaces
    Marketplace {
        #[command(subcommand)]
        command: MarketplaceCommands,
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
    /// Check for and install a newer otter release from GitHub
    Update {
        /// Only check for a newer release; do not download or install
        #[arg(long)]
        check: bool,
        /// Reinstall the latest release even if it matches the current version
        #[arg(long)]
        force: bool,
    },
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
    /// Stop the running daemon (if any) and start it again
    Restart,
}

#[derive(Subcommand)]
enum MarketplaceCommands {
    /// Clone a marketplace git repo and register it
    Add {
        /// Anything `git clone` accepts: a local path or a `git://` /
        /// `https://` / `ssh://` URL.
        url: String,
    },
    /// Unregister a marketplace and delete its local clone. Installed workflows
    /// that came from it remain on disk but their `origin.toml` becomes dangling.
    Remove {
        /// Name as printed by `otter status` / written to `marketplaces.toml`
        name: String,
    },
    /// Browse workflows available in registered marketplaces.
    List {
        /// Restrict listing to a single marketplace
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkflowCommands {
    /// Install or upgrade a workflow.
    ///
    /// Accepts three forms:
    /// - a local path (.toml file or package directory),
    /// - `<name>@<marketplace>` reference into a registered marketplace,
    /// - bare `<name>` of an already-installed workflow (re-resolved from its
    ///   recorded origin marketplace).
    ///
    /// If the workflow is already installed, `[require]` values are preserved
    /// and only newly-introduced inputs are prompted for. Use `--force` to
    /// wipe the existing install and start fresh.
    Install {
        /// Path, `<name>@<marketplace>` reference, or installed workflow name
        target: String,
        /// Discard existing values and reinstall from scratch
        #[arg(long)]
        force: bool,
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
    /// List installed workflows and their auto-start state.
    List,
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
        Some(Commands::Marketplace { command }) => handle_marketplace_command(command).await,
        Some(Commands::Secret { command }) => handle_secret_command(command),
        Some(Commands::Service { command }) => handle_service_command(command),
        Some(Commands::Log) => handle_log_command(),
        Some(Commands::Update { check, force }) => updater::cli::run(check, force).await,
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
        WorkflowCommands::Install { target, force } => handle_workflow_install(target, force).await,
        WorkflowCommands::Remove { name } => handle_workflow_remove(name).await,
        WorkflowCommands::Enable { name } => handle_workflow_enable(name).await,
        WorkflowCommands::Disable { name } => handle_workflow_disable(name),
        WorkflowCommands::Configure {
            name,
            reset_secrets,
        } => handle_workflow_configure(name, reset_secrets).await,
        WorkflowCommands::List => handle_workflow_list(),
    }
}

fn handle_workflow_list() -> anyhow::Result<()> {
    let config_dir = dirs_config_dir();
    let workflows_dir = config_dir.join("workflows");
    let enabled = read_enabled(&config_dir)?;
    let rows = collect_installed_workflows(&workflows_dir, &enabled);

    if rows.is_empty() {
        println!("No workflows installed.");
        return Ok(());
    }

    print_workflow_list(&rows);
    Ok(())
}

struct InstalledWorkflowRow {
    name: String,
    marketplace: Option<String>,
    kind: String,
    autostart: bool,
}

impl InstalledWorkflowRow {
    fn display_name(&self) -> String {
        match &self.marketplace {
            Some(m) => format!("{}@{}", self.name, m),
            None => self.name.clone(),
        }
    }
}

fn collect_installed_workflows(
    workflows_dir: &Path,
    enabled: &HashSet<String>,
) -> Vec<InstalledWorkflowRow> {
    let Ok(entries) = std::fs::read_dir(workflows_dir) else {
        return Vec::new();
    };
    let mut rows: Vec<InstalledWorkflowRow> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        // Skip the upgrade-staging / backup dirs created by `workflow install`.
        if file_name.starts_with('.') {
            continue;
        }
        let (toml_path, pkg_dir) = if path.is_dir() {
            (path.join("workflow.toml"), Some(path.clone()))
        } else if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            (path.clone(), None)
        } else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&toml_path) else {
            continue;
        };
        let Ok(def) = toml::from_str::<otter_core::types::WorkflowDef>(&content) else {
            continue;
        };
        let kind = match def.workflow_type {
            otter_core::types::WorkflowType::Looping => "looping",
            otter_core::types::WorkflowType::Triggered => "triggered",
        };
        let marketplace = pkg_dir.as_deref().and_then(|d| {
            otter_core::marketplace::load_origin(d)
                .ok()
                .flatten()
                .map(|o| o.marketplace)
        });
        rows.push(InstalledWorkflowRow {
            autostart: enabled.contains(&def.name),
            name: def.name,
            marketplace,
            kind: kind.to_string(),
        });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

fn print_workflow_list(rows: &[InstalledWorkflowRow]) {
    let display_names: Vec<String> = rows.iter().map(|r| r.display_name()).collect();
    let name_w = display_names
        .iter()
        .map(|n| n.chars().count())
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let kind_w = rows
        .iter()
        .map(|r| r.kind.chars().count())
        .max()
        .unwrap_or(0)
        .max("KIND".len());
    let auto_w = "AUTO-START".len();
    let total_w = name_w + kind_w + auto_w + 2;

    println!("{:<name_w$} {:<kind_w$} AUTO-START", "NAME", "KIND");
    println!("{}", "-".repeat(total_w));
    for (r, name) in rows.iter().zip(display_names.iter()) {
        let autostart = if r.autostart { "enabled" } else { "disabled" };
        println!("{:<name_w$} {:<kind_w$} {}", name, r.kind, autostart);
    }
}

/// Resolved source for an install: where the package lives on disk and (if
/// applicable) the marketplace it should be linked to via `origin.toml`.
struct InstallSource {
    /// Either a `workflow.toml` file or a package directory containing one.
    path: PathBuf,
    /// `Some((marketplace_name, rel-path-in-clone))` when the source came from
    /// a marketplace (either `<name>@<marketplace>` or a bare-name refresh).
    origin: Option<(String, String)>,
    /// `true` when the source was a bare installed-workflow name, in which
    /// case we skip the y/n README confirmation — the user is upgrading
    /// something they already chose to install.
    is_bare_name_refresh: bool,
}

fn resolve_install_source(target: &str) -> anyhow::Result<InstallSource> {
    // Case 1: <name>@<marketplace> marketplace reference.
    if let Some((marketplace_name, workflow_name)) = parse_marketplace_ref(target) {
        let data_dir = dirs_data_dir();
        let pkg = otter_core::marketplace::resolve_workflow_in_marketplace(
            &data_dir,
            marketplace_name,
            workflow_name,
        )?;
        let clone = otter_core::marketplace::clone_dir(&data_dir, marketplace_name);
        let rel = pkg
            .strip_prefix(&clone)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| pkg.to_string_lossy().to_string());
        return Ok(InstallSource {
            path: pkg,
            origin: Some((marketplace_name.to_string(), rel)),
            is_bare_name_refresh: false,
        });
    }

    // Case 2: existing filesystem path.
    let candidate = PathBuf::from(target);
    if candidate.exists() {
        let canon = candidate
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Cannot access '{}': {}", candidate.display(), e))?;
        return Ok(InstallSource {
            path: canon,
            origin: None,
            is_bare_name_refresh: false,
        });
    }

    // Case 3: bare installed-workflow name — re-resolve from recorded origin.
    let workflows_dir = dirs_config_dir().join("workflows");
    let dest = workflows_dir.join(target);
    if dest.is_dir() {
        let origin = otter_core::marketplace::load_origin(&dest)?.ok_or_else(|| {
            anyhow::anyhow!(
                "Workflow '{target}' has no recorded origin — pass a local path to reinstall."
            )
        })?;
        let pkg = otter_core::marketplace::resolve_workflow_in_marketplace(
            &dirs_data_dir(),
            &origin.marketplace,
            target,
        )?;
        return Ok(InstallSource {
            path: pkg,
            origin: Some((origin.marketplace.clone(), origin.path.clone())),
            is_bare_name_refresh: true,
        });
    }

    anyhow::bail!(
        "'{target}' is not a path, not '<name>@<marketplace>', and not an installed workflow name."
    );
}

async fn handle_workflow_install(target: String, force: bool) -> anyhow::Result<()> {
    let source = resolve_install_source(&target)?;

    let (toml_path, is_package) = if source.path.is_dir() {
        let tp = source.path.join("workflow.toml");
        anyhow::ensure!(
            tp.exists(),
            "No workflow.toml found in '{}'",
            source.path.display()
        );
        (tp, true)
    } else if source.path.is_file()
        && source.path.extension().and_then(|e| e.to_str()) == Some("toml")
    {
        (source.path.clone(), false)
    } else {
        anyhow::bail!(
            "'{}' is not a .toml file or directory",
            source.path.display()
        );
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
    let dir_dest = workflows_dir.join(&def.name);
    let file_dest = workflows_dir.join(format!("{}.toml", def.name));

    // A legacy single-file install (`<name>.toml`) carries no `.otter-state/`,
    // so there is nothing for the upgrade path to preserve. Drop it now and
    // fall through to a fresh package install.
    if !dir_dest.exists() && file_dest.exists() {
        std::fs::remove_file(&file_dest)?;
    }

    let already_installed = dir_dest.exists() || file_dest.exists();

    // --force: wipe any existing install before doing a fresh one.
    if already_installed && force {
        let _ = std::fs::remove_file(&file_dest);
        let _ = std::fs::remove_dir_all(&dir_dest);
    }

    // Upgrade path: workflow already installed and not forcing a fresh start.
    if already_installed && !force {
        // Same-version no-op when re-resolving a marketplace install: skip
        // staging, prompts, and disk writes entirely.
        if source.origin.is_some() {
            if let Ok(Some(existing_origin)) = otter_core::marketplace::load_origin(&dir_dest) {
                if existing_origin.installed_version.is_some()
                    && existing_origin.installed_version == def.version
                {
                    println!(
                        "'{}' is already at version {}.",
                        def.name,
                        def.version.as_deref().unwrap_or("?")
                    );
                    return Ok(());
                }
            }
        }

        anyhow::ensure!(
            is_package,
            "Cannot upgrade from a bare .toml file — pass a package directory or use --force."
        );

        let staging = workflows_dir.join(format!(".{}.upgrade", def.name));
        let backup = workflows_dir.join(format!(".{}.old", def.name));
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        if backup.exists() {
            std::fs::remove_dir_all(&backup)?;
        }

        let new_origin =
            source
                .origin
                .as_ref()
                .map(|(marketplace_name, rel)| otter_core::marketplace::Origin {
                    marketplace: marketplace_name.clone(),
                    path: rel.clone(),
                    installed_version: def.version.clone(),
                });
        let result = stage_and_swap_upgrade(
            &def.name,
            &source.path,
            &dir_dest,
            &staging,
            &backup,
            new_origin.as_ref(),
        )
        .await;

        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
            let _ = std::fs::remove_dir_all(&backup);
        }
        result?;

        println!(
            "Upgraded workflow '{}'{}.",
            def.name,
            def.version
                .as_deref()
                .map(|v| format!(" to {v}"))
                .unwrap_or_default()
        );
        if client::send_command_once(DaemonCommand::ReloadWorkflows)
            .await
            .is_ok()
        {
            println!("Daemon reloaded.");
        }
        return Ok(());
    }

    // Fresh install path. Confirm marketplace README/[require] preview unless
    // this is a bare-name refresh after --force (the user already opted in
    // when they first installed it).
    if let Some((marketplace_name, _)) = source.origin.as_ref() {
        if !source.is_bare_name_refresh {
            confirm_marketplace_install(&source.path, marketplace_name, &def.name)?;
        }
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
        let overwrite_hint = format!(
            "use `otter workflow configure {} --reset-secrets` to overwrite",
            def.name
        );
        for (name, entry) in manifest.iter() {
            if entry.sensitive {
                let store = store
                    .as_ref()
                    .expect("keyring opened when any entry is sensitive");
                prompt_sensitive(name, entry, store.as_ref(), false, &overwrite_hint)?;
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
        copy_dir_excluding_state(&source.path, &dir_dest)?;
    }
    std::fs::write(dir_dest.join("workflow.toml"), &raw_toml)?;
    if let Some(values) = resolved_values {
        let state_dir = dir_dest.join(".otter-state");
        std::fs::create_dir_all(&state_dir)?;
        std::fs::write(state_dir.join("values.toml"), values_toml(&values))?;
    }

    // Persist marketplace origin sidecar so subsequent `install <name>` knows
    // where the package came from and so the daemon can report updates.
    if let Some((marketplace_name, rel)) = source.origin {
        otter_core::marketplace::save_origin(
            &dir_dest,
            &otter_core::marketplace::Origin {
                marketplace: marketplace_name,
                path: rel,
                installed_version: def.version.clone(),
            },
        )?;
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
    overwrite_hint: &str,
) -> anyhow::Result<()> {
    if !force && store.list().iter().any(|k| k == name) {
        println!("\n{name} — {}", entry.description);
        println!("  ✓ already set ({overwrite_hint})");
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

/// Parse `<name>@<marketplace>` references — e.g. `hello-world@acme`.
/// Returns `(marketplace_name, workflow_name)` to match
/// `resolve_workflow_in_marketplace`'s argument order. Requires exactly one
/// `@`, both halves non-empty, and both consisting of ASCII alphanumeric /
/// `-` / `_` characters.
fn parse_marketplace_ref(target: &str) -> Option<(&str, &str)> {
    let (workflow_name, marketplace_name) = target.split_once('@')?;
    if workflow_name.is_empty() || marketplace_name.is_empty() {
        return None;
    }
    if target.matches('@').count() != 1 {
        return None;
    }
    let valid = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if !valid(workflow_name) || !valid(marketplace_name) {
        return None;
    }
    Some((marketplace_name, workflow_name))
}

/// Render the marketplace package's README (if any) and the `[require]` table
/// to stdout, then ask for `y/n` confirmation before any prompts kick in.
fn confirm_marketplace_install(
    pkg: &Path,
    marketplace_name: &str,
    workflow_name: &str,
) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};

    println!(
        "About to install '{workflow_name}' from marketplace '{marketplace_name}' (source: {}).",
        pkg.display()
    );

    let readme = pkg.join("README.md");
    if readme.exists() {
        if let Ok(content) = std::fs::read_to_string(&readme) {
            println!("\n--- README.md ---\n{content}\n-----------------\n");
        }
    }

    // Surface the [require] manifest up front so the user knows what they'll
    // be asked for. We parse leniently — a workflow without [require] is fine.
    let wf_toml = pkg.join("workflow.toml");
    if let Ok(raw) = std::fs::read_to_string(&wf_toml) {
        if let Ok(def) = toml::from_str::<otter_core::types::WorkflowDef>(&raw) {
            if let Some(req) = def.require.as_ref() {
                if !req.is_empty() {
                    println!("This workflow declares the following inputs:");
                    for (n, entry) in req.iter() {
                        let kind = if entry.sensitive { "secret" } else { "param" };
                        println!("  - {n} ({kind}): {}", entry.description);
                    }
                }
            }
        }
    }

    print!("\nProceed? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim().to_lowercase();
    if answer != "y" && answer != "yes" {
        anyhow::bail!("Install cancelled.");
    }
    Ok(())
}

async fn stage_and_swap_upgrade(
    name: &str,
    new_pkg: &Path,
    dest: &Path,
    staging: &Path,
    backup: &Path,
    origin: Option<&otter_core::marketplace::Origin>,
) -> anyhow::Result<()> {
    // 1. Copy new package files into staging (workflow.toml + any companion scripts).
    copy_dir_excluding_state(new_pkg, staging)?;

    // 2. Validate the new workflow.toml against this otter's schema.
    let new_toml_path = staging.join("workflow.toml");
    let raw = std::fs::read_to_string(&new_toml_path)
        .map_err(|e| anyhow::anyhow!("Failed to read staged workflow.toml: {e}"))?;
    let def = validate_workflow(&raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    let v = def.schema.expect("required by validate_workflow");
    anyhow::ensure!(
        v <= WORKFLOW_SCHEMA_VERSION,
        "Workflow requires schema version {} but this otter supports up to {}",
        v,
        WORKFLOW_SCHEMA_VERSION
    );
    anyhow::ensure!(
        def.name == name,
        "Upgraded package's workflow name '{}' does not match installed name '{}'",
        def.name,
        name
    );

    // 3. Copy the existing .otter-state/ (preserving values.toml, origin.toml)
    //    into the staging dir so values are preserved.
    let live_state = dest.join(".otter-state");
    let staged_state = staging.join(".otter-state");
    std::fs::create_dir_all(&staged_state)?;
    if live_state.is_dir() {
        for entry in std::fs::read_dir(&live_state)? {
            let entry = entry?;
            let target = staged_state.join(entry.file_name());
            std::fs::copy(entry.path(), &target)?;
        }
    }

    // 4. Prompt for any newly-introduced [require] entries.
    let values_path = staged_state.join("values.toml");
    let previous = otter_core::requirements::load_values_toml(&values_path)?;
    if let Some(manifest) = def.require.as_ref() {
        if !manifest.is_empty() {
            let needs_keyring = manifest.values().any(|e| e.sensitive);
            let store = if needs_keyring {
                Some(open_secret_store_or_bail()?)
            } else {
                None
            };
            let mut new_values: indexmap::IndexMap<String, String> = indexmap::IndexMap::new();
            let mut needs_tty = false;
            for (n, entry) in manifest.iter() {
                if entry.sensitive {
                    let store = store.as_ref().expect("keyring opened above");
                    if !store.list().iter().any(|k| k == n) {
                        needs_tty = true;
                    }
                } else if !previous.contains_key(n) {
                    needs_tty = true;
                }
            }
            if needs_tty {
                ensure_tty(manifest)?;
            }
            let overwrite_hint =
                format!("use `otter workflow configure {name} --reset-secrets` to overwrite");
            for (n, entry) in manifest.iter() {
                if entry.sensitive {
                    let store = store.as_ref().expect("keyring opened above");
                    let already_set = store.list().iter().any(|k| k == n);
                    if !already_set {
                        prompt_sensitive(n, entry, store.as_ref(), true, &overwrite_hint)?;
                    }
                } else if let Some(existing) = previous.get(n) {
                    new_values.insert(n.clone(), existing.clone());
                } else {
                    let v = prompt_param(n, entry, None)?;
                    new_values.insert(n.clone(), v);
                }
            }
            std::fs::write(&values_path, values_toml(&new_values))?;
        }
    }

    // 5. Update origin.toml with the new version (only when re-installing
    //    from a marketplace source — local-path upgrades leave any inherited
    //    origin.toml untouched in the copied `.otter-state/`).
    if let Some(origin) = origin {
        otter_core::marketplace::save_origin(staging, origin)?;
    }

    // 6. Atomic swap: live → backup → live, then drop the backup.
    std::fs::rename(dest, backup)?;
    if let Err(e) = std::fs::rename(staging, dest) {
        // Try to recover the live install before bubbling up.
        let _ = std::fs::rename(backup, dest);
        return Err(anyhow::anyhow!("Failed to swap upgraded package: {e}"));
    }
    std::fs::remove_dir_all(backup)?;
    Ok(())
}

async fn handle_marketplace_command(command: MarketplaceCommands) -> anyhow::Result<()> {
    match command {
        MarketplaceCommands::Add { url } => handle_marketplace_add(url).await,
        MarketplaceCommands::Remove { name } => handle_marketplace_remove(name).await,
        MarketplaceCommands::List { name } => client::print_marketplace_catalog(name).await,
    }
}

async fn handle_marketplace_add(url: String) -> anyhow::Result<()> {
    let config_dir = dirs_config_dir();
    let data_dir = dirs_data_dir();
    let mut registry = otter_core::marketplace::load_registry(&config_dir)?;

    // Clone into a staging directory first so we can read the declared name
    // before placing the clone in its final, name-keyed location. Staging lives
    // alongside the final dir so the rename is atomic on the same filesystem.
    let marketplaces_dir = otter_core::marketplace::marketplaces_dir(&data_dir);
    std::fs::create_dir_all(&marketplaces_dir)?;
    let staging = marketplaces_dir.join(format!(".staging-{}", Uuid::new_v4()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }

    println!("Cloning '{url}' ...");
    if let Err(e) = otter_core::marketplace::clone_marketplace(&url, &staging).await {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    let idx = match otter_core::marketplace::load_index(&staging) {
        Ok(i) => i,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };
    let name = idx.name.clone();

    if registry.iter().any(|m| m.name == name) {
        let _ = std::fs::remove_dir_all(&staging);
        anyhow::bail!(
            "Marketplace '{name}' is already registered. Remove it first with `otter marketplace remove {name}`."
        );
    }

    let final_clone = otter_core::marketplace::clone_dir(&data_dir, &name);
    if final_clone.exists() {
        std::fs::remove_dir_all(&final_clone)?;
    }
    std::fs::rename(&staging, &final_clone).map_err(|e| {
        anyhow::anyhow!(
            "failed to move staged clone {} → {}: {e}",
            staging.display(),
            final_clone.display()
        )
    })?;

    println!(
        "Registered marketplace '{name}' ({} workflow{}). Run `otter marketplace list {name}` to browse.",
        idx.workflows.len(),
        if idx.workflows.len() == 1 { "" } else { "s" }
    );

    registry.push(otter_core::marketplace::Marketplace {
        name: name.clone(),
        url,
        added_at: chrono::Utc::now(),
    });
    otter_core::marketplace::save_registry(&config_dir, &registry)?;

    // Refresh state synchronously so `otter status` shows correct counts right away.
    if let Err(e) = otter_core::marketplace::refresh_state_from_clone(&data_dir, &name) {
        eprintln!("Warning: failed to record marketplace state: {e}");
    }

    Ok(())
}

async fn handle_marketplace_remove(name: String) -> anyhow::Result<()> {
    let config_dir = dirs_config_dir();
    let data_dir = dirs_data_dir();
    let mut registry = otter_core::marketplace::load_registry(&config_dir)?;
    let before = registry.len();
    registry.retain(|m| m.name != name);
    if registry.len() == before {
        anyhow::bail!("Marketplace '{name}' is not registered.");
    }
    otter_core::marketplace::save_registry(&config_dir, &registry)?;

    let clone = otter_core::marketplace::clone_dir(&data_dir, &name);
    if clone.exists() {
        std::fs::remove_dir_all(&clone)?;
    }
    let state_path = otter_core::marketplace::state_path(&data_dir, &name);
    if state_path.exists() {
        let _ = std::fs::remove_file(&state_path);
    }

    println!("Removed marketplace '{name}'. Installed workflows from it remain on disk.");
    Ok(())
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
            prompt_sensitive(
                name,
                entry,
                store.as_ref(),
                force,
                "re-run with --reset-secrets to overwrite",
            )?;
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
pub(crate) fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
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
        ServiceCommands::Restart => mgr.restart(),
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
            .arg("+G")
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
    fn parse_marketplace_ref_accepts_workflow_at_marketplace() {
        // GIVEN/WHEN/THEN — returns (marketplace_name, workflow_name)
        assert_eq!(
            parse_marketplace_ref("hello-world@official"),
            Some(("official", "hello-world"))
        );
    }

    #[test]
    fn parse_marketplace_ref_rejects_non_marketplace_inputs() {
        // GIVEN inputs that don't match <name>@<marketplace>
        // WHEN/THEN
        assert_eq!(parse_marketplace_ref("./hello-world"), None);
        assert_eq!(parse_marketplace_ref("examples/hello-world"), None); // path-shaped
        assert_eq!(parse_marketplace_ref("hello-world"), None); // bare name
        assert_eq!(parse_marketplace_ref("@official"), None); // empty workflow name
        assert_eq!(parse_marketplace_ref("hello@"), None); // empty marketplace
        assert_eq!(parse_marketplace_ref("a@b@c"), None); // multiple @
    }

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
    fn collect_installed_workflows_lists_packages_and_marks_autostart() {
        // GIVEN a workflows dir with a marketplace-installed package, a bare .toml,
        //       a leftover upgrade-staging dir, and one autostart entry
        let dir = TempDir::new().unwrap();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();

        let pkg = workflows_dir.join("alpha");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("workflow.toml"),
            "name = \"alpha\"\ntype = \"triggered\"\nschema = 1\n\n\
             [trigger]\ntype = \"manual\"\n\n\
             [[steps]]\ntype = \"shell\"\ncommand = [\"true\"]\n",
        )
        .unwrap();
        otter_core::marketplace::save_origin(
            &pkg,
            &otter_core::marketplace::Origin {
                marketplace: "acme".to_string(),
                path: "workflows/alpha".to_string(),
                installed_version: Some("1.0.0".to_string()),
            },
        )
        .unwrap();

        std::fs::write(
            workflows_dir.join("beta.toml"),
            "name = \"beta\"\ntype = \"looping\"\nschema = 1\n\n\
             [[steps]]\ntype = \"shell\"\ncommand = [\"true\"]\n",
        )
        .unwrap();

        let staging = workflows_dir.join(".alpha.upgrade");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(
            staging.join("workflow.toml"),
            "name = \"alpha\"\ntype = \"triggered\"\nschema = 1\n\n\
             [trigger]\ntype = \"manual\"\n\n\
             [[steps]]\ntype = \"shell\"\ncommand = [\"true\"]\n",
        )
        .unwrap();

        let mut enabled = HashSet::new();
        enabled.insert("alpha".to_string());

        // WHEN collecting installed workflows
        let rows = collect_installed_workflows(&workflows_dir, &enabled);

        // THEN both real workflows appear sorted with correct kind/autostart;
        //      alpha is tagged with its marketplace; the staging dir is skipped.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "alpha");
        assert_eq!(rows[0].marketplace.as_deref(), Some("acme"));
        assert_eq!(rows[0].display_name(), "alpha@acme");
        assert_eq!(rows[0].kind, "triggered");
        assert!(rows[0].autostart);
        assert_eq!(rows[1].name, "beta");
        assert_eq!(rows[1].marketplace, None);
        assert_eq!(rows[1].display_name(), "beta");
        assert_eq!(rows[1].kind, "looping");
        assert!(!rows[1].autostart);
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
