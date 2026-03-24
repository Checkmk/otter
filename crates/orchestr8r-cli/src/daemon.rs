use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::warn;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(not(target_os = "windows"))]
use tokio::net::UnixListener;
#[cfg(target_os = "windows")]
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::sync::{mpsc, Mutex};
use tracing::info;
use uuid::Uuid;

use orchestr8r_core::triggers::polling::{consumed_triggers_path, delete_consumed_trigger, load_consumed_triggers};
use orchestr8r_core::engine::Engine;
use orchestr8r_core::types::{
    CheckpointAction, CheckpointResponse, DaemonCommand, DaemonEvent, DaemonResponse, EngineEvent,
    RunOutcome, RunStatus, StorageBackend, WorkflowDef, WorkflowRun,
};
use orchestr8r_core::WorkflowManager;
use orchestr8r_notify::{DesktopNotifier, Notification, Notifier};
use orchestr8r_secrets::{EncryptedSecretStore, KeyProvider, KeyringKeyProvider, SecretStore};
use orchestr8r_storage::SqliteStorage;

use crate::{dirs_config_dir, dirs_data_dir, read_enabled, write_enabled, socket_path};

struct PendingEntry {
    response_tx: tokio::sync::oneshot::Sender<CheckpointResponse>,
    step_index: usize,
    message: String,
    feedback_available: bool,
}

/// Returns a key provider for daemon use.
/// Uses the OS keyring. If unavailable, the daemon starts anyway but steps
/// that declare `secrets` will fail with SecretError::Locked at resolve time.
/// Passphrase unlock via the TUI is deferred to a future implementation.
fn build_daemon_key_provider() -> std::sync::Arc<dyn KeyProvider> {
    let kp = KeyringKeyProvider::new();
    if kp.probe().is_err() {
        warn!("OS keyring unavailable — steps that declare secrets will fail at runtime");
    }
    std::sync::Arc::new(kp)
}

/// Returns true when systemd has passed a pre-activated socket via LISTEN_FDS.
#[cfg(not(target_os = "windows"))]
fn is_socket_activated() -> bool {
    std::env::var("LISTEN_FDS").as_deref() == Ok("1")
}

/// Returns the listener to accept connections on, plus a flag indicating whether
/// it came from systemd socket activation (true) or was freshly bound (false).
#[cfg(not(target_os = "windows"))]
fn get_or_bind_listener(socket_path: &std::path::Path) -> anyhow::Result<(UnixListener, bool)> {
    if is_socket_activated() {
        use std::os::unix::io::FromRawFd;
        // systemd passes the activated socket as FD 3 (SD_LISTEN_FDS_START).
        // SAFETY: systemd guarantees FD 3 is a valid, bound, listening socket.
        let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
        std_listener.set_nonblocking(true).context("set socket non-blocking")?;
        let listener = UnixListener::from_std(std_listener).context("wrap activated socket")?;
        info!("Using systemd socket-activated listener");
        return Ok((listener, true));
    }
    let listener = UnixListener::bind(socket_path).context("bind unix socket")?;
    Ok((listener, false))
}

