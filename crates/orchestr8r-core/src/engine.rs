use chrono::Utc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::agent_runner::{AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec};
use crate::steps::StepExecutor;
use crate::types::StepError;
use crate::types::{
    LogEntry, RunStatus, StepContext, StepType, StorageBackend, EngineEvent, WorkflowDef, WorkflowRun,
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
        let mut run = WorkflowRun::new(workflow.name.clone());
        let scratch_dir = self.scratch_base.join(run.id.to_string());
        std::fs::create_dir_all(&scratch_dir)?;

        self.storage.save_workflow_run(&run)?;
        Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
        info!(run_id = %run.id, workflow = %workflow.name, "Starting workflow run");

        let mut workspace_dir: Option<std::path::PathBuf> = None;
        let mut sessions: HashMap<String, AgentSessionHandle> = HashMap::new();
        let mut last_session_key: Option<String> = None;

        'outer: loop {
            if shutdown.load(Ordering::Relaxed) {
                info!("Shutdown requested, stopping after current iteration");
                break;
            }

            info!(iteration = run.iteration, "Starting iteration");

            for (i, step_def) in workflow.steps.iter().enumerate() {
                if shutdown.load(Ordering::Relaxed) {
                    info!("Shutdown requested, stopping after current step");
                    break 'outer;
                }

                run.current_step = i;

                // Agent steps are handled directly by the engine
                if step_def.step_type == StepType::Agent {
                    run.status = RunStatus::Running;
                    self.storage.update_workflow_run(&run)?;
                    Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));

                    info!(step = i, step_type = "agent", "Executing agent step");

                    let ctx = StepContext {
                        run_id: run.id,
                        workflow_name: workflow.name.clone(),
                        iteration: run.iteration,
                        step_index: i,
                        scratch_dir: scratch_dir.clone(),
                        workspace_dir: workspace_dir.clone(),
                        feedback_available: false,
                        checkpoint_tx: None,
                    };

                    match self
                        .execute_agent_step(step_def, &ctx, &mut sessions, &mut last_session_key)
                        .await
                    {
                        Ok(output) => {
                            let entry = LogEntry {
                                run_id: run.id,
                                iteration: run.iteration,
                                step_index: i,
                                step_type: "agent".to_string(),
                                stdout: output.stdout.clone(),
                                stderr: output.stderr.clone(),
                                exit_code: output.exit_code,
                                accepted: None,
                                feedback: None,
                                timestamp: Utc::now(),
                            };
                            self.storage.append_log(entry.clone())?;
                            Self::emit(&ui_tx, EngineEvent::LogAppended(entry));

                            let output_path =
                                scratch_dir.join(format!("step-{}-output.md", ctx.step_index));
                            let _ = tokio::fs::write(&output_path, &output.stdout).await;

                            info!(step = i, "Agent step completed successfully");
                        }
                        Err(e) => {
                            error!(step = i, error = %e, "Agent step failed");
                            let entry = LogEntry {
                                run_id: run.id,
                                iteration: run.iteration,
                                step_index: i,
                                step_type: "agent".to_string(),
                                stdout: String::new(),
                                stderr: e.to_string(),
                                exit_code: Some(1),
                                accepted: None,
                                feedback: None,
                                timestamp: Utc::now(),
                            };
                            self.storage.append_log(entry.clone())?;
                            Self::emit(&ui_tx, EngineEvent::LogAppended(entry));
                            run.status = RunStatus::Failed;
                            self.storage.update_workflow_run(&run)?;
                            Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
                            break 'outer;
                        }
                    }
                    continue;
                }

                // Checkpoint steps get the feedback loop
                if step_def.step_type == StepType::Checkpoint {
                    run.status = RunStatus::WaitingCheckpoint;
                    self.storage.update_workflow_run(&run)?;
                    Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));

                    let has_session = last_session_key
                        .as_ref()
                        .map_or(false, |k| sessions.contains_key(k));

                    let ctx = StepContext {
                        run_id: run.id,
                        workflow_name: workflow.name.clone(),
                        iteration: run.iteration,
                        step_index: i,
                        scratch_dir: scratch_dir.clone(),
                        workspace_dir: workspace_dir.clone(),
                        feedback_available: has_session,
                        checkpoint_tx: ui_tx.clone(),
                    };

                    let executor = match self.find_executor(StepType::Checkpoint) {
                        Some(e) => e,
                        None => {
                            error!("No executor found for checkpoint");
                            run.status = RunStatus::Failed;
                            self.storage.update_workflow_run(&run)?;
                            break 'outer;
                        }
                    };

                    let mut broke_outer = false;
                    loop {
                        match executor.execute(step_def, &ctx).await {
                            Ok(output) => {
                                if let Some(ref feedback_text) = output.feedback {
                                    // Feedback loop: send to last agent session
                                    let entry = LogEntry {
                                        run_id: run.id,
                                        iteration: run.iteration,
                                        step_index: i,
                                        step_type: "checkpoint".to_string(),
                                        stdout: String::new(),
                                        stderr: String::new(),
                                        exit_code: None,
                                        accepted: None,
                                        feedback: Some(feedback_text.clone()),
                                        timestamp: Utc::now(),
                                    };
                                    self.storage.append_log(entry.clone())?;
                                    Self::emit(&ui_tx, EngineEvent::LogAppended(entry));

                                    if let Some(ref session_key) = last_session_key {
                                        if let Some(session) = sessions.get(session_key) {
                                            match self
                                                .agent_runner
                                                .prompt(session, feedback_text)
                                                .await
                                            {
                                                Ok(agent_output) => {
                                                    let agent_entry = LogEntry {
                                                        run_id: run.id,
                                                        iteration: run.iteration,
                                                        step_index: i,
                                                        step_type: "agent".to_string(),
                                                        stdout: agent_output.stdout,
                                                        stderr: agent_output.stderr,
                                                        exit_code: agent_output.exit_code,
                                                        accepted: None,
                                                        feedback: None,
                                                        timestamp: Utc::now(),
                                                    };
                                                    self.storage.append_log(agent_entry.clone())?;
                                                    Self::emit(&ui_tx, EngineEvent::LogAppended(agent_entry));
                                                }
                                                Err(e) => {
                                                    warn!(error = %e, "Agent feedback prompt failed");
                                                }
                                            }
                                        } else {
                                            warn!("No active session for feedback");
                                        }
                                    } else {
                                        warn!("No agent session available for feedback");
                                    }
                                    continue; // re-present checkpoint
                                }

                                // Accepted
                                let entry = LogEntry {
                                    run_id: run.id,
                                    iteration: run.iteration,
                                    step_index: i,
                                    step_type: "checkpoint".to_string(),
                                    stdout: output.stdout.clone(),
                                    stderr: output.stderr.clone(),
                                    exit_code: output.exit_code,
                                    accepted: output.accepted,
                                    feedback: None,
                                    timestamp: Utc::now(),
                                };
                                self.storage.append_log(entry.clone())?;
                                Self::emit(&ui_tx, EngineEvent::LogAppended(entry));
                                info!(step = i, "Checkpoint accepted");
                                break;
                            }
                            Err(StepError::Rejected) => {
                                warn!(step = i, "Checkpoint rejected — stopping workflow");
                                let entry = LogEntry {
                                    run_id: run.id,
                                    iteration: run.iteration,
                                    step_index: i,
                                    step_type: "checkpoint".to_string(),
                                    stdout: String::new(),
                                    stderr: String::new(),
                                    exit_code: None,
                                    accepted: Some(false),
                                    feedback: None,
                                    timestamp: Utc::now(),
                                };
                                self.storage.append_log(entry.clone())?;
                                Self::emit(&ui_tx, EngineEvent::LogAppended(entry));
                                run.status = RunStatus::Failed;
                                self.storage.update_workflow_run(&run)?;
                                Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
                                broke_outer = true;
                                break;
                            }
                            Err(e) => {
                                error!(step = i, error = %e, "Checkpoint failed");
                                let entry = LogEntry {
                                    run_id: run.id,
                                    iteration: run.iteration,
                                    step_index: i,
                                    step_type: "checkpoint".to_string(),
                                    stdout: String::new(),
                                    stderr: e.to_string(),
                                    exit_code: Some(1),
                                    accepted: None,
                                    feedback: None,
                                    timestamp: Utc::now(),
                                };
                                self.storage.append_log(entry.clone())?;
                                Self::emit(&ui_tx, EngineEvent::LogAppended(entry));
                                run.status = RunStatus::Failed;
                                self.storage.update_workflow_run(&run)?;
                                Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
                                broke_outer = true;
                                break;
                            }
                        }
                    }
                    if broke_outer {
                        break 'outer;
                    }
                    continue;
                }

                // All other step types: delegate to executor
                run.status = RunStatus::Running;
                self.storage.update_workflow_run(&run)?;
                Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));

                let ctx = StepContext {
                    run_id: run.id,
                    workflow_name: workflow.name.clone(),
                    iteration: run.iteration,
                    step_index: i,
                    scratch_dir: scratch_dir.clone(),
                    workspace_dir: workspace_dir.clone(),
                    feedback_available: false,
                    checkpoint_tx: None,
                };

                info!(step = i, step_type = %step_def.step_type, "Executing step");

                let executor = match self.find_executor(step_def.step_type) {
                    Some(e) => e,
                    None => {
                        error!(step_type = %step_def.step_type, "No executor found for step type");
                        run.status = RunStatus::Failed;
                        self.storage.update_workflow_run(&run)?;
                        break 'outer;
                    }
                };

                match executor.execute(step_def, &ctx).await {
                    Ok(output) => {
                        if step_def.step_type == StepType::Workspace {
                            let resolved = output.stdout.trim().to_string();
                            if !resolved.is_empty() {
                                workspace_dir = Some(std::path::PathBuf::from(&resolved));
                            }
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
                        Self::emit(&ui_tx, EngineEvent::LogAppended(entry));
                        info!(step = i, "Step completed successfully");
                    }
                    Err(StepError::Rejected) => {
                        warn!(step = i, iteration = run.iteration, "Checkpoint rejected — stopping workflow");
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
                        Self::emit(&ui_tx, EngineEvent::LogAppended(entry));
                        run.status = RunStatus::Failed;
                        self.storage.update_workflow_run(&run)?;
                        Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
                        break 'outer;
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
                        Self::emit(&ui_tx, EngineEvent::LogAppended(entry));
                        run.status = RunStatus::Failed;
                        self.storage.update_workflow_run(&run)?;
                        Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
                        break 'outer;
                    }
                }
            }

            run.iteration += 1;
            run.status = RunStatus::Running;
            self.storage.update_workflow_run(&run)?;
            Self::emit(&ui_tx, EngineEvent::RunUpdated(run.clone()));
        }

        // Cleanup all sessions
        for (_key, session) in &sessions {
            if let Err(e) = self.agent_runner.stop(session).await {
                warn!(error = %e, "Failed to stop agent session");
            }
        }

        info!(run_id = %run.id, iterations = run.iteration, "Workflow run ended");
        Ok(())
    }

    async fn execute_agent_step(
        &self,
        step_def: &crate::types::StepDef,
        ctx: &StepContext,
        sessions: &mut HashMap<String, AgentSessionHandle>,
        last_session_key: &mut Option<String>,
    ) -> Result<AgentOutput, AgentError> {
        let message = step_def
            .message
            .as_ref()
            .ok_or_else(|| AgentError::Failed("agent step missing message".to_string()))?;

        let session_key = step_def
            .session
            .clone()
            .unwrap_or_else(|| format!("__anon_{}_{}", ctx.iteration, ctx.step_index));

        let working_dir = ctx
            .workspace_dir
            .clone()
            .unwrap_or_else(|| ctx.scratch_dir.clone());

        let output = if sessions.contains_key(&session_key) {
            let session = sessions.get(&session_key).unwrap();
            self.agent_runner.prompt(session, message).await?
        } else {
            let command = step_def
                .command
                .as_ref()
                .ok_or_else(|| {
                    AgentError::Failed("agent step missing command (required for new session)".to_string())
                })?
                .clone();

            let spec = AgentSpec {
                command,
                message: message.clone(),
                working_dir,
            };

            let handle = self.agent_runner.start(spec).await?;
            let start_output = AgentOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            };
            sessions.insert(session_key.clone(), handle);
            start_output
        };

        *last_session_key = Some(session_key.clone());

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorage;
    use crate::types::{RunStatus, StepDef, StepOutput, StepType, WorkflowDef, WorkflowKind};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct NoOpAgentRunner;

    #[async_trait::async_trait]
    impl AgentRunner for NoOpAgentRunner {
        async fn start(&self, _spec: AgentSpec) -> Result<AgentSessionHandle, AgentError> {
            Ok(AgentSessionHandle {
                id: "noop".to_string(),
                command: vec![],
                working_dir: std::path::PathBuf::new(),
            })
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
        async fn start(&self, spec: AgentSpec) -> Result<AgentSessionHandle, AgentError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("start:{}", spec.message));
            Ok(AgentSessionHandle {
                id: "mock-session".to_string(),
                command: spec.command,
                working_dir: spec.working_dir,
            })
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

    use std::sync::Mutex;

    /// Mock checkpoint that returns a sequence of pre-configured responses.
    /// When responses are exhausted, rejects to terminate the workflow.
    struct MockCheckpointExecutor {
        responses: Mutex<Vec<Result<StepOutput, StepError>>>,
    }

    impl MockCheckpointExecutor {
        fn accepting() -> Self {
            Self {
                responses: Mutex::new(vec![Ok(StepOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    accepted: Some(true),
                    feedback: None,
                })]),
            }
        }

        fn feedback_then_accept(feedback_text: &str) -> Self {
            Self {
                responses: Mutex::new(vec![
                    Ok(StepOutput {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: Some(0),
                        accepted: None,
                        feedback: Some(feedback_text.to_string()),
                    }),
                    Ok(StepOutput {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: Some(0),
                        accepted: Some(true),
                        feedback: None,
                    }),
                ]),
            }
        }
    }

    #[async_trait::async_trait]
    impl StepExecutor for MockCheckpointExecutor {
        async fn execute(
            &self,
            _step_def: &crate::types::StepDef,
            _ctx: &StepContext,
        ) -> Result<StepOutput, StepError> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Err(StepError::Rejected)
            } else {
                responses.remove(0)
            }
        }
        fn step_type(&self) -> StepType {
            StepType::Checkpoint
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

    #[tokio::test]
    async fn shell_step_runs_and_logs() {
        // GIVEN
        let storage = Arc::new(InMemoryStorage::new());
        let engine = make_engine(storage.clone());
        let workflow = WorkflowDef {
            name: "test-shell".to_string(),
            kind: WorkflowKind::Indefinite,
            steps: vec![StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["echo".to_string(), "hello".to_string()]),
                message: None,
                path: None,
                output_file: None,
                session: None,
            }],
        };

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let storage_clone = storage.clone();
        let handle = tokio::spawn(async move {
            engine.run(&workflow, shutdown_clone, None).await.unwrap();
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
        let workflow = WorkflowDef {
            name: "test-fail".to_string(),
            kind: WorkflowKind::Indefinite,
            steps: vec![StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["false".to_string()]),
                message: None,
                path: None,
                output_file: None,
                session: None,
            }],
        };

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        engine.run(&workflow, shutdown, None).await.unwrap();

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
        let workflow = WorkflowDef {
            name: "test-workspace".to_string(),
            kind: WorkflowKind::Indefinite,
            steps: vec![
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
        };

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            engine.run(&workflow, shutdown_clone, None).await.unwrap();
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
            vec![],
        );
        let workflow = WorkflowDef {
            name: "test-sessions".to_string(),
            kind: WorkflowKind::Indefinite,
            steps: vec![
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
        };

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            engine.run(&workflow, shutdown_clone, None).await.unwrap();
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
        // GIVEN an agent step followed by a checkpoint that gives feedback then accepts
        let storage = Arc::new(InMemoryStorage::new());
        let agent_runner = Arc::new(MockAgentRunner::new());
        let scratch = tempfile::tempdir().unwrap();
        let engine = Engine::with_executors(
            storage.clone(),
            scratch.path().to_path_buf(),
            agent_runner.clone(),
            vec![Box::new(MockCheckpointExecutor::feedback_then_accept(
                "please fix the typo",
            ))],
        );
        let workflow = WorkflowDef {
            name: "test-feedback".to_string(),
            kind: WorkflowKind::Indefinite,
            steps: vec![
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
        };

        // WHEN — mock gives feedback then accepts; next iteration rejects, terminating the run
        let shutdown = Arc::new(AtomicBool::new(false));
        engine.run(&workflow, shutdown, None).await.unwrap();

        // THEN — agent was started, then re-prompted with feedback
        let calls = agent_runner.calls();
        assert_eq!(calls[0], "start:write code");
        assert_eq!(calls[1], "prompt:please fix the typo");

        // Feedback log entry was recorded
        let logs = storage.logs();
        let feedback_logs: Vec<_> = logs.iter().filter(|l| l.feedback.is_some()).collect();
        assert_eq!(feedback_logs.len(), 1);
        assert_eq!(
            feedback_logs[0].feedback.as_deref(),
            Some("please fix the typo")
        );
    }

    #[tokio::test]
    async fn checkpoint_without_session_does_not_offer_feedback() {
        // GIVEN a checkpoint with no prior agent step — feedback_available will be false
        let storage = Arc::new(InMemoryStorage::new());
        let agent_runner = Arc::new(MockAgentRunner::new());
        let scratch = tempfile::tempdir().unwrap();
        let engine = Engine::with_executors(
            storage.clone(),
            scratch.path().to_path_buf(),
            agent_runner.clone(),
            vec![Box::new(MockCheckpointExecutor::accepting())],
        );
        let workflow = WorkflowDef {
            name: "test-no-session".to_string(),
            kind: WorkflowKind::Indefinite,
            steps: vec![StepDef {
                step_type: StepType::Checkpoint,
                message: Some("Review".to_string()),
                ..step_def(StepType::Checkpoint)
            }],
        };

        // WHEN — mock accepts once then rejects, terminating the run
        let shutdown = Arc::new(AtomicBool::new(false));
        engine.run(&workflow, shutdown, None).await.unwrap();

        // THEN — no agent calls, checkpoint accepted without feedback option
        assert!(agent_runner.calls().is_empty());
        let logs = storage.logs();
        assert!(!logs.is_empty());
        assert!(logs.iter().all(|l| l.feedback.is_none()));
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
            vec![],
        );
        let workflow = WorkflowDef {
            name: "test-anon".to_string(),
            kind: WorkflowKind::Indefinite,
            steps: vec![
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
        };

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            engine.run(&workflow, shutdown_clone, None).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        shutdown.store(true, Ordering::Relaxed);
        handle.await.unwrap();

        // THEN — every anonymous step calls start (never prompt), because each
        // iteration generates unique keys (__anon_{iteration}_{step_index})
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
            vec![],
        );
        let workflow = WorkflowDef {
            name: "test-cleanup".to_string(),
            kind: WorkflowKind::Indefinite,
            steps: vec![StepDef {
                step_type: StepType::Agent,
                command: Some(vec!["agent".to_string()]),
                message: Some("do work".to_string()),
                session: Some("worker".to_string()),
                ..step_def(StepType::Agent)
            }],
        };

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            engine.run(&workflow, shutdown_clone, None).await.unwrap();
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
}
