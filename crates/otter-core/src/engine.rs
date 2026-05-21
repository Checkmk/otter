use chrono::Utc;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::process::{inject_isolated_env, subprocess_path, PrependScriptsDir};
use crate::resource_limiter::build_limiter;
use crate::sandbox::resolve_sandbox_config;
use crate::session::AgentSessionManager;
use crate::steps::StepExecutor;
use crate::triggers::build_trigger;
use crate::types::StepError;
use crate::types::{
    EngineEvent, LogEntry, RunOutcome, RunStatus, StepContext, StepType, StorageBackend,
    TriggerEvent, WorkflowDef, WorkflowRun, WorkflowType, WorkspaceConfig, WorkspaceSource,
};
use crate::workspace::{cleanup_workspace, resolve_workspace};
use otter_notify::{NoOpNotifier, Notifier};
use otter_secrets::{NoOpSecretStore, SecretStore};
use tokio::sync::mpsc;

pub struct Engine {
    executors: Vec<Box<dyn StepExecutor>>,
    storage: Arc<dyn StorageBackend>,
    scratch_base: std::path::PathBuf,
    notifier: Arc<dyn Notifier>,
    scripts_dir: Option<std::path::PathBuf>,
    secret_store: Arc<dyn SecretStore>,
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
            scripts_dir: None,
            secret_store: Arc::new(NoOpSecretStore),
        }
    }

    pub fn new_with_scripts_dir(
        storage: Arc<dyn StorageBackend>,
        scratch_base: std::path::PathBuf,
        notifier: Arc<dyn Notifier>,
        scripts_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            executors: crate::steps::registry(),
            storage,
            scratch_base,
            notifier,
            scripts_dir,
            secret_store: Arc::new(NoOpSecretStore),
        }
    }

    pub fn new_with_secret_store(
        storage: Arc<dyn StorageBackend>,
        scratch_base: std::path::PathBuf,
        notifier: Arc<dyn Notifier>,
        scripts_dir: Option<std::path::PathBuf>,
        secret_store: Arc<dyn SecretStore>,
    ) -> Self {
        Self {
            executors: crate::steps::registry(),
            storage,
            scratch_base,
            notifier,
            scripts_dir,
            secret_store,
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
            scripts_dir: None,
            secret_store: Arc::new(NoOpSecretStore),
        }
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
        match workflow.workflow_type {
            WorkflowType::Looping => self.run_looping(workflow, shutdown, ui_tx).await,
            WorkflowType::Triggered => self.run_triggered(workflow, shutdown, ui_tx).await,
        }
    }

    async fn run_looping(
        &self,
        workflow: &WorkflowDef,
        shutdown: Arc<AtomicBool>,
        ui_tx: Option<mpsc::Sender<EngineEvent>>,
    ) -> anyhow::Result<()> {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                info!("Shutdown requested, stopping after current iteration");
                break;
            }

            let mut run = WorkflowRun::new(workflow.name.clone());
            let scratch_dir = self.scratch_base.join(run.id.to_string());
            std::fs::create_dir_all(&scratch_dir)?;

            let workspace_dir = resolve_workspace(
                workflow.workspace.as_ref(),
                &workflow.name,
                run.id,
                &scratch_dir,
                self.secret_store.as_ref(),
            )
            .await?;
            run.workspace_dir = workspace_dir.clone();
            let workspace_type = workspace_type_label(workflow.workspace.as_ref());
            let effective_dir = workspace_dir.as_deref().unwrap_or(&scratch_dir);
            info!(run_id = %run.id, workflow = %workflow.name, workspace_type, workspace = %effective_dir.display(), "Starting looping workflow iteration");

            self.storage.save_workflow_run(&run)?;
            Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));

            let run_start_entry = LogEntry {
                run_id: run.id,
                iteration: run.iteration,
                step_index: usize::MAX,
                step_type: "run_start".to_string(),
                stdout: format!(
                    "\nRun ID: {}\nWorkspace ({}): {}",
                    run.id,
                    workspace_type,
                    effective_dir.display()
                ),
                stderr: String::new(),
                exit_code: None,
                accepted: None,
                feedback: None,
                timestamp: Utc::now(),
            };
            self.storage.append_log(run_start_entry.clone())?;
            Self::emit(&ui_tx, EngineEvent::LogAppended(run_start_entry));

            let session_manager = Arc::new(AgentSessionManager::new());

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

            let outcome = if stop {
                run.status.as_outcome().unwrap_or(RunOutcome::Stopped)
            } else {
                run.status = RunStatus::Completed;
                self.storage.update_workflow_run(&run)?;
                Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
                RunOutcome::Success
            };

            self.execute_finally_steps(
                workflow,
                &run,
                &outcome,
                &scratch_dir,
                workspace_dir.as_deref(),
                &ui_tx,
            )
            .await;

            session_manager.cleanup().await;

            if stop {
                break;
            }

            info!(run_id = %run.id, "Looping workflow iteration completed");
        }

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

        let trigger = build_trigger(
            trigger_def,
            &workflow.name,
            &data_dir,
            &self.scratch_base,
            workflow.workspace.as_ref(),
            self.scripts_dir.as_deref(),
            self.secret_store.clone(),
        )?;

        let (trigger_tx, mut trigger_rx) = mpsc::channel::<TriggerEvent>(32);

        // Fire one initial event for manual triggers; polling triggers wait for their interval
        if matches!(trigger_def, crate::types::TriggerDef::Manual) {
            if let Err(e) = trigger.fire_once(trigger_tx.clone()).await {
                error!(workflow = %workflow.name, "Failed to fire initial trigger: {}", e);
                return Err(anyhow::anyhow!("Failed to fire initial trigger: {}", e));
            }
        }

        let trigger_for_spawn = trigger.clone();
        let trigger_handle = tokio::spawn(async move {
            if let Err(e) = trigger_for_spawn.subscribe(trigger_tx).await {
                error!("Trigger subscribe error: {}", e);
            }
        });

        let mut queued: VecDeque<TriggerEvent> = VecDeque::new();

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

            let run_status = self
                .run_once(workflow, Some(&event), shutdown.clone(), ui_tx.clone())
                .await?;
            trigger
                .on_run_completed(&event.payload, run_status == RunStatus::Completed)
                .await;

            // Collect events that arrived during the run
            while let Ok(e) = trigger_rx.try_recv() {
                queued.push_back(e);
            }

            // Manual triggers should only fire once per start; exit after the first event completes
            if matches!(trigger_def, crate::types::TriggerDef::Manual) {
                info!(workflow = %workflow.name, "Manual trigger completed, exiting");
                trigger_handle.abort();
                break;
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
    ) -> anyhow::Result<RunStatus> {
        let run_id = event
            .and_then(|e| e.preallocated_run_id)
            .unwrap_or_else(uuid::Uuid::new_v4);
        let mut run = WorkflowRun::new(workflow.name.clone());
        run.id = run_id;
        if let Some(event) = event {
            run.trigger_payload = Some(event.payload.clone());
        }
        let scratch_dir = self.scratch_base.join(run.id.to_string());
        std::fs::create_dir_all(&scratch_dir)?;

        self.storage.save_workflow_run(&run)?;
        Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));

        let session_manager = Arc::new(AgentSessionManager::new());

        let workspace_dir = match resolve_workspace(
            workflow.workspace.as_ref(),
            &workflow.name,
            run.id,
            &scratch_dir,
            self.secret_store.as_ref(),
        )
        .await
        {
            Ok(dir) => dir,
            Err(e) => {
                run.status = RunStatus::Failed;
                self.storage.update_workflow_run(&run)?;
                Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
                let entry = LogEntry {
                    run_id: run.id,
                    iteration: run.iteration,
                    step_index: usize::MAX,
                    step_type: "workspace_setup".to_string(),
                    stdout: String::new(),
                    stderr: format!("Workspace setup failed: {e}"),
                    exit_code: Some(1),
                    accepted: None,
                    feedback: None,
                    timestamp: Utc::now(),
                };
                self.storage.append_log(entry.clone())?;
                Self::emit(&ui_tx, EngineEvent::LogAppended(entry));
                return Ok(run.status);
            }
        };
        run.workspace_dir = workspace_dir.clone();
        self.storage.update_workflow_run(&run)?;
        Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
        let workspace_type = workspace_type_label(workflow.workspace.as_ref());
        let effective_dir = workspace_dir.as_deref().unwrap_or(&scratch_dir);
        info!(run_id = %run.id, workflow = %workflow.name, workspace_type, workspace = %effective_dir.display(), "Starting triggered workflow run");

        let run_start_entry = LogEntry {
            run_id: run.id,
            iteration: run.iteration,
            step_index: usize::MAX,
            step_type: "run_start".to_string(),
            stdout: format!(
                "\nRun ID: {}\nWorkspace ({}): {}",
                run.id,
                workspace_type,
                effective_dir.display()
            ),
            stderr: String::new(),
            exit_code: None,
            accepted: None,
            feedback: None,
            timestamp: Utc::now(),
        };
        self.storage.append_log(run_start_entry.clone())?;
        Self::emit(&ui_tx, EngineEvent::LogAppended(run_start_entry));

        // Run the pending context command (from a polling trigger) now that the workspace is ready.
        // Failure here is treated like a step failure: skip execute_steps but still run finally
        // (including workspace cleanup) so we don't leak pool slots / worktrees.
        let mut context_failed = false;
        if let Some(ctx) = event.and_then(|e| e.pending_context.as_ref()) {
            let ctx_dir = match &workspace_dir {
                Some(ws) => ws.join("trigger-context"),
                None => scratch_dir.join("trigger-context"),
            };
            std::fs::create_dir_all(&ctx_dir)?;
            info!(run_id = %run.id, "running context command for hash {}", ctx.hash);
            let resolved = self.secret_store.resolve(&ctx.secrets).map_err(|e| {
                anyhow::anyhow!("secret resolution for context command failed: {}", e)
            })?;
            let mut cmd = tokio::process::Command::new(&ctx.command[0]);
            cmd.args(&ctx.command[1..])
                .arg(&ctx.hash)
                .arg(subprocess_path(&ctx_dir));
            inject_isolated_env(&mut cmd, &resolved, true);
            cmd.prepend_scripts_dir(self.scripts_dir.as_deref());
            let out = cmd.output().await?;
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!(run_id = %run.id, "context command failed for hash {}: {} (stderr: {})", ctx.hash, out.status, stderr);
                run.status = RunStatus::Failed;
                self.storage.update_workflow_run(&run)?;
                Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
                context_failed = true;
            }
        }

        let stop = if context_failed {
            true
        } else {
            self.execute_steps(
                workflow,
                &mut run,
                &scratch_dir,
                workspace_dir.as_deref(),
                &session_manager,
                &shutdown,
                &ui_tx,
            )
            .await?
        };

        let outcome = if !stop {
            run.status = RunStatus::Completed;
            self.storage.update_workflow_run(&run)?;
            Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
            RunOutcome::Success
        } else {
            run.status.as_outcome().unwrap_or(RunOutcome::Stopped)
        };

        self.execute_finally_steps(
            workflow,
            &run,
            &outcome,
            &scratch_dir,
            workspace_dir.as_deref(),
            &ui_tx,
        )
        .await;

        session_manager.cleanup().await;
        info!(run_id = %run.id, "Triggered workflow run ended");
        Ok(run.status)
    }

    /// Runs all steps once. Returns `Ok(true)` if execution should stop (failed or shutdown).
    #[allow(clippy::too_many_arguments)]
    async fn execute_steps(
        &self,
        workflow: &WorkflowDef,
        run: &mut WorkflowRun,
        scratch_dir: &std::path::Path,
        workspace_dir: Option<&std::path::Path>,
        session_manager: &Arc<AgentSessionManager>,
        shutdown: &Arc<AtomicBool>,
        ui_tx: &Option<mpsc::Sender<EngineEvent>>,
    ) -> anyhow::Result<bool> {
        for (i, step_def) in workflow.steps.iter().enumerate() {
            if shutdown.load(Ordering::Relaxed) {
                info!("Shutdown requested, stopping after current step");
                run.status = RunStatus::Stopped;
                self.storage.update_workflow_run(run)?;
                Self::emit(ui_tx, EngineEvent::RunUpdated(run.clone()));
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

            let storage_log = self.storage.clone();
            let ui_tx_log = ui_tx.clone();
            let log_fn: Arc<dyn Fn(crate::types::LogEntry) + Send + Sync> =
                Arc::new(move |entry: crate::types::LogEntry| {
                    let _ = storage_log.append_log(entry.clone());
                    Self::emit(&ui_tx_log, EngineEvent::LogAppended(entry));
                });

            let progress_fn: Option<Arc<dyn Fn(crate::types::ProgressChunk) + Send + Sync>> =
                ui_tx.as_ref().map(|tx| {
                    let tx = tx.clone();
                    let run_id = run.id;
                    Arc::new(move |chunk: crate::types::ProgressChunk| {
                        let _ = tx.try_send(EngineEvent::StepProgress {
                            run_id,
                            step_index: i,
                            chunk,
                        });
                    }) as Arc<dyn Fn(crate::types::ProgressChunk) + Send + Sync>
                });

            let sandbox_config = resolve_sandbox_config(
                workflow.sandbox.as_ref(),
                step_def,
                workspace_dir.unwrap_or(scratch_dir),
                self.scripts_dir.as_deref(),
                workflow.resources.as_ref(),
                scratch_dir,
            );

            let ctx = StepContext {
                run_id: run.id,
                workflow_name: workflow.name.clone(),
                iteration: run.iteration,
                step_index: i,
                scratch_dir: scratch_dir.to_path_buf(),
                workspace_dir: workspace_dir.map(|p| p.to_owned()),
                scripts_dir: self.scripts_dir.clone(),
                checkpoint_tx: ui_tx.clone(),
                session_manager: Some(session_manager.clone()),
                notifier: self.notifier.clone(),
                log_fn: Some(log_fn),
                progress_fn,
                resource_limiter: build_limiter(workflow.resources.as_ref()),
                secret_store: self.secret_store.clone(),
                sandbox_config,
            };

            info!(step = i, step_type = %step_def.step_type, command = ?step_def.command, "Executing step");

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
                Err(StepError::Rejected) => {
                    warn!(
                        step = i,
                        iteration = run.iteration,
                        "Step rejected — stopping workflow"
                    );
                    let entry = LogEntry {
                        run_id: run.id,
                        iteration: run.iteration,
                        step_index: i,
                        step_type: step_def.step_type.to_string(),
                        stdout: "Stopped".to_string(),
                        stderr: String::new(),
                        exit_code: None,
                        accepted: Some(false),
                        feedback: None,
                        timestamp: Utc::now(),
                    };
                    self.storage.append_log(entry.clone())?;
                    Self::emit(ui_tx, EngineEvent::LogAppended(entry));
                    run.status = RunStatus::Stopped;
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

    pub async fn run_finally(
        &self,
        workflow: &WorkflowDef,
        run: &WorkflowRun,
        outcome: RunOutcome,
        ui_tx: Option<mpsc::Sender<EngineEvent>>,
    ) {
        let scratch_dir = self.scratch_base.join(run.id.to_string());
        let _ = std::fs::create_dir_all(&scratch_dir);
        self.execute_finally_steps(
            workflow,
            run,
            &outcome,
            &scratch_dir,
            run.workspace_dir.as_deref(),
            &ui_tx,
        )
        .await;
    }

    /// Runs `[[finally]]` steps that match the given run outcome.
    /// Errors from individual steps are logged as warnings and do not change `run.status`.
    async fn execute_finally_steps(
        &self,
        workflow: &WorkflowDef,
        run: &WorkflowRun,
        outcome: &RunOutcome,
        scratch_dir: &std::path::Path,
        workspace_dir: Option<&std::path::Path>,
        ui_tx: &Option<mpsc::Sender<EngineEvent>>,
    ) {
        for (i, finally_def) in workflow.finally.iter().enumerate() {
            if !finally_def.applies_to(outcome) {
                continue;
            }

            let step_index = workflow.steps.len() + i;
            let step_def = &finally_def.step;

            let storage_log = self.storage.clone();
            let ui_tx_log = ui_tx.clone();
            let log_fn: Arc<dyn Fn(LogEntry) + Send + Sync> = Arc::new(move |entry: LogEntry| {
                let _ = storage_log.append_log(entry.clone());
                Self::emit(&ui_tx_log, EngineEvent::LogAppended(entry));
            });

            let sandbox_config = resolve_sandbox_config(
                workflow.sandbox.as_ref(),
                step_def,
                workspace_dir.unwrap_or(scratch_dir),
                self.scripts_dir.as_deref(),
                workflow.resources.as_ref(),
                scratch_dir,
            );

            let ctx = StepContext {
                run_id: run.id,
                workflow_name: workflow.name.clone(),
                iteration: run.iteration,
                step_index,
                scratch_dir: scratch_dir.to_path_buf(),
                workspace_dir: workspace_dir.map(|p| p.to_owned()),
                scripts_dir: self.scripts_dir.clone(),
                checkpoint_tx: None,
                session_manager: None,
                notifier: self.notifier.clone(),
                log_fn: Some(log_fn),
                progress_fn: None,
                resource_limiter: build_limiter(workflow.resources.as_ref()),
                secret_store: self.secret_store.clone(),
                sandbox_config,
            };

            info!(step = i, step_type = %step_def.step_type, "Executing finally step");

            let executor = match self.find_executor(step_def.step_type) {
                Some(e) => e,
                None => {
                    warn!(step_type = %step_def.step_type, "No executor for finally step, skipping");
                    continue;
                }
            };

            match executor.execute(step_def, &ctx).await {
                Ok(output) => {
                    let entry = LogEntry {
                        run_id: run.id,
                        iteration: run.iteration,
                        step_index,
                        step_type: format!("finally:{}", step_def.step_type),
                        stdout: output.stdout,
                        stderr: output.stderr,
                        exit_code: output.exit_code,
                        accepted: output.accepted,
                        feedback: None,
                        timestamp: Utc::now(),
                    };
                    let _ = self.storage.append_log(entry.clone());
                    Self::emit(ui_tx, EngineEvent::LogAppended(entry));
                    info!(step = i, "Finally step completed");
                }
                Err(e) => {
                    warn!(step = i, error = %e, "Finally step failed (ignoring)");
                    let entry = LogEntry {
                        run_id: run.id,
                        iteration: run.iteration,
                        step_index,
                        step_type: format!("finally:{}", step_def.step_type),
                        stdout: String::new(),
                        stderr: e.to_string(),
                        exit_code: Some(1),
                        accepted: None,
                        feedback: None,
                        timestamp: Utc::now(),
                    };
                    let _ = self.storage.append_log(entry.clone());
                    Self::emit(ui_tx, EngineEvent::LogAppended(entry));
                }
            }
        }

        // Implicit workspace cleanup runs after all user finally steps, so a user
        // finally step that inspects the workspace still sees a live one. Failures
        // are warning-logged (same posture as user finally-step failures above).
        if let Err(e) = cleanup_workspace(workflow.workspace.as_ref(), workspace_dir, outcome).await
        {
            warn!(error = %e, "Workspace cleanup failed (ignoring)");
        }
    }
}

fn workspace_type_label(config: Option<&WorkspaceConfig>) -> &'static str {
    match config {
        None => "scratch",
        Some(c) => match &c.source {
            WorkspaceSource::Scratch => "scratch",
            WorkspaceSource::Fixed { .. } => "fixed",
            WorkspaceSource::Script { .. } => "script",
            WorkspaceSource::Git { .. } => {
                if c.pool.is_some() {
                    "git-pooled"
                } else {
                    "git"
                }
            }
        },
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
