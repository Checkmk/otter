use std::collections::HashMap;

use orchestr8r_core::types::{CheckpointResponse, LogEntry, EngineEvent, WorkflowKind, WorkflowRun};
use tokio::sync::oneshot;
use uuid::Uuid;

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    FeedbackInput,
}

pub struct PendingCheckpoint {
    pub message: String,
    pub feedback_available: bool,
    pub response_tx: oneshot::Sender<CheckpointResponse>,
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
}

impl App {
    pub fn new() -> Self {
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

    pub fn handle_engine_event(&mut self, event: EngineEvent) {
        match event {
            EngineEvent::RunUpdated(run) => {
                if let Some(existing) = self.runs.iter_mut().find(|r| r.id == run.id) {
                    *existing = run;
                } else {
                    self.runs.push(run);
                }
            }
            EngineEvent::LogAppended(entry) => {
                self.logs.entry(entry.run_id).or_default().push(entry);
            }
            EngineEvent::WorkflowRegistered { name, kind } => {
                if !self.registered.iter().any(|(n, _)| n == &name) {
                    self.registered.push((name, kind));
                }
            }
            EngineEvent::CheckpointPending {
                message,
                feedback_available,
                response_tx,
                ..
            } => {
                self.pending_checkpoints.push(PendingCheckpoint {
                    message,
                    feedback_available,
                    response_tx,
                });
            }
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
