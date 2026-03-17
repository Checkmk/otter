use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    #[serde(rename = "type")]
    pub workflow_type: WorkflowType,
    #[serde(default)]
    pub trigger: Option<TriggerDef>,
    #[serde(default)]
    pub workspace: Option<String>,
    pub steps: Vec<StepDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowType {
    Looping,
    Triggered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TriggerDef {
    Manual,
    Polling {
        command: Vec<String>,
        #[serde(default = "default_poll_interval")]
        interval_secs: u64,
    },
}

fn default_poll_interval() -> u64 {
    600
}

#[derive(Debug, Clone)]
pub struct TriggerEvent {
    pub source: String,
    pub payload: String,
    pub preallocated_run_id: Option<Uuid>,
}

#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    #[error("trigger error: {0}")]
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepType {
    Shell,
    Checkpoint,
    Agent,
    Notify,
}

impl std::fmt::Display for StepType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepType::Shell => write!(f, "shell"),
            StepType::Checkpoint => write!(f, "checkpoint"),
            StepType::Agent => write!(f, "agent"),
            StepType::Notify => write!(f, "notify"),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentConfig {
    pub provider: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub permission_mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StepDef {
    #[serde(rename = "type")]
    pub step_type: StepType,
    pub command: Option<Vec<String>>,
    pub message: Option<String>,
    pub session: Option<String>,
    pub notify: Option<Vec<String>>,
    #[serde(flatten)]
    pub agent: AgentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: Uuid,
    pub workflow_name: String,
    pub status: RunStatus,
    pub current_step: usize,
    pub iteration: u64,
    pub started_at: DateTime<Utc>,
    pub trigger_payload: Option<String>,
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
            trigger_payload: None,
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

#[derive(Clone)]
pub struct StepContext {
    pub run_id: Uuid,
    pub workflow_name: String,
    pub iteration: u64,
    pub step_index: usize,
    pub scratch_dir: std::path::PathBuf,
    pub workspace_dir: Option<std::path::PathBuf>,
    pub checkpoint_tx: Option<mpsc::Sender<EngineEvent>>,
    pub session_manager: Option<Arc<crate::session::AgentSessionManager>>,
    pub notifier: Arc<dyn orchestr8r_notify::Notifier>,
    /// Called by step executors to persist and broadcast a log entry immediately,
    /// rather than waiting until the step's execute() returns.
    pub log_fn: Option<Arc<dyn Fn(LogEntry) + Send + Sync>>,
}

#[derive(Debug)]
pub enum CheckpointResponse {
    Continue,
    Stop,
    Feedback(String),
}

pub enum EngineEvent {
    LogAppended(LogEntry),
    RunUpdated(WorkflowRun),
    WorkflowRegistered { name: String, kind: WorkflowType, trigger: Option<TriggerDef> },
    WorkflowStateChanged { name: String, state: WorkflowState },
    CheckpointPending {
        run_id: Uuid,
        step_index: usize,
        message: String,
        feedback_available: bool,
        response_tx: oneshot::Sender<CheckpointResponse>,
    },
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
    /// Checkpoint stopped by user.
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
    pub feedback: Option<String>,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorkflowState {
    Dormant,
    Running,
    Paused,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStatus {
    pub name: String,
    pub kind: WorkflowType,
    pub state: WorkflowState,
    pub trigger: Option<TriggerDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CheckpointAction {
    Continue,
    Stop,
    Feedback(String),
}

/// Serializable event broadcast to connected UI clients over the daemon socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonEvent {
    LogAppended(LogEntry),
    RunUpdated(WorkflowRun),
    WorkflowRegistered { name: String, kind: WorkflowType, trigger: Option<TriggerDef>, toml_content: Option<String> },
    WorkflowStateChanged { name: String, state: WorkflowState },
    CheckpointPending {
        run_id: Uuid,
        step_index: usize,
        message: String,
        feedback_available: bool,
    },
    RunDeleted { run_id: Uuid },
    ConsumedTriggersChanged { workflow: String, triggers: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonCommand {
    Start { name: String },
    Pause { name: String },
    Stop { name: String },
    Resume { name: String },
    Status,
    /// Persistent subscription: server streams DaemonEvent JSON lines until disconnect.
    Subscribe,
    CheckpointRespond { run_id: Uuid, action: CheckpointAction },
    DeleteRun { run_id: Uuid },
    ListConsumedTriggers { workflow: String },
    DeleteConsumedTrigger { workflow: String, trigger: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonResponse {
    Ok,
    Error { message: String },
    StatusResponse { workflows: Vec<WorkflowStatus> },
    ConsumedTriggersResponse { triggers: Vec<String> },
}

pub trait StorageBackend: Send + Sync {
    fn save_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()>;
    fn update_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()>;
    fn append_log(&self, entry: LogEntry) -> anyhow::Result<()>;
    fn load_latest_run(&self, workflow_name: &str) -> anyhow::Result<Option<WorkflowRun>>;
    fn load_workflow_runs(&self, workflow_name: &str) -> anyhow::Result<Vec<WorkflowRun>>;
    fn load_run_logs(&self, run_id: Uuid) -> anyhow::Result<Vec<LogEntry>>;
    fn delete_run(&self, run_id: Uuid) -> anyhow::Result<()>;
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
        assert_eq!(
            RunStatus::WaitingCheckpoint.to_string(),
            "waiting_checkpoint"
        );
        assert_eq!(RunStatus::Completed.to_string(), "completed");
        assert_eq!(RunStatus::Failed.to_string(), "failed");
    }

    #[test]
    fn workflow_def_deserializes_from_toml() {
        // GIVEN
        let toml_str = r#"
            name = "test"
            type = "looping"

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
        assert_eq!(def.steps[0].step_type, StepType::Shell);
        let cmd = def.steps[0].command.as_ref().unwrap();
        assert_eq!(cmd, &["echo", "hi"]);
        assert_eq!(def.steps[1].step_type, StepType::Checkpoint);
        assert_eq!(def.steps[1].message.as_deref(), Some("ok?"));
    }

    #[test]
    fn manual_trigger_deserializes_from_toml() {
        // GIVEN
        let toml_str = r#"
            name = "on-demand"
            type = "triggered"

            [trigger]
            type = "manual"

            [[steps]]
            type = "shell"
            command = ["echo", "run"]
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        let trigger = def.trigger.unwrap();
        matches!(trigger, TriggerDef::Manual);
    }

    #[test]
    fn polling_trigger_deserializes_with_defaults() {
        // GIVEN
        let toml_str = r#"
            name = "poll-jira"
            type = "triggered"

            [trigger]
            type = "polling"
            command = ["jira-poller.sh"]

            [[steps]]
            type = "shell"
            command = ["echo", "review"]
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        let trigger = def.trigger.unwrap();
        match trigger {
            TriggerDef::Polling { command, interval_secs } => {
                assert_eq!(command, vec!["jira-poller.sh".to_string()]);
                assert_eq!(interval_secs, 600);
            }
            _ => panic!("expected polling trigger"),
        }
    }

    #[test]
    fn polling_trigger_deserializes_with_custom_interval() {
        // GIVEN
        let toml_str = r#"
            name = "poll-jira"
            type = "triggered"

            [trigger]
            type = "polling"
            command = ["jira-poller.py"]
            interval_secs = 300

            [[steps]]
            type = "shell"
            command = ["echo", "issue"]
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        let trigger = def.trigger.unwrap();
        match trigger {
            TriggerDef::Polling { command, interval_secs } => {
                assert_eq!(command, vec!["jira-poller.py".to_string()]);
                assert_eq!(interval_secs, 300);
            }
            _ => panic!("expected polling trigger"),
        }
    }

    #[test]
    fn unknown_step_type_fails_deserialization() {
        // GIVEN
        let toml_str = r#"
            name = "bad"
            type = "looping"
            [[steps]]
            type = "nonexistent"
        "#;

        // WHEN / THEN
        assert!(toml::from_str::<WorkflowDef>(toml_str).is_err());
    }

}
