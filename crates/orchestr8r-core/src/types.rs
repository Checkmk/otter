use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    pub kind: WorkflowKind,
    pub steps: Vec<StepDef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowKind {
    Indefinite,
}

#[derive(Debug, Deserialize)]
pub struct StepDef {
    #[serde(rename = "type")]
    pub step_type: String,
    pub command: Option<Vec<String>>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: Uuid,
    pub workflow_name: String,
    pub status: RunStatus,
    pub current_step: usize,
    pub iteration: u64,
    pub started_at: DateTime<Utc>,
}

impl WorkflowRun {
    pub fn new(workflow_name: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            workflow_name,
            status: RunStatus::Running,
            current_step: 0,
            iteration: 0,
            started_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    WaitingCheckpoint,
    Completed,
    Failed,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Running => write!(f, "running"),
            RunStatus::WaitingCheckpoint => write!(f, "waiting_checkpoint"),
            RunStatus::Completed => write!(f, "completed"),
            RunStatus::Failed => write!(f, "failed"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StepContext {
    pub run_id: Uuid,
    pub workflow_name: String,
    pub iteration: u64,
    pub step_index: usize,
    pub scratch_dir: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub struct StepOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub accepted: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum StepError {
    #[error("step execution failed: {0}")]
    ExecutionFailed(String),
    #[error("rejected at checkpoint")]
    Rejected,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub run_id: Uuid,
    pub iteration: u64,
    pub step_index: usize,
    pub step_type: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub accepted: Option<bool>,
    pub timestamp: DateTime<Utc>,
}

pub trait StorageBackend: Send + Sync {
    fn save_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()>;
    fn update_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()>;
    fn append_log(&self, entry: LogEntry) -> anyhow::Result<()>;
    fn load_latest_run(&self, workflow_name: &str) -> anyhow::Result<Option<WorkflowRun>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_run_new_sets_defaults() {
        // WHEN
        let run = WorkflowRun::new("my-workflow".to_string());

        // THEN
        assert_eq!(run.workflow_name, "my-workflow");
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.current_step, 0);
        assert_eq!(run.iteration, 0);
    }

    #[test]
    fn workflow_run_ids_are_unique() {
        // WHEN
        let a = WorkflowRun::new("wf".to_string());
        let b = WorkflowRun::new("wf".to_string());

        // THEN
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn run_status_display() {
        // WHEN / THEN
        assert_eq!(RunStatus::Running.to_string(), "running");
        assert_eq!(RunStatus::WaitingCheckpoint.to_string(), "waiting_checkpoint");
        assert_eq!(RunStatus::Completed.to_string(), "completed");
        assert_eq!(RunStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn workflow_def_deserializes_from_toml() {
        // GIVEN
        let toml_str = r#"
            name = "test"
            kind = "indefinite"

            [[steps]]
            type = "shell"
            command = ["echo", "hi"]

            [[steps]]
            type = "checkpoint"
            message = "ok?"
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        assert_eq!(def.name, "test");
        assert_eq!(def.steps.len(), 2);
        assert_eq!(def.steps[0].step_type, "shell");
        let cmd = def.steps[0].command.as_ref().unwrap();
        assert_eq!(cmd, &["echo", "hi"]);
        assert_eq!(def.steps[1].step_type, "checkpoint");
        assert_eq!(def.steps[1].message.as_deref(), Some("ok?"));
    }
}