pub async fn run_daemon() -> anyhow::Result<()> {
    let data_dir = dirs_data_dir();
    let config_dir = dirs_config_dir();
    let socket_path = socket_path();

    std::fs::create_dir_all(&data_dir).context("create data dir")?;

    // File-based logging: writes to ~/.local/share/orchestr8r/daemon.log.
    // The _guard must stay alive for the duration of the daemon; dropping it
    // flushes the non-blocking writer and stops log delivery.
    let file_appender = tracing_appender::rolling::never(&data_dir, "daemon.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    // Write pid file so `orchestr8r service stop` can signal the process when not managed by systemd.
    let pid_path = data_dir.join("daemon.pid");
    std::fs::write(&pid_path, std::process::id().to_string())
        .context("write service pid file")?;

    #[cfg(not(target_os = "windows"))]
    // When socket-activated, systemd already owns the socket — skip the stale-check.
    if !is_socket_activated() {
        if socket_path.exists() {
            if tokio::net::UnixStream::connect(&socket_path).await.is_ok() {
                anyhow::bail!("service is already running");
            }
            // Socket exists but no listener — stale; remove it.
            let _ = std::fs::remove_file(&socket_path);
        }
    }

    let workflows_dir = config_dir.join("workflows");
    let workflows = load_workflows_from_dir(&workflows_dir)?;

    let toml_map: Arc<tokio::sync::Mutex<HashMap<String, String>>> =
        Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    let storage: Arc<dyn StorageBackend> = Arc::new(
        SqliteStorage::open(&data_dir.join("state.db")).context("open storage")?,
    );
    let notifier: Arc<dyn Notifier> = Arc::new(DesktopNotifier);
    let secret_store: std::sync::Arc<dyn SecretStore> = std::sync::Arc::new(
        EncryptedSecretStore::new(config_dir.join("secrets.age"), build_daemon_key_provider()),
    );

    mark_interrupted_runs_failed(storage.as_ref(), notifier.as_ref()).await;

    let (event_tx, mut event_rx) = mpsc::channel::<EngineEvent>(256);
    let manager = Arc::new(Mutex::new(WorkflowManager::new_with_secret_store(
        storage.clone(),
        data_dir.clone(),
        event_tx,
        notifier,
        secret_store,
    )));

    reload_workflows(workflows, &manager, &toml_map).await;

    // Auto-start workflows that have been enabled via `orchestr8r workflow enable`
    let enabled = read_enabled(&config_dir).unwrap_or_default();
    for name in &enabled {
        let mut mgr = manager.lock().await;
        if mgr.get_def(name).is_some() {
            match mgr.start(name).await {
                Ok(()) => info!("auto-start: started '{name}'"),
                Err(e) => warn!("auto-start: failed to start '{name}': {e}"),
            }
        } else {
            warn!("auto-start: workflow '{name}' not found, skipping");
        }
    }

    // run_id → pending checkpoint metadata + oneshot sender
    let pending_checkpoints: Arc<std::sync::Mutex<HashMap<Uuid, PendingEntry>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    // run_id → most recent run snapshot for replay on Subscribe
    let recent_runs: Arc<std::sync::Mutex<HashMap<Uuid, WorkflowRun>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Broadcast channels for Subscribe connections
    let subscribers: Arc<std::sync::Mutex<Vec<mpsc::Sender<DaemonEvent>>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // Fan-out task: translate EngineEvents → DaemonEvents, route checkpoint senders
    let pending_cp_fanout = pending_checkpoints.clone();
    let recent_runs_fanout = recent_runs.clone();
    let subscribers_fanout = subscribers.clone();
    let toml_map_fanout = toml_map.clone();
    let config_dir_fanout = config_dir.clone();
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            let daemon_ev = match ev {
                EngineEvent::LogAppended(e) => DaemonEvent::LogAppended(e),
                EngineEvent::RunUpdated(r) => {
                    recent_runs_fanout.lock().unwrap().insert(r.id, r.clone());
                    DaemonEvent::RunUpdated(r)
                }
                EngineEvent::WorkflowRegistered { name, kind, trigger } => {
                    let toml_content = toml_map_fanout.lock().await.get(&name).cloned();
                    let enabled = read_enabled(&config_dir_fanout).unwrap_or_default().contains(&name);
                    DaemonEvent::WorkflowRegistered { name, kind, trigger, toml_content, enabled }
                }
                EngineEvent::WorkflowStateChanged { name, state } => {
                    DaemonEvent::WorkflowStateChanged { name, state }
                }
                EngineEvent::WorkflowRemoved { name } => {
                    toml_map_fanout.lock().await.remove(&name);
                    DaemonEvent::WorkflowRemoved { name }
                }
                EngineEvent::CheckpointPending {
                    run_id,
                    step_index,
                    message,
                    feedback_available,
                    response_tx,
                } => {
                    pending_cp_fanout.lock().unwrap().insert(
                        run_id,
                        PendingEntry {
                            response_tx,
                            step_index,
                            message: message.clone(),
                            feedback_available,
                        },
                    );
                    DaemonEvent::CheckpointPending {
                        run_id,
                        step_index,
                        message,
                        feedback_available,
                    }
                }
                EngineEvent::StepProgress { run_id, step_index, chunk } => {
                    DaemonEvent::StepProgress { run_id, step_index, chunk }
                }
            };
            let mut subs = subscribers_fanout.lock().unwrap();
            subs.retain(|tx| tx.try_send(daemon_ev.clone()).is_ok());
        }
    });

    info!(socket = ?socket_path, "Daemon started — all workflows dormant");

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_ctrlc = shutdown.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Ctrl+C received, shutting down");
        shutdown_ctrlc.store(true, Ordering::Relaxed);
    });

    #[cfg(not(target_os = "windows"))]
    {
        let (listener, socket_activated) = get_or_bind_listener(&socket_path)?;
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                listener.accept(),
            )
            .await
            {
                Ok(Ok((stream, _addr))) => {
                    let mgr = manager.clone();
                    let pending = pending_checkpoints.clone();
                    let runs = recent_runs.clone();
                    let subs = subscribers.clone();
                    let st = storage.clone();
                    let tm = toml_map.clone();
                    let wd = workflows_dir.clone();
                    tokio::spawn(handle_connection(stream, mgr, pending, runs, subs, st, tm, wd));
                }
                Ok(Err(e)) => {
                    tracing::error!("accept error: {}", e);
                    break;
                }
                Err(_) => {} // timeout — check shutdown flag and continue
            }
        }
        // When socket-activated, systemd owns the socket (RemoveOnStop=yes in the unit).
        // Only remove it ourselves when we bound it directly.
        if !socket_activated {
            let _ = std::fs::remove_file(&socket_path);
        }
        let _ = std::fs::remove_file(&pid_path);
    }

    #[cfg(target_os = "windows")]
    {
        let pipe_name = socket_path.to_str().expect("pipe path is valid UTF-8");
        let mut first = true;
        loop {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let server = ServerOptions::new()
                .first_pipe_instance(first)
                .create(pipe_name)
                .context("create named pipe")?;
            first = false;
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                server.connect(),
            )
            .await
            {
                Ok(Ok(())) => {
                    let mgr = manager.clone();
                    let pending = pending_checkpoints.clone();
                    let runs = recent_runs.clone();
                    let subs = subscribers.clone();
                    let st = storage.clone();
                    let tm = toml_map.clone();
                    let wd = workflows_dir.clone();
                    tokio::spawn(handle_connection(server, mgr, pending, runs, subs, st, tm, wd));
                }
                Ok(Err(e)) => {
                    tracing::error!("accept error: {}", e);
                    break;
                }
                Err(_) => {} // timeout — check shutdown flag and continue
            }
        }
        let _ = std::fs::remove_file(&pid_path);
    }

    info!("Daemon stopped");
    Ok(())
}

