use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(not(target_os = "windows"))]
use tokio::net::{UnixListener, UnixStream};
#[cfg(target_os = "windows")]
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::sync::{mpsc, Mutex};
use tracing::info;
use uuid::Uuid;

use orchestr8r_core::triggers::polling::{consumed_triggers_path, delete_consumed_trigger, load_consumed_triggers};
use orchestr8r_core::types::{
    CheckpointAction, CheckpointResponse, DaemonCommand, DaemonEvent, DaemonResponse, EngineEvent,
    StorageBackend, WorkflowDef, WorkflowRun,
};
use orchestr8r_core::WorkflowManager;
use orchestr8r_notify::{DesktopNotifier, Notifier};
use orchestr8r_storage::SqliteStorage;

use crate::{dirs_config_dir, dirs_data_dir, socket_path};

struct PendingEntry {
    response_tx: tokio::sync::oneshot::Sender<CheckpointResponse>,
    step_index: usize,
    message: String,
    feedback_available: bool,
}

pub async fn run_daemon() -> anyhow::Result<()> {
    let data_dir = dirs_data_dir();
    let config_dir = dirs_config_dir();
    let socket_path = socket_path();

    std::fs::create_dir_all(&data_dir).context("create data dir")?;
    #[cfg(not(target_os = "windows"))]
    let _ = std::fs::remove_file(&socket_path); // remove stale socket if present

    let workflows = load_workflows_from_dir(&config_dir.join("workflows"))?;

    let toml_map: Arc<HashMap<String, String>> = Arc::new(
        workflows.iter().map(|(def, raw)| (def.name.clone(), raw.clone())).collect()
    );

    let storage: Arc<dyn StorageBackend> = Arc::new(
        SqliteStorage::open(&data_dir.join("state.db")).context("open storage")?,
    );
    let notifier: Arc<dyn Notifier> = Arc::new(DesktopNotifier);

    let (event_tx, mut event_rx) = mpsc::channel::<EngineEvent>(256);
    let manager = Arc::new(Mutex::new(WorkflowManager::new(
        storage.clone(),
        data_dir.clone(),
        event_tx,
        notifier,
    )));

    {
        let mut mgr = manager.lock().await;
        for (def, _raw) in workflows {
            mgr.register(def);
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
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            let daemon_ev = match ev {
                EngineEvent::LogAppended(e) => DaemonEvent::LogAppended(e),
                EngineEvent::RunUpdated(r) => {
                    recent_runs_fanout.lock().unwrap().insert(r.id, r.clone());
                    DaemonEvent::RunUpdated(r)
                }
                EngineEvent::WorkflowRegistered { name, kind, trigger } => {
                    DaemonEvent::WorkflowRegistered { name, kind, trigger, toml_content: None }
                }
                EngineEvent::WorkflowStateChanged { name, state } => {
                    DaemonEvent::WorkflowStateChanged { name, state }
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
        let listener = UnixListener::bind(&socket_path).context("bind unix socket")?;
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
                    tokio::spawn(handle_connection(stream, mgr, pending, runs, subs, st, tm));
                }
                Ok(Err(e)) => {
                    tracing::error!("accept error: {}", e);
                    break;
                }
                Err(_) => {} // timeout — check shutdown flag and continue
            }
        }
        let _ = std::fs::remove_file(&socket_path);
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
                    tokio::spawn(handle_connection(server, mgr, pending, runs, subs, st, tm));
                }
                Ok(Err(e)) => {
                    tracing::error!("accept error: {}", e);
                    break;
                }
                Err(_) => {} // timeout — check shutdown flag and continue
            }
        }
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
    toml_map: Arc<HashMap<String, String>>,
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
            let current = manager.lock().await.status();
            for wf in current {
                let _ = write_json(&mut writer, &DaemonEvent::WorkflowRegistered {
                    toml_content: toml_map.get(&wf.name).cloned(),
                    name: wf.name.clone(),
                    kind: wf.kind,
                    trigger: wf.trigger.clone(),
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
        DaemonCommand::Pause { name } => {
            let result = manager.lock().await.pause(&name);
            let _ = write_json(&mut writer, &result_to_response(result)).await;
        }
        DaemonCommand::Stop { name } => {
            let result = manager.lock().await.stop(&name).await;
            let _ = write_json(&mut writer, &result_to_response(result)).await;
        }
        DaemonCommand::Resume { name } => {
            let result = manager.lock().await.resume(&name);
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
        DaemonCommand::DeleteRun { run_id } => {
            // Delete from storage
            let storage_result = storage.delete_run(run_id);
            // Delete scratch directory
            let scratch_dir = std::path::PathBuf::from(dirs_data_dir()).join("runs").join(run_id.to_string());
            let dir_result = if scratch_dir.exists() {
                std::fs::remove_dir_all(&scratch_dir)
            } else {
                Ok(())
            };
            let resp = match (storage_result, dir_result) {
                (Ok(()), Ok(())) => {
                    recent_runs.lock().unwrap().remove(&run_id);
                    // Broadcast the RunDeleted event to all subscribers
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
    }
}

fn load_workflows_from_dir(dir: &Path) -> anyhow::Result<Vec<(WorkflowDef, String)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();
    files.sort();
    let mut workflows = Vec::new();
    for path in &files {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {path:?}"))?;
        let def: WorkflowDef = toml::from_str(&content)
            .with_context(|| format!("Failed to parse {path:?}"))?;
        info!(workflow = %def.name, "Loaded workflow");
        workflows.push((def, content));
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

async fn write_json<T: serde::Serialize>(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &T,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}
