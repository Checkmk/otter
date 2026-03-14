use chrono::Utc;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::session::AgentSessionManager;
use crate::steps::StepExecutor;
use crate::triggers::build_trigger;
use crate::types::StepError;
use crate::types::{
    EngineEvent, LogEntry, RunStatus, StepContext, StepType, StorageBackend, TriggerEvent,
    WorkflowDef, WorkflowKind, WorkflowRun,
};
use orchestr8r_notify::{NoOpNotifier, Notifier};
use tokio::sync::mpsc;

pub struct Engine {
    executors: Vec<Box<dyn StepExecutor>>,
    storage: Arc<dyn StorageBackend>,
    scratch_base: std::path::PathBuf,
    notifier: Arc<dyn Notifier>,
    paused: Arc<AtomicBool>,
}

impl Engine {
    pub fn new(
        storage: Arc<dyn StorageBackend>,
        scratch_base: std::path::PathBuf,
        notifier: Arc<dyn Notifier>,
    ) -> Self {
        Self {
            executors: crate::steps::registry(),
            storage,
            scratch_base,
            notifier,
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_executors(
        storage: Arc<dyn StorageBackend>,
        scratch_base: std::path::PathBuf,
        executors: Vec<Box<dyn StepExecutor>>,
    ) -> Self {
        Self {
            executors,
            storage,
            scratch_base,
            notifier: Arc::new(NoOpNotifier),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns a clone of the pause flag. Set to `true` to pause an indefinite workflow
    /// between iterations; clear to resume.
    pub fn paused_flag(&self) -> Arc<AtomicBool> {
        self.paused.clone()
    }

    fn find_executor(&self, step_type: StepType) -> Option<&dyn StepExecutor> {
        self.executors
            .iter()
            .find(|e| e.step_type() == step_type)
            .map(|e| e.as_ref())
    }

    fn emit(tx: &Option<mpsc::Sender<EngineEvent>>, event: EngineEvent) {
        if let Some(tx) = tx {
            let _ = tx.try_send(event);
        }
    }

    pub async fn run(
        &self,
        workflow: &WorkflowDef,
        shutdown: Arc<AtomicBool>,
        ui_tx: Option<mpsc::Sender<EngineEvent>>,
    ) -> anyhow::Result<()> {
        match workflow.kind {
            WorkflowKind::Indefinite => self.run_indefinite(workflow, shutdown, ui_tx).await,
            WorkflowKind::Triggered => self.run_triggered(workflow, shutdown, ui_tx).await,
        }
    }

    async fn run_indefinite(
        &self,
        workflow: &WorkflowDef,
        shutdown: Arc<AtomicBool>,
        ui_tx: Option<mpsc::Sender<EngineEvent>>,
    ) -> anyhow::Result<()> {
        Self::emit(&ui_tx, EngineEvent::WorkflowRegistered { name: workflow.name.clone(), kind: workflow.kind.clone() });
        let mut run = WorkflowRun::new(workflow.name.clone());
        let scratch_dir = self.scratch_base.join(run.id.to_string());
        std::fs::create_dir_all(&scratch_dir)?;

        self.storage.save_workflow_run(&run)?;
        Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
        info!(run_id = %run.id, workflow = %workflow.name, "Starting indefinite workflow run");

        let session_manager = Arc::new(AgentSessionManager::new());

        let workspace_dir: Option<std::path::PathBuf> = match workflow.workspace.as_deref() {
            Some(path) => {
                let resolved = std::fs::canonicalize(path).map_err(|e| {
                    anyhow::anyhow!("cannot resolve workspace path '{}': {}", path, e)
                })?;
                if !resolved.is_dir() {
                    return Err(anyhow::anyhow!("workspace path '{}' is not a directory", resolved.display()));
                }
                Some(resolved)
            }
            None => None,
        };

        loop {
            if shutdown.load(Ordering::Relaxed) {
                info!("Shutdown requested, stopping after current iteration");
                break;
            }

            while self.paused.load(Ordering::Relaxed) {
                if shutdown.load(Ordering::Relaxed) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            info!(iteration = run.iteration, "Starting iteration");

            let stop = self
                .execute_steps(
                    workflow,
                    &mut run,
                    &scratch_dir,
                    workspace_dir.as_deref(),
                    &session_manager,
                    &shutdown,
                    &ui_tx,
                )
                .await?;

            if stop {
                break;
            }

            run.iteration += 1;
            run.status = RunStatus::Running;
            self.storage.update_workflow_run(&run)?;
            Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
        }

        session_manager.cleanup().await;
        info!(run_id = %run.id, iterations = run.iteration, "Workflow run ended");
        Ok(())
    }

    async fn run_triggered(
        &self,
        workflow: &WorkflowDef,
        shutdown: Arc<AtomicBool>,
        ui_tx: Option<mpsc::Sender<EngineEvent>>,
    ) -> anyhow::Result<()> {
        let trigger_def = workflow.trigger.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "triggered workflow '{}' missing 'trigger' field",
                workflow.name
            )
        })?;

        let data_dir = self
            .scratch_base
            .parent()
            .unwrap_or(&self.scratch_base)
            .to_path_buf();

        let workspace_dir: Option<std::path::PathBuf> = match workflow.workspace.as_deref() {
            Some(path) => Some(std::fs::canonicalize(path).map_err(|e| {
                anyhow::anyhow!("cannot resolve workspace path '{}': {}", path, e)
            })?),
            None => None,
        };

        let trigger = build_trigger(
            trigger_def,
            &workflow.name,
            &data_dir,
            &self.scratch_base,
            workspace_dir.as_deref(),
        )?;

        let (trigger_tx, mut trigger_rx) = mpsc::channel::<TriggerEvent>(32);

        let trigger_handle = tokio::spawn(async move {
            if let Err(e) = trigger.subscribe(trigger_tx).await {
                error!("Trigger subscribe error: {}", e);
            }
        });

        let mut queued: VecDeque<TriggerEvent> = VecDeque::new();

        Self::emit(&ui_tx, EngineEvent::WorkflowRegistered { name: workflow.name.clone(), kind: workflow.kind.clone() });
        info!(
            workflow = %workflow.name,
            "Waiting for trigger events"
        );

        loop {
            if shutdown.load(Ordering::Relaxed) {
                info!(workflow = %workflow.name, "Shutdown flag set, exiting trigger loop");
                trigger_handle.abort();
                break;
            }

            // Always drain any pending events into the queue first
            while let Ok(e) = trigger_rx.try_recv() {
                queued.push_back(e);
            }

            let event = if let Some(e) = queued.pop_front() {
                e
            } else {
                // Queue is empty, wait for new events with timeout
                tokio::select! {
                    maybe = trigger_rx.recv() => {
                        match maybe {
                            Some(e) => e,
                            None => {
                                info!(workflow = %workflow.name, "Trigger channel closed, exiting");
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                        continue;
                    }
                }
            };

            info!(
                trigger = %event.source,
                workflow = %workflow.name,
                "Trigger fired, starting run"
            );

            self.run_once(workflow, Some(&event), shutdown.clone(), ui_tx.clone()).await?;

            // Collect events that arrived during the run
            while let Ok(e) = trigger_rx.try_recv() {
                queued.push_back(e);
            }
        }

        Ok(())
    }

    pub async fn run_once(
        &self,
        workflow: &WorkflowDef,
        event: Option<&TriggerEvent>,
        shutdown: Arc<AtomicBool>,
        ui_tx: Option<mpsc::Sender<EngineEvent>>,
    ) -> anyhow::Result<()> {
        let run_id = event.and_then(|e| e.preallocated_run_id).unwrap_or_else(uuid::Uuid::new_v4);
        let mut run = WorkflowRun::new(workflow.name.clone());
        run.id = run_id;
        let scratch_dir = self.scratch_base.join(run.id.to_string());
        std::fs::create_dir_all(&scratch_dir)?;

        self.storage.save_workflow_run(&run)?;
        Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
        info!(run_id = %run.id, workflow = %workflow.name, "Starting triggered workflow run");

        let session_manager = Arc::new(AgentSessionManager::new());

        let workspace_dir: Option<std::path::PathBuf> = match workflow.workspace.as_deref() {
            Some(path) => {
                let resolved = std::fs::canonicalize(path).map_err(|e| {
                    anyhow::anyhow!("cannot resolve workspace path '{}': {}", path, e)
                })?;
                if !resolved.is_dir() {
                    return Err(anyhow::anyhow!("workspace path '{}' is not a directory", resolved.display()));
                }
                Some(resolved)
            }
            None => None,
        };

        let stop = self
            .execute_steps(
                workflow,
                &mut run,
                &scratch_dir,
                workspace_dir.as_deref(),
                &session_manager,
                &shutdown,
                &ui_tx,
            )
            .await?;

        if !stop {
            run.status = RunStatus::Completed;
            self.storage.update_workflow_run(&run)?;
            Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
        }

        session_manager.cleanup().await;
        info!(run_id = %run.id, "Triggered workflow run ended");
        Ok(())
    }

    /// Runs all steps once. Returns `Ok(true)` if execution should stop (failed or shutdown).
    async fn execute_steps(
        &self,
        workflow: &WorkflowDef,
        run: &mut WorkflowRun,
        scratch_dir: &std::path::PathBuf,
        workspace_dir: Option<&std::path::Path>,
        session_manager: &Arc<AgentSessionManager>,
        shutdown: &Arc<AtomicBool>,
        ui_tx: &Option<mpsc::Sender<EngineEvent>>,
    ) -> anyhow::Result<bool> {
        for (i, step_def) in workflow.steps.iter().enumerate() {
            if shutdown.load(Ordering::Relaxed) {
                info!("Shutdown requested, stopping after current step");
                return Ok(true);
            }

            run.current_step = i;
            run.status = if step_def.step_type == StepType::Checkpoint {
                RunStatus::WaitingCheckpoint
            } else {
                RunStatus::Running
            };
            self.storage.update_workflow_run(run)?;
            Self::emit(ui_tx, EngineEvent::RunUpdated(run.clone()));

            let ctx = StepContext {
                run_id: run.id,
                workflow_name: workflow.name.clone(),
                iteration: run.iteration,
                step_index: i,
                scratch_dir: scratch_dir.clone(),
                workspace_dir: workspace_dir.map(|p| p.to_owned()),
                checkpoint_tx: ui_tx.clone(),
                session_manager: Some(session_manager.clone()),
                notifier: self.notifier.clone(),
            };

            info!(step = i, step_type = %step_def.step_type, "Executing step");

            let executor = match self.find_executor(step_def.step_type) {
                Some(e) => e,
                None => {
                    error!(step_type = %step_def.step_type, "No executor found for step type");
                    run.status = RunStatus::Failed;
                    self.storage.update_workflow_run(run)?;
                    Self::emit(ui_tx, EngineEvent::RunUpdated(run.clone()));
                    return Ok(true);
                }
            };

            match executor.execute(step_def, &ctx).await {
                Ok(output) => {
                    for extra in &output.extra_logs {
                        let entry = LogEntry {
                            run_id: run.id,
                            iteration: run.iteration,
                            step_index: i,
                            step_type: extra.step_type.clone(),
                            stdout: extra.stdout.clone(),
                            stderr: extra.stderr.clone(),
                            exit_code: extra.exit_code,
                            accepted: None,
                            feedback: None,
                            timestamp: Utc::now(),
                        };
                        self.storage.append_log(entry.clone())?;
                        Self::emit(ui_tx, EngineEvent::LogAppended(entry));
                    }

                    let entry = LogEntry {
                        run_id: run.id,
                        iteration: run.iteration,
                        step_index: i,
                        step_type: step_def.step_type.to_string(),
                        stdout: output.stdout.clone(),
                        stderr: output.stderr.clone(),
                        exit_code: output.exit_code,
                        accepted: output.accepted,
                        feedback: None,
                        timestamp: Utc::now(),
                    };
                    self.storage.append_log(entry.clone())?;
                    Self::emit(ui_tx, EngineEvent::LogAppended(entry));

                    info!(step = i, "Step completed successfully");
                }
                Err(StepError::Rejected(extra)) => {
                    warn!(step = i, iteration = run.iteration, "Step rejected — stopping workflow");
                    for log in &extra {
                        let entry = LogEntry {
                            run_id: run.id,
                            iteration: run.iteration,
                            step_index: i,
                            step_type: log.step_type.clone(),
                            stdout: log.stdout.clone(),
                            stderr: log.stderr.clone(),
                            exit_code: log.exit_code,
                            accepted: None,
                            feedback: None,
                            timestamp: Utc::now(),
                        };
                        self.storage.append_log(entry.clone())?;
                        Self::emit(ui_tx, EngineEvent::LogAppended(entry));
                    }
                    let entry = LogEntry {
                        run_id: run.id,
                        iteration: run.iteration,
                        step_index: i,
                        step_type: step_def.step_type.to_string(),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: None,
                        accepted: Some(false),
                        feedback: None,
                        timestamp: Utc::now(),
                    };
                    self.storage.append_log(entry.clone())?;
                    Self::emit(ui_tx, EngineEvent::LogAppended(entry));
                    run.status = RunStatus::Failed;
                    self.storage.update_workflow_run(run)?;
                    Self::emit(ui_tx, EngineEvent::RunUpdated(run.clone()));
                    return Ok(true);
                }
                Err(e) => {
                    error!(step = i, error = %e, "Step failed");
                    let entry = LogEntry {
                        run_id: run.id,
                        iteration: run.iteration,
                        step_index: i,
                        step_type: step_def.step_type.to_string(),
                        stdout: String::new(),
                        stderr: e.to_string(),
                        exit_code: Some(1),
                        accepted: None,
                        feedback: None,
                        timestamp: Utc::now(),
                    };
                    self.storage.append_log(entry.clone())?;
                    Self::emit(ui_tx, EngineEvent::LogAppended(entry));
                    run.status = RunStatus::Failed;
                    self.storage.update_workflow_run(run)?;
                    Self::emit(ui_tx, EngineEvent::RunUpdated(run.clone()));
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