async fn handle_connection<S>(
    stream: S,
    manager: Arc<Mutex<WorkflowManager>>,
    pending_checkpoints: Arc<std::sync::Mutex<HashMap<Uuid, PendingEntry>>>,
    recent_runs: Arc<std::sync::Mutex<HashMap<Uuid, WorkflowRun>>>,
    subscribers: Arc<std::sync::Mutex<Vec<mpsc::Sender<DaemonEvent>>>>,
    storage: Arc<dyn StorageBackend>,
    toml_map: Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    workflows_dir: PathBuf,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }

    let cmd: DaemonCommand = match serde_json::from_str(line.trim()) {
        Ok(c) => c,
        Err(e) => {
            let _ = write_json(&mut writer, &DaemonResponse::Error { message: e.to_string() }).await;
            return;
        }
    };

    match cmd {
        DaemonCommand::Subscribe => {
            let (sub_tx, mut sub_rx) = mpsc::channel::<DaemonEvent>(256);
            // Replay current workflow state before streaming live events
            let config_dir = workflows_dir.parent().unwrap_or(&workflows_dir);
            let enabled_set = read_enabled(config_dir).unwrap_or_default();
            let current = manager.lock().await.status();
            for wf in current {
                let _ = write_json(&mut writer, &DaemonEvent::WorkflowRegistered {
                    toml_content: toml_map.lock().await.get(&wf.name).cloned(),
                    name: wf.name.clone(),
                    kind: wf.kind,
                    trigger: wf.trigger.clone(),
                    enabled: enabled_set.contains(&wf.name),
                }).await;
                let _ = write_json(&mut writer, &DaemonEvent::WorkflowStateChanged {
                    name: wf.name,
                    state: wf.state,
                }).await;
            }
            // Replay all historical runs from storage for each workflow
            let current = manager.lock().await.status();
            for wf in current {
                if let Ok(runs) = storage.load_workflow_runs(&wf.name) {
                    for run in runs {
                        let run_id = run.id;
                        let _ = write_json(&mut writer, &DaemonEvent::RunUpdated(run)).await;
                        if let Ok(logs) = storage.load_run_logs(run_id) {
                            for log in logs {
                                let _ = write_json(&mut writer, &DaemonEvent::LogAppended(log)).await;
                            }
                        }
                    }
                }
            }
            // Replay any pending checkpoints so the UI can display a prompt
            let checkpoints: Vec<(Uuid, usize, String, bool)> = pending_checkpoints
                .lock()
                .unwrap()
                .iter()
                .map(|(&id, e)| (id, e.step_index, e.message.clone(), e.feedback_available))
                .collect();
            for (run_id, step_index, message, feedback_available) in checkpoints {
                let _ = write_json(
                    &mut writer,
                    &DaemonEvent::CheckpointPending {
                        run_id,
                        step_index,
                        message,
                        feedback_available,
                    },
                )
                .await;
            }
            subscribers.lock().unwrap().push(sub_tx);
            while let Some(ev) = sub_rx.recv().await {
                if write_json(&mut writer, &ev).await.is_err() {
                    break;
                }
            }
        }
        DaemonCommand::Start { name } => {
            let result = manager.lock().await.start(&name).await;
            let _ = write_json(&mut writer, &result_to_response(result)).await;
        }
        DaemonCommand::Stop { name } => {
            let result = manager.lock().await.stop(&name).await;
            let _ = write_json(&mut writer, &result_to_response(result)).await;
        }
        DaemonCommand::Status => {
            let workflows = manager.lock().await.status();
            let _ = write_json(&mut writer, &DaemonResponse::StatusResponse { workflows }).await;
        }
        DaemonCommand::CheckpointRespond { run_id, action } => {
            let entry = pending_checkpoints.lock().unwrap().remove(&run_id);
            let resp = if let Some(PendingEntry { response_tx, .. }) = entry {
                let _ = response_tx.send(action_to_checkpoint_response(action));
                DaemonResponse::Ok
            } else {
                DaemonResponse::Error {
                    message: format!("no pending checkpoint for run_id {run_id}"),
                }
            };
            let _ = write_json(&mut writer, &resp).await;
        }
        DaemonCommand::StopRun { run_id } => {
            let killed = kill_active_run(run_id, &recent_runs, &pending_checkpoints, &manager, &storage, &subscribers).await;
            if let Some((run, def, scripts_dir)) = killed {
                // Run finally steps in the background — don't block the response.
                let st = storage.clone();
                let subs = subscribers.clone();
                tokio::spawn(run_finally_after_kill(run, def, scripts_dir, st, subs));
            }
            let _ = write_json(&mut writer, &DaemonResponse::Ok).await;
        }
        DaemonCommand::DeleteRun { run_id } => {
            let killed = kill_active_run(run_id, &recent_runs, &pending_checkpoints, &manager, &storage, &subscribers).await;
            if let Some((run, def, scripts_dir)) = killed {
                // Run finally steps before erasing the run record.
                run_finally_after_kill(run, def, scripts_dir, storage.clone(), subscribers.clone()).await;
            }
            // Delete from storage and scratch directory
            let storage_result = storage.delete_run(run_id);
            let scratch_dir = std::path::PathBuf::from(dirs_data_dir()).join("runs").join(run_id.to_string());
            let dir_result = if scratch_dir.exists() {
                std::fs::remove_dir_all(&scratch_dir)
            } else {
                Ok(())
            };
            let resp = match (storage_result, dir_result) {
                (Ok(()), Ok(())) => {
                    recent_runs.lock().unwrap().remove(&run_id);
                    let event = DaemonEvent::RunDeleted { run_id };
                    let mut subs = subscribers.lock().unwrap();
                    subs.retain(|tx| tx.try_send(event.clone()).is_ok());
                    DaemonResponse::Ok
                }
                _ => DaemonResponse::Error {
                    message: "Failed to delete run".to_string(),
                }
            };
            let _ = write_json(&mut writer, &resp).await;
        }
        DaemonCommand::ListConsumedTriggers { workflow } => {
            let path = consumed_triggers_path(&dirs_data_dir(), &workflow);
            let resp = match load_consumed_triggers(&path) {
                Ok(triggers) => DaemonResponse::ConsumedTriggersResponse { triggers },
                Err(e) => DaemonResponse::Error { message: e.to_string() },
            };
            let _ = write_json(&mut writer, &resp).await;
        }
        DaemonCommand::DeleteConsumedTrigger { workflow, trigger } => {
            let path = consumed_triggers_path(&dirs_data_dir(), &workflow);
            let resp = match delete_consumed_trigger(&path, &trigger) {
                Ok(()) => {
                    // Broadcast updated trigger list to subscribers
                    if let Ok(triggers) = load_consumed_triggers(&path) {
                        let event = DaemonEvent::ConsumedTriggersChanged { workflow, triggers };
                        let mut subs = subscribers.lock().unwrap();
                        subs.retain(|tx| tx.try_send(event.clone()).is_ok());
                    }
                    DaemonResponse::Ok
                }
                Err(e) => DaemonResponse::Error { message: e.to_string() },
            };
            let _ = write_json(&mut writer, &resp).await;
        }
        DaemonCommand::ReloadWorkflows => {
            let resp = match load_workflows_from_dir(&workflows_dir) {
                Ok(new_workflows) => {
                    reload_workflows(new_workflows, &manager, &toml_map).await;
                    DaemonResponse::Ok
                }
                Err(e) => DaemonResponse::Error { message: e.to_string() },
            };
            let _ = write_json(&mut writer, &resp).await;
        }
        DaemonCommand::EnableWorkflow { name } => {
            let config_dir = workflows_dir.parent().unwrap_or(&workflows_dir);
            let mut set = read_enabled(config_dir).unwrap_or_default();
            set.insert(name.clone());
            let _ = write_enabled(config_dir, &set);
            let result = manager.lock().await.start(&name).await;
            let _ = write_json(&mut writer, &result_to_response(result)).await;
        }
        DaemonCommand::DisableWorkflow { name } => {
            let config_dir = workflows_dir.parent().unwrap_or(&workflows_dir);
            let mut set = read_enabled(config_dir).unwrap_or_default();
            set.remove(&name);
            let _ = write_enabled(config_dir, &set);
            let _ = write_json(&mut writer, &DaemonResponse::Ok).await;
        }
    }
}

