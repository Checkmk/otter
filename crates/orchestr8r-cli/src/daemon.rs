use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex};
use tracing::info;
use uuid::Uuid;

use orchestr8r_core::types::{
    CheckpointAction, CheckpointResponse, DaemonCommand, DaemonEvent, DaemonResponse, EngineEvent,
    WorkflowDef, WorkflowRun,
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
    let _ = std::fs::remove_file(&socket_path); // remove stale socket if present

    let workflows = load_workflows_from_dir(&config_dir.join("workflows"))?;

    let storage = Arc::new(
        SqliteStorage::open(&data_dir.join("state.db")).context("open storage")?,
    );
    let notifier: Arc<dyn Notifier> = Arc::new(DesktopNotifier);

    let (event_tx, mut event_rx) = mpsc::channel::<EngineEvent>(256);
    let manager = Arc::new(Mutex::new(WorkflowManager::new(
        storage,
        data_dir.clone(),
        event_tx,
        notifier,
    )));

    {
        let mut mgr = manager.lock().await;
        for wf in workflows {
            mgr.register(wf);
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
                EngineEvent::WorkflowRegistered { name, kind } => {
                    DaemonEvent::WorkflowRegistered { name, kind }
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

    let listener = UnixListener::bind(&socket_path).context("bind unix socket")?;
    info!(socket = ?socket_path, "Daemon started — all workflows dormant");

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_ctrlc = shutdown.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Ctrl+C received, shutting down");
        shutdown_ctrlc.store(true, Ordering::Relaxed);
    });

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
                tokio::spawn(handle_connection(stream, mgr, pending, runs, subs));
            }
            Ok(Err(e)) => {
                tracing::error!("accept error: {}", e);
                break;
            }
            Err(_) => {} // timeout — check shutdown flag and continue
        }
    }

    let _ = std::fs::remove_file(&socket_path);
    info!("Daemon stopped");
    Ok(())
}

async fn handle_connection(
    stream: UnixStream,
    manager: Arc<Mutex<WorkflowManager>>,
    pending_checkpoints: Arc<std::sync::Mutex<HashMap<Uuid, PendingEntry>>>,
    recent_runs: Arc<std::sync::Mutex<HashMap<Uuid, WorkflowRun>>>,
    subscribers: Arc<std::sync::Mutex<Vec<mpsc::Sender<DaemonEvent>>>>,
) {
    let (reader, mut writer) = stream.into_split();
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
                    name: wf.name.clone(),
                    kind: wf.kind,
                }).await;
                let _ = write_json(&mut writer, &DaemonEvent::WorkflowStateChanged {
                    name: wf.name,
                    state: wf.state,
                }).await;
            }
            // Replay run snapshots (sorted by started_at so the UI sees them in order)
            let mut runs: Vec<WorkflowRun> = recent_runs
                .lock()
                .unwrap()
                .values()
                .cloned()
                .collect();
            runs.sort_by_key(|r| r.started_at);
            for run in runs {
                let _ = write_json(&mut writer, &DaemonEvent::RunUpdated(run)).await;
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
    }
}

fn load_workflows_from_dir(dir: &Path) -> anyhow::Result<Vec<WorkflowDef>> {
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
        workflows.push(def);
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
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    value: &T,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    Ok(())
}
