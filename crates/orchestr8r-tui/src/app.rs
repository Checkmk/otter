use std::collections::HashMap;

use orchestr8r_core::types::{CheckpointAction, DaemonCommand, DaemonEvent, LogEntry, WorkflowType, WorkflowRun, WorkflowState};
use uuid::Uuid;
use tokio::sync::mpsc;

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    FeedbackInput,
}

pub struct PendingCheckpoint {
    pub run_id: Uuid,
    pub message: String,
    pub feedback_available: bool,
}

pub struct App {
    pub runs: Vec<WorkflowRun>,
    pub registered: Vec<(String, WorkflowType, WorkflowState)>,
    pub selected_run: usize,
    pub logs: HashMap<Uuid, Vec<LogEntry>>,
    pub pending_checkpoints: HashMap<Uuid, PendingCheckpoint>,
    pub feedback_input: String,
    pub mode: Mode,
    pub should_quit: bool,
    pub tick: u64,
    pub cmd_tx: mpsc::Sender<DaemonCommand>,
}

impl App {
    pub fn new(cmd_tx: mpsc::Sender<DaemonCommand>) -> Self {
        Self {
            runs: Vec::new(),
            registered: Vec::new(),
            selected_run: 0,
            logs: HashMap::new(),
            pending_checkpoints: HashMap::new(),
            feedback_input: String::new(),
            mode: Mode::Normal,
            should_quit: false,
            tick: 0,
            cmd_tx,
        }
    }

    pub fn active_checkpoint(&self) -> Option<&PendingCheckpoint> {
        let run_id = self.selected_run_id()?;
        self.pending_checkpoints.get(&run_id)
    }

    pub fn take_selected_checkpoint(&mut self) -> Option<PendingCheckpoint> {
        let run_id = self.selected_run_id()?;
        self.pending_checkpoints.remove(&run_id)
    }

    pub fn other_checkpoint_count(&self) -> usize {
        let selected_run_id = self.selected_run_id();
        self.pending_checkpoints
            .keys()
            .filter(|id| Some(**id) != selected_run_id)
            .count()
    }

    pub fn workflow_count(&self) -> usize {
        self.registered.len().max(self.runs.len())
    }

    pub fn selected_workflow(&self) -> Option<&(String, WorkflowType, WorkflowState)> {
        self.registered.get(self.selected_run)
    }

    pub fn handle_daemon_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::RunUpdated(run) => {
                if let Some(existing) = self.runs.iter_mut().find(|r| r.id == run.id) {
                    *existing = run;
                } else {
                    self.runs.push(run);
                }
            }
            DaemonEvent::LogAppended(entry) => {
                self.logs.entry(entry.run_id).or_default().push(entry);
            }
            DaemonEvent::WorkflowRegistered { name, kind } => {
                if !self.registered.iter().any(|(n, _, _)| n == &name) {
                    self.registered.push((name, kind, WorkflowState::Dormant));
                }
            }
            DaemonEvent::CheckpointPending {
                run_id,
                message,
                feedback_available,
                ..
            } => {
                self.pending_checkpoints.insert(run_id, PendingCheckpoint {
                    run_id,
                    message,
                    feedback_available,
                });
            }
            DaemonEvent::WorkflowStateChanged { name, state } => {
                if let Some(entry) = self.registered.iter_mut().find(|(n, _, _)| n == &name) {
                    entry.2 = state;
                }
            }
        }
    }

    pub fn start_selected(&mut self) {
        if let Some((name, _, _)) = self.selected_workflow() {
            let name = name.clone();
            let _ = self.cmd_tx.try_send(DaemonCommand::Start { name });
        }
    }

    pub fn pause_selected(&mut self) {
        if let Some((name, _, _)) = self.selected_workflow() {
            let name = name.clone();
            let _ = self.cmd_tx.try_send(DaemonCommand::Pause { name });
        }
    }

    pub fn stop_selected(&mut self) {
        if let Some((name, _, _)) = self.selected_workflow() {
            let name = name.clone();
            let _ = self.cmd_tx.try_send(DaemonCommand::Stop { name });
        }
    }

    pub fn respond_checkpoint(&mut self, action: CheckpointAction) {
        if let Some(cp) = self.take_selected_checkpoint() {
            let _ = self.cmd_tx.try_send(DaemonCommand::CheckpointRespond {
                run_id: cp.run_id,
                action,
            });
        }
    }

    pub fn selected_run_id(&self) -> Option<Uuid> {
        if !self.registered.is_empty() {
            let name = &self.registered.get(self.selected_run)?.0;
            self.runs.iter().rev().find(|r| &r.workflow_name == name).map(|r| r.id)
        } else {
            self.runs.get(self.selected_run).map(|r| r.id)
        }
    }

    pub fn selected_workflow_state(&self) -> Option<WorkflowState> {
        self.registered.get(self.selected_run).map(|(_, _, s)| s.clone())
    }

    pub fn selected_workflow_kind(&self) -> Option<&WorkflowType> {
        self.registered.get(self.selected_run).map(|(_, k, _)| k)
    }

    pub fn selected_logs(&self) -> &[LogEntry] {
        self.selected_run_id()
            .and_then(|id| self.logs.get(&id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}
