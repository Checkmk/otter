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

pub struct WorkflowEntry {
    pub name: String,
    pub kind: WorkflowType,
    pub state: WorkflowState,
    pub runs: Vec<WorkflowRun>,
    pub expanded: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CursorTarget {
    Workflow(usize),          // index into workflows vec
    Run(usize, usize),        // workflow_idx, run_idx
}

pub struct App {
    pub workflows: Vec<WorkflowEntry>,
    pub cursor: CursorTarget,
    pub logs: HashMap<Uuid, Vec<LogEntry>>,
    pub pending_checkpoints: HashMap<Uuid, PendingCheckpoint>,
    pub feedback_input: String,
    pub mode: Mode,
    pub should_quit: bool,
    pub tick: u64,
    pub cmd_tx: mpsc::Sender<DaemonCommand>,
    /// Name of workflow we just started; when a new run appears for it, auto-expand
    pub pending_workflow_start: Option<String>,
}

impl App {
    pub fn new(cmd_tx: mpsc::Sender<DaemonCommand>) -> Self {
        Self {
            workflows: Vec::new(),
            cursor: CursorTarget::Workflow(0),
            logs: HashMap::new(),
            pending_checkpoints: HashMap::new(),
            feedback_input: String::new(),
            mode: Mode::Normal,
            should_quit: false,
            tick: 0,
            cmd_tx,
            pending_workflow_start: None,
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

    pub fn selected_workflow(&self) -> Option<&WorkflowEntry> {
        match self.cursor {
            CursorTarget::Workflow(wi) => self.workflows.get(wi),
            CursorTarget::Run(wi, _) => self.workflows.get(wi),
        }
    }

    pub fn handle_daemon_event(&mut self, event: DaemonEvent) {
        match event {
            DaemonEvent::RunUpdated(run) => {
                // Find the workflow by name and upsert the run
                if let Some(entry) = self.workflows.iter_mut().find(|e| e.name == run.workflow_name) {
                    if let Some(existing) = entry.runs.iter_mut().find(|r| r.id == run.id) {
                        *existing = run;
                    } else {
                        entry.runs.push(run);
                        // Sort by started_at descending (newest first)
                        entry.runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
                        // Automatically expand workflow if we just started it
                        if self.pending_workflow_start.as_ref() == Some(&entry.name) {
                            entry.expanded = true;
                            self.pending_workflow_start = None;
                        }
                    }
                }
            }
            DaemonEvent::LogAppended(entry) => {
                self.logs.entry(entry.run_id).or_default().push(entry);
            }
            DaemonEvent::WorkflowRegistered { name, kind } => {
                if !self.workflows.iter().any(|e| e.name == name) {
                    self.workflows.push(WorkflowEntry {
                        name,
                        kind,
                        state: WorkflowState::Dormant,
                        runs: Vec::new(),
                        expanded: false,
                    });
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
                if let Some(entry) = self.workflows.iter_mut().find(|e| e.name == name) {
                    entry.state = state;
                }
            }
            DaemonEvent::RunDeleted { run_id } => {
                // Remove from all workflows
                for entry in &mut self.workflows {
                    entry.runs.retain(|r| r.id != run_id);
                }
                self.logs.remove(&run_id);
                self.pending_checkpoints.remove(&run_id);
                self.ensure_cursor_valid();
            }
        }
    }

    fn ensure_cursor_valid(&mut self) {
        let flat = self.build_flat_list();
        if flat.is_empty() {
            self.cursor = CursorTarget::Workflow(0);
            return;
        }
        if !flat.iter().any(|t| *t == self.cursor) {
            self.cursor = flat[flat.len() - 1];
        }
    }

    pub fn start_selected(&mut self) {
        if let Some(entry) = self.selected_workflow() {
            let name = entry.name.clone();
            self.pending_workflow_start = Some(name.clone());
            let _ = self.cmd_tx.try_send(DaemonCommand::Start { name });
        }
    }

    pub fn pause_selected(&mut self) {
        if let Some(entry) = self.selected_workflow() {
            let name = entry.name.clone();
            let _ = self.cmd_tx.try_send(DaemonCommand::Pause { name });
        }
    }

    pub fn stop_selected(&mut self) {
        if let Some(entry) = self.selected_workflow() {
            let name = entry.name.clone();
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
        match self.cursor {
            CursorTarget::Workflow(_) => None,
            CursorTarget::Run(wi, ri) => {
                self.workflows.get(wi)?.runs.get(ri).map(|r| r.id)
            }
        }
    }

    pub fn selected_workflow_state(&self) -> Option<WorkflowState> {
        self.selected_workflow().map(|e| e.state.clone())
    }

    pub fn selected_workflow_kind(&self) -> Option<&WorkflowType> {
        self.selected_workflow().map(|e| &e.kind)
    }

    pub fn selected_logs(&self) -> &[LogEntry] {
        self.selected_run_id()
            .and_then(|id| self.logs.get(&id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Build a flat list of all cursor targets in navigation order
    fn build_flat_list(&self) -> Vec<CursorTarget> {
        let mut list = Vec::new();
        for (wi, entry) in self.workflows.iter().enumerate() {
            list.push(CursorTarget::Workflow(wi));
            if entry.expanded {
                for (ri, _) in entry.runs.iter().enumerate() {
                    list.push(CursorTarget::Run(wi, ri));
                }
            }
        }
        list
    }

    /// Navigate up one position in the unified flat list (wraps to bottom)
    pub fn move_cursor_up(&mut self) {
        let flat = self.build_flat_list();
        if flat.is_empty() {
            return;
        }
        if let Some(current_pos) = flat.iter().position(|t| *t == self.cursor) {
            self.cursor = flat[(current_pos + flat.len() - 1) % flat.len()];
        }
    }

    /// Navigate down one position in the unified flat list (wraps to top)
    pub fn move_cursor_down(&mut self) {
        let flat = self.build_flat_list();
        if flat.is_empty() {
            return;
        }
        if let Some(current_pos) = flat.iter().position(|t| *t == self.cursor) {
            self.cursor = flat[(current_pos + 1) % flat.len()];
        }
    }

    /// Toggle expanded state of the workflow at the current cursor (only if cursor is on a workflow)
    pub fn toggle_expanded(&mut self) {
        if let CursorTarget::Workflow(wi) = self.cursor {
            if let Some(entry) = self.workflows.get_mut(wi) {
                entry.expanded = !entry.expanded;
                // Collapse: snap cursor to the workflow row
                if !entry.expanded {
                    self.cursor = CursorTarget::Workflow(wi);
                }
            }
        }
    }

    /// Delete the currently selected run (only if cursor is on a run)
    pub fn delete_selected_run(&mut self) {
        if let CursorTarget::Run(_, _) = self.cursor {
            if let Some(run_id) = self.selected_run_id() {
                let _ = self.cmd_tx.try_send(DaemonCommand::DeleteRun { run_id });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestr8r_core::types::{WorkflowType, WorkflowState, DaemonEvent};
    use tokio::sync::mpsc;

    fn make_test_app() -> App {
        let (tx, _rx) = mpsc::channel(32);
        App::new(tx)
    }

    #[test]
    fn cursor_navigation_moves_through_workflows_only_when_collapsed() {
        let mut app = make_test_app();

        // Add two workflows
        app.workflows.push(WorkflowEntry {
            name: "wf1".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
        });
        app.workflows.push(WorkflowEntry {
            name: "wf2".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
        });

        // Start at first workflow
        app.cursor = CursorTarget::Workflow(0);

        // Move down
        app.move_cursor_down();
        assert_eq!(app.cursor, CursorTarget::Workflow(1));

        // Move down again (wraps to first)
        app.move_cursor_down();
        assert_eq!(app.cursor, CursorTarget::Workflow(0));

        // Move up (wraps to last)
        app.move_cursor_up();
        assert_eq!(app.cursor, CursorTarget::Workflow(1));
    }

    #[test]
    fn cursor_navigation_includes_runs_when_expanded() {
        use chrono::Duration;

        let mut app = make_test_app();

        // Add workflow with runs
        let run1 = WorkflowRun::new("wf1".to_string());
        let mut run2 = WorkflowRun::new("wf1".to_string());
        run2.started_at = run1.started_at + Duration::seconds(1);

        app.workflows.push(WorkflowEntry {
            name: "wf1".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run2.clone(), run1.clone()], // newest first
            expanded: true,
        });

        // Navigate through expanded workflow
        // Flat list should be: Workflow(0), Run(0, 0), Run(0, 1)
        app.cursor = CursorTarget::Workflow(0);

        app.move_cursor_down();
        assert_eq!(app.cursor, CursorTarget::Run(0, 0));

        app.move_cursor_down();
        assert_eq!(app.cursor, CursorTarget::Run(0, 1));

        // Move down from last wraps to first
        app.move_cursor_down();
        assert_eq!(app.cursor, CursorTarget::Workflow(0));

        // Move up from first wraps to last
        app.move_cursor_up();
        assert_eq!(app.cursor, CursorTarget::Run(0, 1));

        app.move_cursor_up();
        assert_eq!(app.cursor, CursorTarget::Run(0, 0));
    }

    #[test]
    fn toggle_expanded_expands_and_collapses_workflow() {
        let mut app = make_test_app();

        let run = WorkflowRun::new("wf".to_string());
        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run],
            expanded: false,
        });

        // Select the workflow
        app.cursor = CursorTarget::Workflow(0);

        // Toggle to expand
        app.toggle_expanded();
        assert!(app.workflows[0].expanded);

        // Toggle to collapse
        app.toggle_expanded();
        assert!(!app.workflows[0].expanded);
    }

    #[test]
    fn toggle_expanded_snaps_cursor_to_workflow_when_collapsing() {
        let mut app = make_test_app();

        let run = WorkflowRun::new("wf".to_string());
        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run],
            expanded: true,
        });

        // Start with cursor on the workflow (not on a run)
        app.cursor = CursorTarget::Workflow(0);

        // Toggle to collapse
        app.toggle_expanded();

        // Cursor should remain on the workflow
        assert_eq!(app.cursor, CursorTarget::Workflow(0));
        assert!(!app.workflows[0].expanded);

        // Toggle again to expand
        app.toggle_expanded();
        assert_eq!(app.cursor, CursorTarget::Workflow(0));
        assert!(app.workflows[0].expanded);
    }

    #[test]
    fn selected_run_id_returns_none_when_cursor_on_workflow() {
        let mut app = make_test_app();

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
        });

        app.cursor = CursorTarget::Workflow(0);
        assert!(app.selected_run_id().is_none());
    }

    #[test]
    fn selected_run_id_returns_run_id_when_cursor_on_run() {
        let mut app = make_test_app();

        let run = WorkflowRun::new("wf".to_string());
        let run_id = run.id;

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run],
            expanded: true,
        });