/// Build a temporary engine and run the `[[finally]]` steps for a run that was just killed.
/// Events are forwarded to all current subscribers so the TUI sees the log entries.
async fn run_finally_after_kill(
    run: WorkflowRun,
    def: WorkflowDef,
    scripts_dir: Option<PathBuf>,
    storage: Arc<dyn StorageBackend>,
    subscribers: Arc<std::sync::Mutex<Vec<mpsc::Sender<DaemonEvent>>>>,
) {
    let (ev_tx, mut ev_rx) = mpsc::channel::<EngineEvent>(64);

    // Forward engine events to daemon subscribers while finally steps run.
    let subs = subscribers.clone();
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            let daemon_ev = match ev {
                EngineEvent::LogAppended(e) => DaemonEvent::LogAppended(e),
                EngineEvent::RunUpdated(r) => DaemonEvent::RunUpdated(r),
                _ => continue,
            };
            let mut s = subs.lock().unwrap();
            s.retain(|tx| tx.try_send(daemon_ev.clone()).is_ok());
        }
    });

    let scratch_base = PathBuf::from(dirs_data_dir()).join("runs");
    let notifier = Arc::new(DesktopNotifier);
    let engine = Engine::new_with_scripts_dir(storage, scratch_base, notifier, scripts_dir);
    engine.run_finally(&def, &run, RunOutcome::Stopped, Some(ev_tx)).await;
}

