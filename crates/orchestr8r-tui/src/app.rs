use std::collections::HashMap;

use orchestr8r_core::types::{CheckpointAction, DaemonCommand, DaemonEvent, LogEntry, WorkflowKind, WorkflowRun};
use tokio::sync::mpsc;
use uuid::Uuid;

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
    pub registered: Vec<(String, WorkflowKind)>,
    pub selected_run: usize,
    pub logs: HashMap<Uuid, Vec<LogEntry>>,
    pub pending_checkpoints: Vec<PendingCheckpoint>,
    pub selected_checkpoint: usize,
    pub feedback_input: String,
    pub mode: Mode,
    pub should_quit: bool,
    pub cmd_tx: mpsc::Sender<DaemonCommand>,
}

impl App {
    pub fn new(cmd_tx: mpsc::Sender<DaemonCommand>) -> Self {
        Self {
            runs: Vec::new(),
            registered: Vec::new(),
            selected_run: 0,
            logs: HashMap::new(),
            pending_checkpoints: Vec::new(),
            selected_checkpoint: 0,
            feedback_input: String::new(),
            mode: Mode::Normal,
            should_quit: false,
            cmd_tx,
        }
    }

    pub fn active_checkpoint(&self) -> Option<&PendingCheckpoint> {
        self.pending_checkpoints.get(self.selected_checkpoint)
    }

    pub fn take_selected_checkpoint(&mut self) -> Option<PendingCheckpoint> {
        if self.pending_checkpoints.is_empty() {
            return None;
        }
        let cp = self.pending_checkpoints.remove(self.selected_checkpoint);
        if self.selected_checkpoint >= self.pending_checkpoints.len() {
            self.selected_checkpoint = self.pending_checkpoints.len().saturating_sub(1);
        }
        Some(cp)
    }

    pub fn workflow_count(&self) -> usize {
        self.registered.len().max(self.runs.len())
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
                if !self.registered.iter().any(|(n, _)| n == &name) {
                    self.registered.push((name, kind));
                }
            }
            DaemonEvent::CheckpointPending {
                run_id,
                message,
                feedback_available,
                ..
            } => {
                self.pending_checkpoints.push(PendingCheckpoint {
                    run_id,
                    message,
                    feedback_available,
                });
            }
            DaemonEvent::WorkflowStateChanged { .. } => {
                // Phase 4 will handle TUI state updates; ignore for now.
            }
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

    pub fn selected_logs(&self) -> &[LogEntry] {
        self.selected_run_id()
            .and_then(|id| self.logs.get(&id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }
}
