use chrono::Utc;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::agent_runner::AgentRunner;
use crate::session::AgentSessionManager;
use crate::steps::StepExecutor;
use crate::triggers::build_trigger;
use crate::types::StepError;
use crate::types::{
    EngineEvent, LogEntry, RunStatus, StepContext, StepType, StorageBackend, TriggerEvent,
    WorkflowDef, WorkflowKind, WorkflowRun,
};
use tokio::sync::mpsc;

pub struct Engine {
    executors: Vec<Box<dyn StepExecutor>>,
    storage: Arc<dyn StorageBackend>,
    scratch_base: std::path::PathBuf,
    agent_runner: Arc<dyn AgentRunner>,
}

impl Engine {
    pub fn new(
        storage: Arc<dyn StorageBackend>,
        scratch_base: std::path::PathBuf,
        agent_runner: Arc<dyn AgentRunner>,
    ) -> Self {
        Self {
            executors: crate::steps::registry(),
            storage,
            scratch_base,
            agent_runner,
        }
    }

    pub fn with_executors(
        storage: Arc<dyn StorageBackend>,
        scratch_base: std::path::PathBuf,
        agent_runner: Arc<dyn AgentRunner>,
        executors: Vec<Box<dyn StepExecutor>>,
    ) -> Self {
        Self {
            executors,
            storage,
            scratch_base,
            agent_runner,
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
        let mut run = WorkflowRun::new(workflow.name.clone());
        let scratch_dir = self.scratch_base.join(run.id.to_string());
        std::fs::create_dir_all(&scratch_dir)?;

        self.storage.save_workflow_run(&run)?;
        Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
        info!(run_id = %run.id, workflow = %workflow.name, "Starting indefinite workflow run");

        let session_manager = Arc::new(AgentSessionManager::new(self.agent_runner.clone()));
        let mut workspace_dir: Option<std::path::PathBuf> = None;

        loop {
            if shutdown.load(Ordering::Relaxed) {
                info!("Shutdown requested, stopping after current iteration");
                break;
            }

            info!(iteration = run.iteration, "Starting iteration");

            let stop = self
                .execute_steps(
                    workflow,
                    &mut run,
                    &scratch_dir,
                    &mut workspace_dir,
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

        let trigger = build_trigger(trigger_def, &workflow.name, &data_dir)?;

        let (trigger_tx, mut trigger_rx) = mpsc::channel::<TriggerEvent>(32);

        tokio::spawn(async move {
            if let Err(e) = trigger.subscribe(trigger_tx).await {
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
                break;
            }

            let event = if let Some(e) = queued.pop_front() {
                e
            } else {
                tokio::select! {
                    maybe = trigger_rx.recv() => {
                        match maybe {
                            Some(e) => e,
                            None => break,
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

            self.run_once(workflow, shutdown.clone(), ui_tx.clone()).await?;

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
        shutdown: Arc<AtomicBool>,
        ui_tx: Option<mpsc::Sender<EngineEvent>>,
    ) -> anyhow::Result<()> {
        let mut run = WorkflowRun::new(workflow.name.clone());
        let scratch_dir = self.scratch_base.join(run.id.to_string());
        std::fs::create_dir_all(&scratch_dir)?;

        self.storage.save_workflow_run(&run)?;
        Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
        info!(run_id = %run.id, workflow = %workflow.name, "Starting triggered workflow run");

        let session_manager = Arc::new(AgentSessionManager::new(self.agent_runner.clone()));
        let mut workspace_dir: Option<std::path::PathBuf> = None;

        let stop = self
            .execute_steps(
                workflow,
                &mut run,
                &scratch_dir,
                &mut workspace_dir,
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
        workspace_dir: &mut Option<std::path::PathBuf>,
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
                workspace_dir: workspace_dir.clone(),
                checkpoint_tx: ui_tx.clone(),
                session_manager: Some(session_manager.clone()),
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

                    if step_def.step_type == StepType::Workspace {
                        let resolved = output.stdout.trim().to_string();
                        if !resolved.is_empty() {
                            *workspace_dir = Some(std::path::PathBuf::from(&resolved));
                        }
                    }

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
mod tests {
    use super::*;
    use crate::agent_runner::{AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec};
    use crate::storage::InMemoryStorage;
    use crate::types::{CheckpointResponse, RunStatus, StepDef, StepType, WorkflowDef, WorkflowKind};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct NoOpAgentRunner;

    #[async_trait::async_trait]
    impl AgentRunner for NoOpAgentRunner {
        async fn start(&self, _spec: AgentSpec) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
            Ok((AgentSessionHandle {
                id: "noop".to_string(),
                command: vec![],
                working_dir: std::path::PathBuf::new(),
            }, AgentOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            }))
        }
        async fn prompt(
            &self,
            _session: &AgentSessionHandle,
            _message: &str,
        ) -> Result<AgentOutput, AgentError> {
            Ok(AgentOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        }
        async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
            Ok(())
        }
    }

    fn make_engine(storage: Arc<InMemoryStorage>) -> Engine {
        let scratch = std::env::temp_dir().join("orchestr8r-tests");
        Engine::new(storage, scratch, Arc::new(NoOpAgentRunner))
    }

    /// Tracks all agent runner calls for assertions.
    struct MockAgentRunner {
        calls: Mutex<Vec<String>>,
    }

    impl MockAgentRunner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentRunner for MockAgentRunner {
        async fn start(&self, spec: AgentSpec) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("start:{}", spec.message));
            Ok((AgentSessionHandle {
                id: "mock-session".to_string(),
                command: spec.command,
                working_dir: spec.working_dir,
            }, AgentOutput {
                stdout: format!("response to: {}", spec.message),
                stderr: String::new(),
                exit_code: Some(0),
            }))
        }
        async fn prompt(
            &self,
            _session: &AgentSessionHandle,
            message: &str,
        ) -> Result<AgentOutput, AgentError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("prompt:{}", message));
            Ok(AgentOutput {
                stdout: format!("response to: {}", message),
                stderr: String::new(),
                exit_code: Some(0),
            })
        }
        async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
            self.calls.lock().unwrap().push("stop".to_string());
            Ok(())
        }
    }

    fn step_def(step_type: StepType) -> StepDef {
        StepDef {
            step_type,
            command: None,
            message: None,
            path: None,
            output_file: None,
            session: None,
        }
    }

    fn workflow(name: &str, kind: WorkflowKind, steps: Vec<StepDef>) -> WorkflowDef {
        WorkflowDef {
            name: name.to_string(),
            kind,
            trigger: None,
            steps,
        }
    }

    #[tokio::test]
    async fn shell_step_runs_and_logs() {
        // GIVEN
        let storage = Arc::new(InMemoryStorage::new());
        let engine = make_engine(storage.clone());
        let wf = workflow(
            "test-shell",
            WorkflowKind::Indefinite,
            vec![StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["echo".to_string(), "hello".to_string()]),
                message: None,
                path: None,
                output_file: None,
                session: None,
            }],
        );

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let storage_clone = storage.clone();
        let handle = tokio::spawn(async move {
            engine.run(&wf, shutdown_clone, None).await.unwrap();
            storage_clone
        });
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        shutdown.store(true, Ordering::Relaxed);
        let storage = handle.await.unwrap();

        // THEN
        let logs = storage.logs();
        assert!(!logs.is_empty(), "expected at least one log entry");
        assert_eq!(logs[0].step_type, "shell");
        assert!(logs[0].stdout.contains("hello"));
    }

    #[test]
    fn unknown_step_type_fails_deserialization() {
        // GIVEN
        let toml_str = r#"
            name = "bad"
            kind = "indefinite"
            [[steps]]
            type = "nonexistent"
        "#;

        // WHEN / THEN
        assert!(toml::from_str::<WorkflowDef>(toml_str).is_err());
    }

    #[tokio::test]
    async fn failed_shell_command_marks_run_failed() {
        // GIVEN
        let storage = Arc::new(InMemoryStorage::new());
        let engine = make_engine(storage.clone());
        let wf = workflow(
            "test-fail",
            WorkflowKind::Indefinite,
            vec![StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["false".to_string()]),
                message: None,
                path: None,
                output_file: None,
                session: None,
            }],
        );

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        engine.run(&wf, shutdown, None).await.unwrap();

        // THEN
        let runs = storage.runs();
        assert_eq!(runs.last().unwrap().status, RunStatus::Failed);
    }

    #[tokio::test]
    async fn workspace_step_sets_working_dir_for_shell() {
        // GIVEN
        let workspace = tempfile::tempdir().unwrap();
        let marker = workspace.path().join("marker.txt");

        let storage = Arc::new(InMemoryStorage::new());
        let engine = make_engine(storage.clone());
        let wf = workflow(
            "test-workspace",
            WorkflowKind::Indefinite,
            vec![
                StepDef {
                    step_type: StepType::Workspace,
                    command: None,
                    message: None,
                    path: Some(workspace.path().to_string_lossy().to_string()),
                    output_file: None,
                    session: None,
                },
                StepDef {
                    step_type: StepType::Shell,
                    command: Some(vec!["touch".to_string(), "marker.txt".to_string()]),
                    message: None,
                    path: None,
                    output_file: None,
                    session: None,
                },
            ],
        );

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            engine.run(&wf, shutdown_clone, None).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        shutdown.store(true, Ordering::Relaxed);
        handle.await.unwrap();

        // THEN
        assert!(
            marker.exists(),
            "shell step should have run in the workspace dir"
        );
    }

    #[tokio::test]
    async fn named_session_shared_across_agent_steps() {
        // GIVEN two agent steps sharing the same session name
        let storage = Arc::new(InMemoryStorage::new());
        let agent_runner = Arc::new(MockAgentRunner::new());
        let scratch = tempfile::tempdir().unwrap();
        let engine = Engine::with_executors(
            storage.clone(),
            scratch.path().to_path_buf(),
            agent_runner.clone(),
            vec![Box::new(crate::steps::agent::AgentExecutor)],
        );
        let wf = workflow(
            "test-sessions",
            WorkflowKind::Indefinite,
            vec![
                StepDef {
                    step_type: StepType::Agent,
                    command: Some(vec!["claude".to_string(), "--print".to_string()]),
                    message: Some("first prompt".to_string()),
                    session: Some("planner".to_string()),
                    ..step_def(StepType::Agent)
                },
                StepDef {
                    step_type: StepType::Agent,
                    command: None,
                    message: Some("second prompt".to_string()),
                    session: Some("planner".to_string()),
                    ..step_def(StepType::Agent)
                },
            ],
        );

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            engine.run(&wf, shutdown_clone, None).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        shutdown.store(true, Ordering::Relaxed);
        handle.await.unwrap();

        // THEN — start called once, prompt called for the second step
        let calls = agent_runner.calls();
        assert_eq!(calls[0], "start:first prompt");
        assert_eq!(calls[1], "prompt:second prompt");
        assert!(
            calls.iter().filter(|c| c.starts_with("start:")).count() == 1,
            "start should only be called once for a named session"
        );
    }

    #[tokio::test]
    async fn checkpoint_feedback_loop_reprompts_agent() {
        // GIVEN an agent step followed by a checkpoint that receives feedback then continue
        let storage = Arc::new(InMemoryStorage::new());
        let agent_runner = Arc::new(MockAgentRunner::new());
        let scratch = tempfile::tempdir().unwrap();
        let engine = Engine::new(
            storage.clone(),
            scratch.path().to_path_buf(),
            agent_runner.clone(),
        );
        let wf = workflow(
            "test-feedback",
            WorkflowKind::Indefinite,
            vec![
                StepDef {
                    step_type: StepType::Agent,
                    command: Some(vec!["claude".to_string()]),
                    message: Some("write code".to_string()),
                    session: Some("coder".to_string()),
                    ..step_def(StepType::Agent)
                },
                StepDef {
                    step_type: StepType::Checkpoint,
                    message: Some("Review the code".to_string()),
                    ..step_def(StepType::Checkpoint)
                },
            ],
        );

        let (ui_tx, mut ui_rx) = mpsc::channel::<EngineEvent>(32);

        // Respond: feedback first, then stop to terminate the run
        let feedback_sent = Arc::new(AtomicBool::new(false));
        let feedback_sent_clone = feedback_sent.clone();
        tokio::spawn(async move {
            while let Some(event) = ui_rx.recv().await {
                if let EngineEvent::CheckpointPending { response_tx, .. } = event {
                    if !feedback_sent_clone.load(Ordering::Relaxed) {
                        feedback_sent_clone.store(true, Ordering::Relaxed);
                        let _ = response_tx.send(CheckpointResponse::Feedback(
                            "please fix the typo".to_string(),
                        ));
                    } else {
                        let _ = response_tx.send(CheckpointResponse::Stop);
                        break;
                    }
                }
            }
        });

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        engine.run(&wf, shutdown, Some(ui_tx)).await.unwrap();

        // THEN — agent started, then re-prompted with feedback text
        let calls = agent_runner.calls();
        assert_eq!(calls[0], "start:write code");
        assert_eq!(calls[1], "prompt:please fix the typo");

        // Agent's feedback response was logged via extra_logs at the checkpoint step
        let logs = storage.logs();
        let agent_at_checkpoint: Vec<_> = logs
            .iter()
            .filter(|l| l.step_type == "agent" && l.step_index == 1)
            .collect();
        assert_eq!(
            agent_at_checkpoint.len(),
            1,
            "checkpoint should log the agent feedback response"
        );
    }

    #[tokio::test]
    async fn checkpoint_without_session_does_not_offer_feedback() {
        // GIVEN a checkpoint with no prior agent step — feedback_available will be false
        let storage = Arc::new(InMemoryStorage::new());
        let agent_runner = Arc::new(MockAgentRunner::new());
        let scratch = tempfile::tempdir().unwrap();

        // Use a channel so we can verify what feedback_available was sent
        let (ui_tx, mut ui_rx) = mpsc::channel::<EngineEvent>(32);
        let feedback_available_seen = Arc::new(Mutex::new(None::<bool>));
        let seen_clone = feedback_available_seen.clone();
        tokio::spawn(async move {
            while let Some(event) = ui_rx.recv().await {
                if let EngineEvent::CheckpointPending { feedback_available, response_tx, .. } = event {
                    *seen_clone.lock().unwrap() = Some(feedback_available);
                    let _ = response_tx.send(CheckpointResponse::Stop);
                    break;
                }
            }
        });

        let engine = Engine::new(
            storage.clone(),
            scratch.path().to_path_buf(),
            agent_runner.clone(),
        );
        let wf = workflow(
            "test-no-session",
            WorkflowKind::Indefinite,
            vec![StepDef {
                step_type: StepType::Checkpoint,
                message: Some("Review".to_string()),
                ..step_def(StepType::Checkpoint)
            }],
        );

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        engine.run(&wf, shutdown, Some(ui_tx)).await.unwrap();

        // THEN — no agent calls, checkpoint reported feedback_available = false
        assert!(agent_runner.calls().is_empty());
        assert_eq!(*feedback_available_seen.lock().unwrap(), Some(false));
    }

    #[tokio::test]
    async fn anonymous_sessions_are_single_use() {
        // GIVEN two agent steps without session names
        let storage = Arc::new(InMemoryStorage::new());
        let agent_runner = Arc::new(MockAgentRunner::new());
        let scratch = tempfile::tempdir().unwrap();
        let engine = Engine::with_executors(
            storage.clone(),
            scratch.path().to_path_buf(),
            agent_runner.clone(),
            vec![Box::new(crate::steps::agent::AgentExecutor)],
        );
        let wf = workflow(
            "test-anon",
            WorkflowKind::Indefinite,
            vec![
                StepDef {
                    step_type: StepType::Agent,
                    command: Some(vec!["agent".to_string()]),
                    message: Some("task one".to_string()),
                    session: None,
                    ..step_def(StepType::Agent)
                },
                StepDef {
                    step_type: StepType::Agent,
                    command: Some(vec!["agent".to_string()]),
                    message: Some("task two".to_string()),
                    session: None,
                    ..step_def(StepType::Agent)
                },
            ],
        );

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            engine.run(&wf, shutdown_clone, None).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        shutdown.store(true, Ordering::Relaxed);
        handle.await.unwrap();

        // THEN — every anonymous step calls start (never prompt)
        let calls = agent_runner.calls();
        let starts: Vec<_> = calls.iter().filter(|c| c.starts_with("start:")).collect();
        let prompts = calls.iter().filter(|c| c.starts_with("prompt:")).count();
        assert!(starts.len() >= 2, "at least two start calls expected");
        assert_eq!(prompts, 0, "anonymous sessions should never be resumed");
    }

    #[tokio::test]
    async fn sessions_cleaned_up_at_run_end() {
        // GIVEN a named session agent step
        let storage = Arc::new(InMemoryStorage::new());
        let agent_runner = Arc::new(MockAgentRunner::new());
        let scratch = tempfile::tempdir().unwrap();
        let engine = Engine::with_executors(
            storage.clone(),
            scratch.path().to_path_buf(),
            agent_runner.clone(),
            vec![Box::new(crate::steps::agent::AgentExecutor)],
        );
        let wf = workflow(
            "test-cleanup",
            WorkflowKind::Indefinite,
            vec![StepDef {
                step_type: StepType::Agent,
                command: Some(vec!["agent".to_string()]),
                message: Some("do work".to_string()),
                session: Some("worker".to_string()),
                ..step_def(StepType::Agent)
            }],
        );

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            engine.run(&wf, shutdown_clone, None).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        shutdown.store(true, Ordering::Relaxed);
        handle.await.unwrap();

        // THEN — stop was called for session cleanup
        let calls = agent_runner.calls();
        assert!(
            calls.iter().any(|c| c == "stop"),
            "session should be cleaned up at run end"
        );
    }

    #[tokio::test]
    async fn triggered_workflow_runs_once_per_event() {
        // GIVEN a triggered workflow with a ManualTrigger
        use crate::types::{TriggerDef, TriggerType};

        let storage = Arc::new(InMemoryStorage::new());
        let scratch = tempfile::tempdir().unwrap();

        let engine = Engine::new(
            storage.clone(),
            scratch.path().to_path_buf(),
            Arc::new(NoOpAgentRunner),
        );

        let wf = WorkflowDef {
            name: "my-workflow".to_string(),
            kind: WorkflowKind::Triggered,
            trigger: Some(TriggerDef {
                trigger_type: TriggerType::Manual,
            }),
            steps: vec![StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["echo".to_string(), "triggered".to_string()]),
                ..step_def(StepType::Shell)
            }],
        };

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let storage_clone = storage.clone();

        let handle = tokio::spawn(async move {
            engine.run_once(&wf, shutdown_clone, None).await.unwrap();
            storage_clone
        });

        let storage = handle.await.unwrap();

        // THEN — one completed run was recorded
        let runs = storage.runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, RunStatus::Completed);

        let logs = storage.logs();
        assert!(!logs.is_empty());
        assert!(logs[0].stdout.contains("triggered"));

        // Cleanup
        shutdown.store(true, Ordering::Relaxed);
    }
}