/// Kill an active run immediately. Returns the stopped run snapshot and its workflow def+scripts_dir
/// if the run was active, so the caller can run finally steps afterwards.
async fn kill_active_run(
    run_id: Uuid,
    recent_runs: &Arc<std::sync::Mutex<HashMap<Uuid, WorkflowRun>>>,
    pending_checkpoints: &Arc<std::sync::Mutex<HashMap<Uuid, PendingEntry>>>,
    manager: &Arc<Mutex<WorkflowManager>>,
    storage: &Arc<dyn StorageBackend>,
    subscribers: &Arc<std::sync::Mutex<Vec<mpsc::Sender<DaemonEvent>>>>,
) -> Option<(WorkflowRun, WorkflowDef, Option<PathBuf>)> {
    let workflow_name = recent_runs
        .lock()
        .unwrap()
        .get(&run_id)
        .filter(|r| matches!(r.status, RunStatus::Running | RunStatus::WaitingCheckpoint))
        .map(|r| r.workflow_name.clone());

    let (def, scripts_dir) = if let Some(ref wf_name) = workflow_name {
        // Drop the pending checkpoint response so the executor unblocks immediately.
        pending_checkpoints.lock().unwrap().remove(&run_id);
        // Abort the engine task; kill_on_drop ensures the subprocess is killed.
        let mut mgr = manager.lock().await;
        mgr.abort_and_stop(wf_name);
        let def = mgr.get_def(wf_name);
        let scripts_dir = mgr.get_scripts_dir(wf_name);
        (def, scripts_dir)
    } else {
        (None, None)
    };

    // Mark run as Stopped in storage and broadcast RunUpdated.
    let updated_run = recent_runs.lock().unwrap().get(&run_id).cloned();
    if let Some(mut run) = updated_run {
        if matches!(run.status, RunStatus::Running | RunStatus::WaitingCheckpoint) {
            run.status = RunStatus::Stopped;
            let _ = storage.update_workflow_run(&run);
            recent_runs.lock().unwrap().insert(run_id, run.clone());
            let event = DaemonEvent::RunUpdated(run.clone());
            let mut subs = subscribers.lock().unwrap();
            subs.retain(|tx| tx.try_send(event.clone()).is_ok());
            if let Some(def) = def {
                return Some((run, def, scripts_dir));
            }
        }
    }
    None
}

