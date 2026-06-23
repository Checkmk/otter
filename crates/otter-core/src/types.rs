use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::requirements::Requirements;
use crate::resource_limiter::ResourceLimiter;
use crate::session::AgentSessionManager;
use otter_secrets::SecretStore;

pub const WORKFLOW_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResourceConfig {
    pub cpu_quota: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxDef {
    pub image: Option<String>,
    #[serde(default, deserialize_with = "deserialize_network_mode")]
    pub network: Option<String>,
}

fn deserialize_network_mode<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let val: Option<String> = Option::deserialize(deserializer)?;
    if let Some(ref s) = val {
        match s.as_str() {
            "bridge" | "none" => {}
            other => {
                return Err(serde::de::Error::custom(format!(
                    "invalid network mode '{other}': expected 'bridge' or 'none'"
                )));
            }
        }
    }
    Ok(val)
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowDef {
    pub name: String,
    #[serde(rename = "type")]
    pub workflow_type: WorkflowType,
    #[serde(default)]
    pub schema: Option<u32>,
    /// Human-readable package version (e.g. "1.0.0"). Display only.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub trigger: Option<TriggerDef>,
    #[serde(default)]
    pub workspace: Option<WorkspaceConfig>,
    #[serde(default)]
    pub resources: Option<ResourceConfig>,
    #[serde(default)]
    pub sandbox: Option<SandboxDef>,
    pub steps: Vec<StepDef>,
    /// Steps that always run after main steps, regardless of outcome.
    #[serde(default)]
    pub finally: Vec<FinallyStepDef>,
    /// Declared inputs consumed by the install/configure flow.
    #[serde(default)]
    pub require: Option<Requirements>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowType {
    Looping,
    Triggered,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum TriggerDef {
    Manual,
    Polling {
        poll_command: Vec<String>,
        #[serde(default)]
        context_command: Option<Vec<String>>,
        #[serde(default = "default_poll_interval")]
        interval_secs: u64,
        #[serde(default)]
        requires: Option<Vec<String>>,
    },
    /// Fires only when another workflow hands it a run via `otter dispatch`.
    /// The dispatched event carries a payload and a pre-built `trigger-context/`.
    Dispatch,
}

fn default_poll_interval() -> u64 {
    600
}

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "WorkspaceConfigRaw")]
pub struct WorkspaceConfig {
    pub source: WorkspaceSource,
    pub pool: Option<PoolConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct WorkspaceConfigRaw {
    #[serde(flatten)]
    source: WorkspaceSource,
    #[serde(default)]
    pool: Option<PoolConfig>,
}

impl TryFrom<WorkspaceConfigRaw> for WorkspaceConfig {
    type Error = String;
    fn try_from(raw: WorkspaceConfigRaw) -> Result<Self, Self::Error> {
        if raw.pool.is_some() && !matches!(raw.source, WorkspaceSource::Git { .. }) {
            return Err("[workspace.pool] is only supported when type = \"git\"".to_string());
        }
        Ok(WorkspaceConfig {
            source: raw.source,
            pool: raw.pool,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceSource {
    /// Use the run's scratch directory (default when omitted).
    Scratch,
    /// An existing directory on disk.
    Fixed { path: String },
    /// A command whose stdout (trimmed) is the workspace path.
    /// Invoked as: `command[0] command[1..] <workflow-name> <run-id>`.
    /// Always runs with a clean environment; declare `requires` to inject from the store.
    Script {
        command: Vec<String>,
        #[serde(default)]
        requires: Option<Vec<String>>,
    },
    /// A git worktree against a local base repo, checked out at `ref`.
    /// Combine with `[workspace.pool]` to share reusable, locked slots across runs.
    Git {
        base_repo: String,
        #[serde(default, rename = "ref")]
        ref_: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolConfig {
    pub dir: String,
    #[serde(default)]
    pub keep_directory_on: Vec<RunOutcome>,
}

impl From<WorkspaceSource> for WorkspaceConfig {
    fn from(source: WorkspaceSource) -> Self {
        Self { source, pool: None }
    }
}

#[derive(Debug, Clone)]
pub struct PendingContext {
    pub command: Vec<String>,
    pub hash: String,
    /// Secret names to resolve and inject when running this context command.
    /// Always isolated (no daemon env); empty = safe system vars only.
    pub secrets: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TriggerEvent {
    pub source: String,
    pub payload: String,
    pub preallocated_run_id: Option<Uuid>,
    /// Context command to run at the start of the workflow run, after the workspace is set up.
    /// When set, `run_once()` invokes `command <hash> <ctx_dir>` before executing steps.
    pub pending_context: Option<PendingContext>,
    /// Files to materialize directly into `trigger-context/` before steps run, as
    /// `(filename, contents)` pairs. This is the inline counterpart to
    /// `pending_context`'s command form, used by the `dispatch` trigger to hand a
    /// pre-built context to a started run.
    pub inline_context: Option<Vec<(String, String)>>,
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
    #[serde(default)]
    pub message_file: Option<String>,
    pub session: Option<String>,
    pub notify: Option<Vec<String>>,
    #[serde(default)]
    pub requires: Option<Vec<String>>,
    /// Per-step sandbox override: `true` forces sandbox on, `false` opts out,
    /// absent inherits from workflow-level `[sandbox]`.
    #[serde(default)]
    pub sandbox: Option<bool>,
    #[serde(flatten)]
    pub agent: AgentConfig,
}

/// A step in the `[[finally]]` section — runs after main steps regardless of outcome.
#[derive(Debug, Clone, Deserialize)]
pub struct FinallyStepDef {
    #[serde(flatten)]
    pub step: StepDef,
    /// Outcomes that trigger this step. `None` means all outcomes.
    #[serde(default)]
    pub on: Option<Vec<RunOutcome>>,
}

impl FinallyStepDef {
    pub fn applies_to(&self, outcome: &RunOutcome) -> bool {
        match &self.on {
            None => true,
            Some(filters) => filters.contains(outcome),
        }
    }
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
    #[serde(default)]
    pub orphaned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_dir: Option<std::path::PathBuf>,
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
            orphaned: false,
            workspace_dir: None,
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
    /// User explicitly stopped the workflow at a checkpoint.
    Stopped,
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunStatus::Running => write!(f, "running"),
            RunStatus::WaitingCheckpoint => write!(f, "waiting_checkpoint"),
            RunStatus::Completed => write!(f, "completed"),
            RunStatus::Failed => write!(f, "failed"),
            RunStatus::Stopped => write!(f, "stopped"),
        }
    }
}

/// Terminal outcome of a workflow run, used to filter `[[finally]]` steps.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Success,
    Failed,
    Stopped,
}

impl RunStatus {
    /// Convert a terminal `RunStatus` to a `RunOutcome` for finally-step filtering.
    /// Returns `None` for non-terminal statuses.
    pub fn as_outcome(&self) -> Option<RunOutcome> {
        match self {
            RunStatus::Completed => Some(RunOutcome::Success),
            RunStatus::Failed => Some(RunOutcome::Failed),
            RunStatus::Stopped => Some(RunOutcome::Stopped),
            _ => None,
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
    pub scripts_dir: Option<std::path::PathBuf>,
    pub checkpoint_tx: Option<mpsc::Sender<EngineEvent>>,
    pub session_manager: Option<Arc<AgentSessionManager>>,
    pub notifier: Arc<dyn otter_notify::Notifier>,
    /// Called by step executors to persist and broadcast a log entry immediately,
    /// rather than waiting until the step's execute() returns.
    pub log_fn: Option<Arc<dyn Fn(LogEntry) + Send + Sync>>,
    /// Called by step executors to emit ephemeral progress chunks for live TUI display.
    pub progress_fn: Option<Arc<dyn Fn(ProgressChunk) + Send + Sync>>,
    pub resource_limiter: Arc<dyn ResourceLimiter>,
    pub secret_store: Arc<dyn SecretStore>,
    pub requirements: Option<Arc<Requirements>>,
    /// Resolved sandbox configuration for this step, if sandboxing is active.
    pub sandbox_config: Option<agentbox::SandboxConfig>,
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
    WorkflowStateChanged {
        name: String,
        state: WorkflowState,
    },
    CheckpointPending {
        run_id: Uuid,
        step_index: usize,
        message: String,
        feedback_available: bool,
        response_tx: oneshot::Sender<CheckpointResponse>,
    },
    /// Ephemeral progress from a running step — not persisted, not replayed on reconnect.
    StepProgress {
        run_id: Uuid,
        step_index: usize,
        chunk: ProgressChunk,
    },
}

/// Ephemeral progress chunk emitted during step execution for live TUI display.
/// Not persisted to storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProgressChunk {
    /// Raw stdout line (for non-Claude providers or shell streaming).
    Stdout(String),
    /// Raw stderr line.
    Stderr(String),
    /// Parsed high-level status (e.g. "Thinking...", "Using tool: Read").
    Status(String),
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStatus {
    pub name: String,
    pub kind: WorkflowType,
    pub state: WorkflowState,
    pub trigger: Option<TriggerDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toml_content: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_available: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MarketplaceOrigin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MarketplaceOrigin {
    pub marketplace: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dangling: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceStatus {
    pub name: String,
    pub url: String,
    pub workflow_count: usize,
    pub last_fetched_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub workflows: Vec<MarketplaceWorkflowEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceWorkflowEntry {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Path of the package inside the marketplace clone, relative to the clone
    /// root. Lets TUI clients locate the on-disk package for previews.
    #[serde(default)]
    pub path: String,
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
    WorkflowsSnapshot(Vec<WorkflowStatus>),
    MarketplacesSnapshot(Vec<MarketplaceStatus>),
    CheckpointPending {
        run_id: Uuid,
        step_index: usize,
        message: String,
        feedback_available: bool,
    },
    RunDeleted {
        run_id: Uuid,
    },
    ConsumedTriggersChanged {
        workflow: String,
        triggers: Vec<String>,
    },
    /// Ephemeral progress from a running step — not persisted, not replayed on reconnect.
    StepProgress {
        run_id: Uuid,
        step_index: usize,
        chunk: ProgressChunk,
    },
    UpdateAvailable {
        current: String,
        latest: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonCommand {
    Ping,
    Start {
        name: String,
    },
    /// Hand a one-off run to a `dispatch`-triggered workflow, carrying a payload
    /// and a set of `(filename, contents)` files to pre-populate `trigger-context/`.
    Dispatch {
        workflow: String,
        #[serde(default)]
        payload: Option<String>,
        #[serde(default)]
        context_files: Vec<(String, String)>,
    },
    Stop {
        name: String,
    },
    Status,
    /// Persistent subscription: server streams DaemonEvent JSON lines until disconnect.
    Subscribe,
    CheckpointRespond {
        run_id: Uuid,
        action: CheckpointAction,
    },
    StopRun {
        run_id: Uuid,
    },
    DeleteRun {
        run_id: Uuid,
    },
    ListConsumedTriggers {
        workflow: String,
    },
    DeleteConsumedTrigger {
        workflow: String,
        trigger: String,
    },
    ReloadWorkflows,
    EnableWorkflow {
        name: String,
    },
    DisableWorkflow {
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonResponse {
    Ok,
    /// Reply to [`DaemonCommand::Ping`]; proves a live daemon is listening.
    Pong,
    Error {
        message: String,
    },
    StatusResponse {
        workflows: Vec<WorkflowStatus>,
        #[serde(default)]
        marketplaces: Vec<MarketplaceStatus>,
    },
    ConsumedTriggersResponse {
        triggers: Vec<String>,
    },
}

pub trait StorageBackend: Send + Sync {
    fn save_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()>;
    fn update_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()>;
    fn append_log(&self, entry: LogEntry) -> anyhow::Result<()>;
    fn load_latest_run(&self, workflow_name: &str) -> anyhow::Result<Option<WorkflowRun>>;
    fn load_workflow_runs(&self, workflow_name: &str) -> anyhow::Result<Vec<WorkflowRun>>;
    fn load_all_runs(&self) -> anyhow::Result<Vec<WorkflowRun>>;
    fn load_run_logs(&self, run_id: Uuid) -> anyhow::Result<Vec<LogEntry>>;
    fn delete_run(&self, run_id: Uuid) -> anyhow::Result<()>;
    fn register_workflow(&self, workflow_name: &str) -> anyhow::Result<()>;
    fn deregister_workflow(&self, workflow_name: &str) -> anyhow::Result<()>;
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
        assert_eq!(RunStatus::Stopped.to_string(), "stopped");
    }

    #[test]
    fn run_status_as_outcome_mapping() {
        // GIVEN / WHEN / THEN
        assert_eq!(RunStatus::Completed.as_outcome(), Some(RunOutcome::Success));
        assert_eq!(RunStatus::Failed.as_outcome(), Some(RunOutcome::Failed));
        assert_eq!(RunStatus::Stopped.as_outcome(), Some(RunOutcome::Stopped));
        assert_eq!(RunStatus::Running.as_outcome(), None);
        assert_eq!(RunStatus::WaitingCheckpoint.as_outcome(), None);
    }

    #[test]
    fn finally_step_applies_to_all_when_on_is_none() {
        // GIVEN
        let step = FinallyStepDef {
            step: StepDef {
                step_type: StepType::Shell,
                command: None,
                message: None,
                message_file: None,
                session: None,
                notify: None,
                requires: None,
                sandbox: None,
                agent: Default::default(),
            },
            on: None,
        };
        // WHEN / THEN
        assert!(step.applies_to(&RunOutcome::Success));
        assert!(step.applies_to(&RunOutcome::Failed));
        assert!(step.applies_to(&RunOutcome::Stopped));
    }

    #[test]
    fn finally_step_applies_to_matching_outcome() {
        // GIVEN
        let step = FinallyStepDef {
            step: StepDef {
                step_type: StepType::Shell,
                command: None,
                message: None,
                message_file: None,
                session: None,
                notify: None,
                requires: None,
                sandbox: None,
                agent: Default::default(),
            },
            on: Some(vec![RunOutcome::Failed]),
        };
        // WHEN / THEN
        assert!(step.applies_to(&RunOutcome::Failed));
        assert!(!step.applies_to(&RunOutcome::Success));
        assert!(!step.applies_to(&RunOutcome::Stopped));
    }

    #[test]
    fn workflow_def_with_finally_deserializes() {
        // GIVEN
        let toml_str = r#"
            name = "test"
            type = "looping"

            [[steps]]
            type = "shell"
            command = ["echo", "main"]

            [[finally]]
            type = "shell"
            command = ["cleanup.sh"]

            [[finally]]
            type = "notify"
            message = "done"
            on = ["success"]
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        assert_eq!(def.steps.len(), 1);
        assert_eq!(def.finally.len(), 2);
        assert_eq!(def.finally[0].step.step_type, StepType::Shell);
        assert!(def.finally[0].on.is_none());
        assert_eq!(def.finally[1].step.step_type, StepType::Notify);
        assert_eq!(def.finally[1].on, Some(vec![RunOutcome::Success]));
    }

    #[test]
    fn workflow_def_without_finally_deserializes() {
        // GIVEN
        let toml_str = r#"
            name = "test"
            type = "looping"
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        assert!(def.finally.is_empty());
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
            poll_command = ["jira-poller.sh"]

            [[steps]]
            type = "shell"
            command = ["echo", "review"]
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        let trigger = def.trigger.unwrap();
        match trigger {
            TriggerDef::Polling {
                poll_command,
                context_command,
                interval_secs,
                ..
            } => {
                assert_eq!(poll_command, vec!["jira-poller.sh".to_string()]);
                assert!(context_command.is_none());
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
            poll_command = ["jira-poller.py"]
            context_command = ["jira-poller.py", "--context"]
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
            TriggerDef::Polling {
                poll_command,
                context_command,
                interval_secs,
                ..
            } => {
                assert_eq!(poll_command, vec!["jira-poller.py".to_string()]);
                assert_eq!(
                    context_command,
                    Some(vec!["jira-poller.py".to_string(), "--context".to_string()])
                );
                assert_eq!(interval_secs, 300);
            }
            _ => panic!("expected polling trigger"),
        }
    }

    #[test]
    fn sandbox_config_deserializes_from_toml() {
        // GIVEN
        let toml_str = r#"
            name = "sandboxed"
            type = "looping"

            [sandbox]
            image = "my-image:latest"
            network = "none"

            [[steps]]
            type = "shell"
            command = ["echo", "hi"]

            [[steps]]
            type = "shell"
            command = ["git", "push"]
            sandbox = false
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        let sandbox = def.sandbox.unwrap();
        assert_eq!(sandbox.image.as_deref(), Some("my-image:latest"));
        assert_eq!(sandbox.network.as_deref(), Some("none"));
        assert!(def.steps[0].sandbox.is_none());
        assert_eq!(def.steps[1].sandbox, Some(false));
    }

    #[test]
    fn sandbox_invalid_network_mode_fails_deserialization() {
        // GIVEN
        let toml_str = r#"
            name = "bad-network"
            type = "looping"

            [sandbox]
            network = "noen"

            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;

        // WHEN
        let err = toml::from_str::<WorkflowDef>(toml_str).unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(
            msg.contains("invalid network mode"),
            "error should mention invalid network mode: {msg}"
        );
    }

    #[test]
    fn sandbox_absent_deserializes_as_none() {
        // GIVEN
        let toml_str = r#"
            name = "no-sandbox"
            type = "looping"
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        assert!(def.sandbox.is_none());
        assert!(def.steps[0].sandbox.is_none());
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

    #[test]
    fn git_workspace_with_pool_deserializes() {
        // GIVEN
        let toml_str = r#"
            name = "wf"
            type = "triggered"
            [trigger]
            type = "manual"
            [workspace]
            type = "git"
            base_repo = "/tmp/repo"
            ref = "origin/master"
            [workspace.pool]
            dir = "/tmp/pool"
            keep_directory_on = ["failed"]
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;

        // WHEN
        let def: WorkflowDef = toml::from_str(toml_str).unwrap();

        // THEN
        let ws = def.workspace.unwrap();
        match &ws.source {
            WorkspaceSource::Git { base_repo, ref_ } => {
                assert_eq!(base_repo, "/tmp/repo");
                assert_eq!(ref_.as_deref(), Some("origin/master"));
            }
            _ => panic!("expected git source"),
        }
        let pool = ws.pool.unwrap();
        assert_eq!(pool.dir, "/tmp/pool");
        assert_eq!(pool.keep_directory_on, vec![RunOutcome::Failed]);
    }

    #[test]
    fn pool_with_non_git_source_fails_deserialization() {
        // GIVEN — scratch + pool is rejected
        let toml_str = r#"
            name = "wf"
            type = "looping"
            [workspace]
            type = "scratch"
            [workspace.pool]
            dir = "/tmp/pool"
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;

        // WHEN / THEN
        let err = toml::from_str::<WorkflowDef>(toml_str).unwrap_err();
        assert!(
            err.to_string()
                .contains("[workspace.pool] is only supported"),
            "expected validation error, got: {err}"
        );
    }

    #[test]
    fn pool_with_fixed_source_fails_deserialization() {
        // GIVEN
        let toml_str = r#"
            name = "wf"
            type = "looping"
            [workspace]
            type = "fixed"
            path = "/tmp"
            [workspace.pool]
            dir = "/tmp/pool"
            [[steps]]
            type = "shell"
            command = ["echo", "hi"]
        "#;

        // WHEN / THEN
        assert!(toml::from_str::<WorkflowDef>(toml_str).is_err());
    }
}