        app.cursor = CursorTarget::Run(0, 0);
        assert_eq!(app.selected_run_id(), Some(run_id));
    }

    #[test]
    fn handle_daemon_event_run_deleted_removes_run_from_workflows() {
        let mut app = make_test_app();

        let run1 = WorkflowRun::new("wf".to_string());
        let run2 = WorkflowRun::new("wf".to_string());
        let run1_id = run1.id;

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run2.clone(), run1.clone()],
            expanded: true,
        });

        // Handle RunDeleted event
        app.handle_daemon_event(DaemonEvent::RunDeleted { run_id: run1_id });

        // run1 should be removed
        assert_eq!(app.workflows[0].runs.len(), 1);
        assert_eq!(app.workflows[0].runs[0].id, run2.id);
    }

    #[test]
    fn handle_daemon_event_run_updated_inserts_and_sorts_by_started_at() {
        let mut app = make_test_app();

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
        });

        // Add runs in non-chronological order
        let mut run1 = WorkflowRun::new("wf".to_string());
        let mut run2 = WorkflowRun::new("wf".to_string());

        run1.started_at = chrono::Utc::now();
        run2.started_at = run1.started_at + chrono::Duration::seconds(10);

        // Add run2 first
        app.handle_daemon_event(DaemonEvent::RunUpdated(run2.clone()));
        assert_eq!(app.workflows[0].runs.len(), 1);

        // Add run1
        app.handle_daemon_event(DaemonEvent::RunUpdated(run1.clone()));
        assert_eq!(app.workflows[0].runs.len(), 2);

        // Verify sorting: newest first
        assert_eq!(app.workflows[0].runs[0].id, run2.id);
        assert_eq!(app.workflows[0].runs[1].id, run1.id);
    }

    #[test]
    fn deleting_selected_run_snaps_cursor_to_valid_position() {
        let mut app = make_test_app();

        let run1 = WorkflowRun::new("wf".to_string());
        let run2 = WorkflowRun::new("wf".to_string());
        let run1_id = run1.id;
        let run2_id = run2.id;

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run2, run1],
            expanded: true,
        });

        // Cursor on the last run
        app.cursor = CursorTarget::Run(0, 1);
        assert_eq!(app.selected_run_id(), Some(run1_id));

        // Delete the selected run
        app.handle_daemon_event(DaemonEvent::RunDeleted { run_id: run1_id });

        // Cursor should snap to the remaining run
        assert_eq!(app.cursor, CursorTarget::Run(0, 0));
        assert_eq!(app.selected_run_id(), Some(run2_id));
    }

    #[test]
    fn deleting_last_run_from_expanded_workflow_snaps_cursor_to_workflow() {
        let mut app = make_test_app();

        let run = WorkflowRun::new("wf".to_string());
        let run_id = run.id;

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run],
            expanded: true,
        });

        // Cursor on the only run
        app.cursor = CursorTarget::Run(0, 0);
        assert_eq!(app.selected_run_id(), Some(run_id));

        // Delete the run
        app.handle_daemon_event(DaemonEvent::RunDeleted { run_id });

        // Cursor should snap to the workflow row
        assert_eq!(app.cursor, CursorTarget::Workflow(0));
        assert_eq!(app.selected_run_id(), None);
    }

    #[test]
    fn handle_daemon_event_run_updated_does_not_auto_expand_without_start() {
        let mut app = make_test_app();

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
        });

        // Add a new run without starting the workflow
        let run = WorkflowRun::new("wf".to_string());
        app.handle_daemon_event(DaemonEvent::RunUpdated(run));

        // Workflow should NOT be expanded
        assert!(!app.workflows[0].expanded);
    }

    #[test]
    fn handle_daemon_event_run_updated_expands_workflow_when_just_started() {
        let mut app = make_test_app();

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
        });

        // Start the workflow
        app.cursor = CursorTarget::Workflow(0);
        app.start_selected();

        // Add a new run
        let run = WorkflowRun::new("wf".to_string());
        app.handle_daemon_event(DaemonEvent::RunUpdated(run));

        // Workflow should be expanded
        assert!(app.workflows[0].expanded);
        // pending_workflow_start should be cleared
        assert!(app.pending_workflow_start.is_none());
    }
}