async fn reload_workflows(
    workflows: Vec<(WorkflowDef, String, Option<PathBuf>)>,
    manager: &Mutex<WorkflowManager>,
    toml_map: &Mutex<HashMap<String, String>>,
) {
    {
        let mut map = toml_map.lock().await;
        for (def, raw, _) in &workflows {
            map.insert(def.name.clone(), raw.clone());
        }
    }
    let mapped: Vec<_> = workflows
        .into_iter()
        .map(|(def, _, scripts_dir)| (def, scripts_dir))
        .collect();
    manager.lock().await.reload(mapped);
}

/// Returns `(def, raw_toml, scripts_dir)` tuples.
///
/// Scans both flat `.toml` files and one level of subdirectories (each containing `workflow.toml`).
/// Skips workflows with unsupported `schema_version` with a warning.
pub(crate) fn load_workflows_from_dir(dir: &Path) -> anyhow::Result<Vec<(WorkflowDef, String, Option<PathBuf>)>> {
    use orchestr8r_core::types::WORKFLOW_SCHEMA_VERSION;

    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();

    for entry in std::fs::read_dir(dir)?.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("toml") {
            // Flat .toml file — no scripts directory
            entries.push((path, None));
        } else if path.is_dir() {
            // Package directory — look for workflow.toml inside
            let wf_toml = path.join("workflow.toml");
            if wf_toml.is_file() {
                entries.push((wf_toml, Some(path)));
            }
        }
    }
    entries.sort_by_key(|(p, _)| p.clone());

    let mut workflows = Vec::new();
    for (path, scripts_dir) in &entries {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {path:?}"))?;
        let def: WorkflowDef = match toml::from_str(&content) {
            Ok(d) => d,
            Err(e) => {
                warn!(path = ?path, error = %e, "Skipping workflow: parse error");
                continue;
            }
        };

        // Schema version check
        let schema_ver = def.schema.unwrap_or(1);
        if schema_ver > WORKFLOW_SCHEMA_VERSION {
            warn!(
                workflow = %def.name,
                schema_version = schema_ver,
                current = WORKFLOW_SCHEMA_VERSION,
                "Skipping workflow: requires schema version {} but this orchestr8r supports up to {}",
                schema_ver,
                WORKFLOW_SCHEMA_VERSION
            );
            continue;
        }

        info!(workflow = %def.name, "Loaded workflow");
        workflows.push((def, content, scripts_dir.clone()));
    }
    Ok(workflows)
}

fn result_to_response(result: anyhow::Result<()>) -> DaemonResponse {
    match result {
        Ok(()) => DaemonResponse::Ok,
        Err(e) => DaemonResponse::Error { message: e.to_string() },
    }
}

fn action_to_checkpoint_response(action: CheckpointAction) -> CheckpointResponse {
    match action {
        CheckpointAction::Continue => CheckpointResponse::Continue,
        CheckpointAction::Stop => CheckpointResponse::Stop,
        CheckpointAction::Feedback(s) => CheckpointResponse::Feedback(s),
    }
}

