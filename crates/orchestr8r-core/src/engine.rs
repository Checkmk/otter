use chrono::Utc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};

use crate::steps::StepExecutor;
use crate::types::StepError;
use crate::types::{
    LogEntry, RunStatus, StepContext, StepType, StorageBackend, WorkflowDef, WorkflowRun,
};

pub struct Engine {
    executors: Vec<Box<dyn StepExecutor>>,
    storage: Arc<dyn StorageBackend>,
    scratch_base: std::path::PathBuf,
}

impl Engine {
    pub fn new(storage: Arc<dyn StorageBackend>, scratch_base: std::path::PathBuf) -> Self {
        Self {
            executors: crate::steps::registry(),
            storage,
            scratch_base,
        }
    }

    fn find_executor(&self, step_type: StepType) -> Option<&dyn StepExecutor> {
        self.executors
            .iter()
            .find(|e| e.step_type() == step_type)
            .map(|e| e.as_ref())
    }

    pub async fn run(
        &self,
        workflow: &WorkflowDef,
        shutdown: Arc<AtomicBool>,
    ) -> anyhow::Result<()> {
        let mut run = WorkflowRun::new(workflow.name.clone());
        let scratch_dir = self.scratch_base.join(run.id.to_string());
        std::fs::create_dir_all(&scratch_dir)?;

        self.storage.save_workflow_run(&run)?;
        info!(run_id = %run.id, workflow = %workflow.name, "Starting workflow run");

        let mut workspace_dir: Option<std::path::PathBuf> = None;

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
                run.status = if step_def.step_type == StepType::Checkpoint {
                    RunStatus::WaitingCheckpoint
                } else {
                    RunStatus::Running
                };
                self.storage.update_workflow_run(&run)?;

                let ctx = StepContext {
                    run_id: run.id,
                    workflow_name: workflow.name.clone(),
                    iteration: run.iteration,
                    step_index: i,
                    scratch_dir: scratch_dir.clone(),
                    workspace_dir: workspace_dir.clone(),
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
                            timestamp: Utc::now(),
                        };
                        self.storage.append_log(entry)?;
                        info!(step = i, "Step completed successfully");
                    }
                    Err(StepError::Rejected) => {
                        warn!(
                            step = i,
                            iteration = run.iteration,
                            "Checkpoint rejected — stopping workflow"
                        );
                        let entry = LogEntry {
                            run_id: run.id,
                            iteration: run.iteration,
                            step_index: i,
                            step_type: step_def.step_type.to_string(),
                            stdout: String::new(),
                            stderr: String::new(),
                            exit_code: None,
                            accepted: Some(false),
                            timestamp: Utc::now(),
                        };
                        self.storage.append_log(entry)?;
                        run.status = RunStatus::Failed;
                        self.storage.update_workflow_run(&run)?;
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
                            timestamp: Utc::now(),
                        };
                        self.storage.append_log(entry)?;
                        run.status = RunStatus::Failed;
                        self.storage.update_workflow_run(&run)?;
                        break 'outer;
                    }
                }
            }

            run.iteration += 1;
            run.status = RunStatus::Running;
            self.storage.update_workflow_run(&run)?;
        }

        info!(run_id = %run.id, iterations = run.iteration, "Workflow run ended");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorage;
    use crate::types::{RunStatus, StepDef, StepType, WorkflowDef, WorkflowKind};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn make_engine(storage: Arc<InMemoryStorage>) -> Engine {
        let scratch = std::env::temp_dir().join("orchestr8r-tests");
        Engine::new(storage, scratch)
    }

    #[tokio::test]
    async fn shell_step_runs_and_logs() {
        // GIVEN a workflow with a single shell step that echoes "hello"
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
            }],
        };

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let storage_clone = storage.clone();
        let handle = tokio::spawn(async move {
            engine.run(&workflow, shutdown_clone).await.unwrap();
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
        // GIVEN a TOML workflow with an invalid step type
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
        // GIVEN a shell step whose command exits non-zero
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
            }],
        };

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        engine.run(&workflow, shutdown).await.unwrap();

        // THEN
        let runs = storage.runs();
        assert_eq!(runs.last().unwrap().status, RunStatus::Failed);
    }

    #[tokio::test]
    async fn workspace_step_sets_working_dir_for_shell() {
        // GIVEN a workspace step pointing to a temp dir, followed by a shell step
        // that creates a marker file there
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
                },
                StepDef {
                    step_type: StepType::Shell,
                    command: Some(vec!["touch".to_string(), "marker.txt".to_string()]),
                    message: None,
                    path: None,
                },
            ],
        };

        // WHEN
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let handle = tokio::spawn(async move {
            engine.run(&workflow, shutdown_clone).await.unwrap();
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
}