async fn mark_interrupted_runs_failed(storage: &dyn StorageBackend, notifier: &dyn Notifier) {
    let runs = match storage.load_all_runs() {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to load runs on startup: {e}");
            return;
        }
    };

    let interrupted: Vec<_> = runs
        .into_iter()
        .filter(|r| matches!(r.status, RunStatus::Running | RunStatus::WaitingCheckpoint))
        .collect();

    for mut run in interrupted {
        info!(run_id = %run.id, workflow = %run.workflow_name, "Marking interrupted run as failed");
        run.status = RunStatus::Failed;
        if let Err(e) = storage.update_workflow_run(&run) {
            warn!(run_id = %run.id, "Failed to update interrupted run: {e}");
        }
        let _ = notifier
            .send(&Notification {
                summary: format!("Workflow '{}' interrupted", run.workflow_name),
                body: format!("Run {} was interrupted when the daemon stopped.", run.id),
            })
            .await;
    }
}

async fn write_json<T: serde::Serialize>(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &T,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestr8r_core::storage::InMemoryStorage;
    use orchestr8r_core::types::{RunStatus, WorkflowRun};
    use orchestr8r_notify::NoOpNotifier;
    use std::sync::Mutex;

    struct TrackingNotifier {
        sent: Mutex<Vec<String>>,
    }

    impl TrackingNotifier {
        fn new() -> Self {
            Self { sent: Mutex::new(Vec::new()) }
        }

        fn summaries(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Notifier for TrackingNotifier {
        fn name(&self) -> &str { "tracking" }
        async fn send(&self, n: &orchestr8r_notify::Notification) -> Result<(), orchestr8r_notify::NotifyError> {
            self.sent.lock().unwrap().push(n.summary.clone());
            Ok(())
        }
    }

    fn run_with_status(name: &str, status: RunStatus) -> WorkflowRun {
        let mut run = WorkflowRun::new(name.to_string());
        run.status = status;
        run
    }

    #[tokio::test]
    async fn running_and_waiting_checkpoint_runs_are_marked_failed() {
        // GIVEN one run in each non-terminal state
        let storage = InMemoryStorage::new();
        storage.save_workflow_run(&run_with_status("wf-a", RunStatus::Running)).unwrap();
        storage.save_workflow_run(&run_with_status("wf-b", RunStatus::WaitingCheckpoint)).unwrap();

        // WHEN
        mark_interrupted_runs_failed(&storage, &NoOpNotifier).await;

        // THEN
        assert_eq!(storage.load_latest_run("wf-a").unwrap().unwrap().status, RunStatus::Failed);
        assert_eq!(storage.load_latest_run("wf-b").unwrap().unwrap().status, RunStatus::Failed);
    }

    #[tokio::test]
    async fn terminal_runs_are_not_touched() {
        // GIVEN runs already in terminal states
        let storage = InMemoryStorage::new();
        storage.save_workflow_run(&run_with_status("wf-a", RunStatus::Completed)).unwrap();
        storage.save_workflow_run(&run_with_status("wf-b", RunStatus::Failed)).unwrap();

        // WHEN
        mark_interrupted_runs_failed(&storage, &NoOpNotifier).await;

        // THEN
        assert_eq!(storage.load_latest_run("wf-a").unwrap().unwrap().status, RunStatus::Completed);
        assert_eq!(storage.load_latest_run("wf-b").unwrap().unwrap().status, RunStatus::Failed);
    }

    #[tokio::test]
    async fn notification_sent_per_interrupted_run() {
        // GIVEN two non-terminal runs and one terminal run
        let storage = InMemoryStorage::new();
        storage.save_workflow_run(&run_with_status("alpha", RunStatus::Running)).unwrap();
        storage.save_workflow_run(&run_with_status("beta", RunStatus::WaitingCheckpoint)).unwrap();
        storage.save_workflow_run(&run_with_status("gamma", RunStatus::Completed)).unwrap();
        let notifier = TrackingNotifier::new();

        // WHEN
        mark_interrupted_runs_failed(&storage, &notifier).await;

        // THEN — exactly two notifications, one per interrupted run
        let summaries = notifier.summaries();
        assert_eq!(summaries.len(), 2);
        assert!(summaries.iter().any(|s| s.contains("alpha")));
        assert!(summaries.iter().any(|s| s.contains("beta")));
    }
}
