use std::collections::HashMap;

use chrono::Utc;
use otter_core::types::{
    CheckpointAction, DaemonCommand, DaemonEvent, LogEntry, ProgressChunk, TriggerDef, WorkflowRun,
    WorkflowState, WorkflowStatus, WorkflowType,
};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    FeedbackInput,
}

#[derive(Debug, PartialEq)]
pub enum Modal {
    Help { scroll: usize },
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Focus {
    Left,
    Right,
}

#[derive(Debug, PartialEq, Clone)]
pub enum RightPanelContent {
    Contextual,
    ConsumedTriggers,
}

pub struct PendingCheckpoint {
    pub run_id: Uuid,
    pub feedback_available: bool,
    pub processing: bool,
}

pub struct WorkflowEntry {
    pub name: String,
    pub kind: WorkflowType,
    pub state: WorkflowState,
    pub runs: Vec<WorkflowRun>,
    pub expanded: bool,
    pub trigger: Option<TriggerDef>,
    pub toml_content: Option<String>,
    pub autostart: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CursorTarget {
    Workflow(usize),   // index into workflows vec
    Run(usize, usize), // workflow_idx, run_idx
}

pub struct App {
    pub workflows: Vec<WorkflowEntry>,
    pub cursor: CursorTarget,
    pub logs: HashMap<Uuid, Vec<LogEntry>>,
    pub pending_checkpoints: HashMap<Uuid, PendingCheckpoint>,
    pub feedback_input: String,
    pub mode: Mode,
    pub modal: Option<Modal>,
    pub should_quit: bool,
    pub tick: u64,
    pub cmd_tx: mpsc::Sender<DaemonCommand>,
    /// Name of workflow we just started; when a new run appears for it, auto-expand
    pub pending_workflow_start: Option<String>,
    pub focus: Focus,
    pub right_panel_content: RightPanelContent,
    pub right_cursor: usize,
    pub right_scroll: usize,
    pub right_panel_height: usize,
    pub consumed_triggers: HashMap<String, Vec<String>>,
    pub progress: HashMap<Uuid, Vec<(usize, ProgressChunk)>>,
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
            modal: None,
            should_quit: false,
            tick: 0,
            cmd_tx,
            pending_workflow_start: None,
            focus: Focus::Left,
            right_panel_content: RightPanelContent::Contextual,
            right_cursor: 0,
            right_scroll: 0,
            right_panel_height: 0,
            consumed_triggers: HashMap::new(),
            progress: HashMap::new(),
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
                if let Some((wi, entry)) = self
                    .workflows
                    .iter_mut()
                    .enumerate()
                    .find(|(_, e)| e.name == run.workflow_name)
                {
                    if let Some(existing) = entry.runs.iter_mut().find(|r| r.id == run.id) {
                        *existing = run;
                    } else {
                        entry.runs.push(run);
                        // Sort by started_at descending (newest first)
                        entry.runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
                        // Automatically expand and focus the new run if we just started it
                        if self.pending_workflow_start.as_ref() == Some(&entry.name) {
                            entry.expanded = true;
                            self.pending_workflow_start = None;
                            self.cursor = CursorTarget::Run(wi, 0);
                        }
                    }
                }
            }
            DaemonEvent::LogAppended(entry) => {
                self.logs.entry(entry.run_id).or_default().push(entry);
            }
            DaemonEvent::WorkflowsSnapshot(snapshot) => self.apply_workflows_snapshot(snapshot),
            DaemonEvent::CheckpointPending {
                run_id,
                step_index,
                message,
                feedback_available,
            } => {
                // Inject checkpoint message as a synthetic log entry only on first presentation;
                // feedback loops re-emit CheckpointPending for the same step, so skip if already pending.
                if !self.pending_checkpoints.contains_key(&run_id) {
                    let iteration = self
                        .logs
                        .get(&run_id)
                        .and_then(|l| l.last())
                        .map(|e| e.iteration)
                        .unwrap_or(0);
                    self.logs.entry(run_id).or_default().push(LogEntry {
                        run_id,
                        iteration,
                        step_index,
                        step_type: "checkpoint".to_string(),
                        stdout: message.clone(),
                        stderr: String::new(),
                        exit_code: None,
                        accepted: None,
                        feedback: None,
                        timestamp: Utc::now(),
                    });
                }

                self.pending_checkpoints.insert(
                    run_id,
                    PendingCheckpoint {
                        run_id,
                        feedback_available,
                        processing: false,
                    },
                );
            }
            DaemonEvent::RunDeleted { run_id } => {
                let old_pos = self
                    .build_flat_list()
                    .iter()
                    .position(|t| *t == self.cursor);
                for entry in &mut self.workflows {
                    entry.runs.retain(|r| r.id != run_id);
                }
                self.logs.remove(&run_id);
                self.progress.remove(&run_id);
                self.pending_checkpoints.remove(&run_id);
                self.ensure_cursor_valid(old_pos);
            }
            DaemonEvent::StepProgress {
                run_id,
                step_index,
                chunk,
            } => {
                self.progress
                    .entry(run_id)
                    .or_default()
                    .push((step_index, chunk));
            }
            DaemonEvent::ConsumedTriggersChanged { workflow, triggers } => {
                self.consumed_triggers.insert(workflow, triggers);
                // Clamp right_cursor in case the list shrank
                let len = self.selected_consumed_triggers().len();
                if len == 0 {
                    self.right_cursor = 0;
                } else {
                    self.right_cursor = self.right_cursor.min(len - 1);
                }
            }
        }
    }

    fn apply_workflows_snapshot(&mut self, snapshot: Vec<WorkflowStatus>) {
        let old_pos = self
            .build_flat_list()
            .iter()
            .position(|t| *t == self.cursor);
        let snapshot_names: std::collections::HashSet<&str> =
            snapshot.iter().map(|s| s.name.as_str()).collect();

        self.workflows
            .retain(|e| snapshot_names.contains(e.name.as_str()));

        for incoming in snapshot {
            match self.workflows.iter_mut().find(|e| e.name == incoming.name) {
                Some(entry) => {
                    entry.kind = incoming.kind;
                    entry.state = incoming.state;
                    entry.trigger = incoming.trigger;
                    entry.toml_content = incoming.toml_content;
                    entry.autostart = incoming.enabled;
                }
                None => {
                    self.workflows.push(WorkflowEntry {
                        name: incoming.name,
                        kind: incoming.kind,
                        state: incoming.state,
                        runs: Vec::new(),
                        expanded: false,
                        trigger: incoming.trigger,
                        toml_content: incoming.toml_content,
                        autostart: incoming.enabled,
                    });
                }
            }
        }

        self.ensure_cursor_valid(old_pos);
    }

    fn ensure_cursor_valid(&mut self, preferred_pos: Option<usize>) {
        let flat = self.build_flat_list();
        if flat.is_empty() {
            self.cursor = CursorTarget::Workflow(0);
            return;
        }
        if !flat.contains(&self.cursor) {
            let target = preferred_pos
                .map(|p| p.saturating_sub(1).min(flat.len() - 1))
                .unwrap_or(flat.len() - 1);
            self.cursor = flat[target];
        }
    }

    pub fn start_selected(&mut self) {
        if let Some(entry) = self.selected_workflow() {
            let name = entry.name.clone();
            self.pending_workflow_start = Some(name.clone());
            let _ = self.cmd_tx.try_send(DaemonCommand::Start { name });
        }
    }

    pub fn stop_selected_run(&mut self) {
        if let CursorTarget::Run(_, _) = self.cursor {
            if let Some(run_id) = self.selected_run_id() {
                let _ = self.cmd_tx.try_send(DaemonCommand::StopRun { run_id });
            }
        }
    }

    pub fn stop_selected(&mut self) {
        if let Some(entry) = self.selected_workflow() {
            let name = entry.name.clone();
            let _ = self.cmd_tx.try_send(DaemonCommand::Stop { name });
        }
    }

    pub fn toggle_enable_selected(&mut self) {
        if let CursorTarget::Workflow(wi) = self.cursor {
            if let Some(entry) = self.workflows.get_mut(wi) {
                let name = entry.name.clone();
                if entry.autostart {
                    entry.autostart = false;
                    let _ = self
                        .cmd_tx
                        .try_send(DaemonCommand::DisableWorkflow { name });
                } else {
                    entry.autostart = true;
                    self.pending_workflow_start = Some(name.clone());
                    let _ = self.cmd_tx.try_send(DaemonCommand::EnableWorkflow { name });
                }
            }
        }
    }

    pub fn respond_checkpoint(&mut self, action: CheckpointAction) {
        // For Feedback, keep the checkpoint in pending_checkpoints so that when the engine
        // re-presents the checkpoint after processing the feedback, the guard in
        // handle_daemon_event fires and skips injecting a duplicate synthetic log entry.
        let run_id = match &action {
            CheckpointAction::Feedback(_) => self
                .selected_run_id()
                .filter(|id| self.pending_checkpoints.contains_key(id)),
            _ => self.take_selected_checkpoint().map(|cp| cp.run_id),
        };
        if let Some(run_id) = run_id {
            if matches!(action, CheckpointAction::Feedback(_)) {
                if let Some(cp) = self.pending_checkpoints.get_mut(&run_id) {
                    cp.processing = true;
                }
            }
            let _ = self
                .cmd_tx
                .try_send(DaemonCommand::CheckpointRespond { run_id, action });
        }
    }

    pub fn selected_run_id(&self) -> Option<Uuid> {
        match self.cursor {
            CursorTarget::Workflow(_) => None,
            CursorTarget::Run(wi, ri) => self.workflows.get(wi)?.runs.get(ri).map(|r| r.id),
        }
    }

    pub fn selected_workflow_state(&self) -> Option<WorkflowState> {
        self.selected_workflow().map(|e| e.state.clone())
    }

    pub fn selected_logs(&self) -> &[LogEntry] {
        self.selected_run_id()
            .and_then(|id| self.logs.get(&id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn selected_progress(&self) -> &[(usize, ProgressChunk)] {
        self.selected_run_id()
            .and_then(|id| self.progress.get(&id))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn selected_workflow_toml(&self) -> Option<&str> {
        match self.cursor {
            CursorTarget::Workflow(wi) => self.workflows.get(wi)?.toml_content.as_deref(),
            CursorTarget::Run(_, _) => None,
        }
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
        self.close_right_panel();
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
        self.close_right_panel();
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
                if entry.runs.is_empty() {
                    return;
                }
                entry.expanded = !entry.expanded;
                // Collapse: snap cursor to the workflow row
                if !entry.expanded {
                    self.cursor = CursorTarget::Workflow(wi);
                }
            }
        }
    }

    pub fn cursor_is_polling_workflow(&self) -> bool {
        matches!(
            self.selected_workflow().and_then(|e| e.trigger.as_ref()),
            Some(TriggerDef::Polling { .. })
        )
    }

    pub fn open_consumed_triggers(&mut self) {
        if !self.cursor_is_polling_workflow() {
            return;
        }
        if let Some(name) = self.selected_workflow().map(|e| e.name.clone()) {
            self.consumed_triggers.entry(name.clone()).or_default();
            self.right_panel_content = RightPanelContent::ConsumedTriggers;
            self.focus = Focus::Right;
            self.right_cursor = 0;
            let _ = self
                .cmd_tx
                .try_send(DaemonCommand::ListConsumedTriggers { workflow: name });
        }
    }

    pub fn enter_right_panel(&mut self) {
        self.focus = Focus::Right;
        self.right_panel_content = RightPanelContent::Contextual;
        self.right_scroll = 0;
    }

    pub fn close_right_panel(&mut self) {
        self.focus = Focus::Left;
        self.right_panel_content = RightPanelContent::Contextual;
        self.right_scroll = 0;
    }

    pub fn move_right_up(&mut self) {
        match self.right_panel_content {
            RightPanelContent::ConsumedTriggers => self.move_right_cursor_up(),
            RightPanelContent::Contextual => match self.cursor {
                // TOML: right_scroll is absolute from top — ↑ means smaller offset (toward top)
                CursorTarget::Workflow(_) => {
                    self.right_scroll = self.right_scroll.saturating_sub(1)
                }
                // Logs: right_scroll is lines from bottom — ↑ means more lines from bottom (toward top)
                CursorTarget::Run(_, _) => self.right_scroll += 1,
            },
        }
    }

    pub fn move_right_down(&mut self) {
        match self.right_panel_content {
            RightPanelContent::ConsumedTriggers => self.move_right_cursor_down(),
            RightPanelContent::Contextual => match self.cursor {
                // TOML: right_scroll is absolute from top — ↓ means larger offset (toward bottom)
                CursorTarget::Workflow(_) => self.right_scroll += 1,
                // Logs: right_scroll is lines from bottom — ↓ means fewer lines from bottom (toward bottom)
                CursorTarget::Run(_, _) => self.right_scroll = self.right_scroll.saturating_sub(1),
            },
        }
    }

    pub fn scroll_right_page_up(&mut self) {
        match self.cursor {
            CursorTarget::Workflow(_) => {
                self.right_scroll = self.right_scroll.saturating_sub(self.right_panel_height)
            }
            CursorTarget::Run(_, _) => self.right_scroll += self.right_panel_height,
        }
    }

    pub fn scroll_right_page_down(&mut self) {
        match self.cursor {
            CursorTarget::Workflow(_) => self.right_scroll += self.right_panel_height,
            CursorTarget::Run(_, _) => {
                self.right_scroll = self.right_scroll.saturating_sub(self.right_panel_height)
            }
        }
    }

    pub fn scroll_right_half_page_up(&mut self) {
        let half = (self.right_panel_height / 2).max(1);
        match self.cursor {
            CursorTarget::Workflow(_) => self.right_scroll = self.right_scroll.saturating_sub(half),
            CursorTarget::Run(_, _) => self.right_scroll += half,
        }
    }

    pub fn scroll_right_half_page_down(&mut self) {
        let half = (self.right_panel_height / 2).max(1);
        match self.cursor {
            CursorTarget::Workflow(_) => self.right_scroll += half,
            CursorTarget::Run(_, _) => self.right_scroll = self.right_scroll.saturating_sub(half),
        }
    }

    pub fn scroll_right_top(&mut self) {
        match self.cursor {
            // TOML: absolute offset — top is 0
            CursorTarget::Workflow(_) => self.right_scroll = 0,
            // Logs: lines-from-bottom — top is usize::MAX (clamped to auto_bottom on render)
            CursorTarget::Run(_, _) => self.right_scroll = usize::MAX,
        }
    }

    pub fn scroll_right_bottom(&mut self) {
        // Bottom is 0 for logs (live tail), usize::MAX for TOML (clamped on render)
        match self.cursor {
            CursorTarget::Workflow(_) => self.right_scroll = usize::MAX,
            CursorTarget::Run(_, _) => self.right_scroll = 0,
        }
    }

    pub fn selected_consumed_triggers(&self) -> &[String] {
        let name = match self.selected_workflow() {
            Some(e) => &e.name,
            None => return &[],
        };
        self.consumed_triggers
            .get(name)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn move_right_cursor_up(&mut self) {
        let len = self.selected_consumed_triggers().len();
        if len == 0 {
            return;
        }
        self.right_cursor = (self.right_cursor + len - 1) % len;
    }

    pub fn move_right_cursor_down(&mut self) {
        let len = self.selected_consumed_triggers().len();
        if len == 0 {
            return;
        }
        self.right_cursor = (self.right_cursor + 1) % len;
    }

    pub fn delete_selected_consumed_trigger(&mut self) {
        let workflow = match self.selected_workflow().map(|e| e.name.clone()) {
            Some(n) => n,
            None => return,
        };
        let triggers = match self.consumed_triggers.get_mut(&workflow) {
            Some(t) => t,
            None => return,
        };
        if self.right_cursor >= triggers.len() {
            return;
        }
        let trigger = triggers.remove(self.right_cursor);
        if !triggers.is_empty() {
            self.right_cursor = self.right_cursor.min(triggers.len() - 1);
        } else {
            self.right_cursor = 0;
        }
        let _ = self
            .cmd_tx
            .try_send(DaemonCommand::DeleteConsumedTrigger { workflow, trigger });
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
    use otter_core::types::{DaemonEvent, WorkflowState, WorkflowType};
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
            trigger: None,
            toml_content: None,
            autostart: false,
        });
        app.workflows.push(WorkflowEntry {
            name: "wf2".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
            trigger: None,
            toml_content: None,
            autostart: false,
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
            trigger: None,
            toml_content: None,
            autostart: false,
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
            trigger: None,
            toml_content: None,
            autostart: false,
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
            trigger: None,
            toml_content: None,
            autostart: false,
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
            trigger: None,
            toml_content: None,
            autostart: false,
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
            trigger: None,
            toml_content: None,
            autostart: false,
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
            trigger: None,
            toml_content: None,
            autostart: false,
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
            trigger: None,
            toml_content: None,
            autostart: false,
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
    fn deleting_selected_run_snaps_cursor_to_previous_run() {
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
            trigger: None,
            toml_content: None,
            autostart: false,
        });

        // Cursor on the last run (index 1)
        app.cursor = CursorTarget::Run(0, 1);
        assert_eq!(app.selected_run_id(), Some(run1_id));

        // Delete the selected run
        app.handle_daemon_event(DaemonEvent::RunDeleted { run_id: run1_id });

        // Cursor should snap to the previous run (index 0), not jump to another workflow
        assert_eq!(app.cursor, CursorTarget::Run(0, 0));
        assert_eq!(app.selected_run_id(), Some(run2_id));
    }

    #[test]
    fn deleting_run_does_not_jump_to_another_workflow() {
        use chrono::Duration;

        let mut app = make_test_app();

        let run_a = WorkflowRun::new("wf-a".to_string());
        let run_b0 = WorkflowRun::new("wf-b".to_string());
        let mut run_b1 = WorkflowRun::new("wf-b".to_string());
        run_b1.started_at = run_b0.started_at + Duration::seconds(1);
        let run_b1_id = run_b1.id;

        // wf-a has one run; wf-b has two runs (b1 newest first)
        app.workflows.push(WorkflowEntry {
            name: "wf-a".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run_a],
            expanded: false,
            trigger: None,
            toml_content: None,
            autostart: false,
        });
        app.workflows.push(WorkflowEntry {
            name: "wf-b".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run_b1, run_b0.clone()],
            expanded: true,
            trigger: None,
            toml_content: None,
            autostart: false,
        });

        // Cursor on the second run of wf-b (the older one)
        app.cursor = CursorTarget::Run(1, 1);

        // Delete that run
        app.handle_daemon_event(DaemonEvent::RunDeleted { run_id: run_b0.id });

        // Should snap to Run(1, 0) — the first run of wf-b — NOT to wf-a or run_b1_id accidentally
        assert_eq!(app.cursor, CursorTarget::Run(1, 0));
        assert_eq!(app.selected_run_id(), Some(run_b1_id));
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
            trigger: None,
            toml_content: None,
            autostart: false,
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
    fn handle_daemon_event_run_updated_moves_cursor_to_new_run_when_just_started() {
        let mut app = make_test_app();

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
            trigger: None,
            toml_content: None,
            autostart: false,
        });

        app.cursor = CursorTarget::Workflow(0);
        app.start_selected();

        let run = WorkflowRun::new("wf".to_string());
        app.handle_daemon_event(DaemonEvent::RunUpdated(run));

        // Cursor should move to the new run, not stay on the workflow row
        assert_eq!(app.cursor, CursorTarget::Run(0, 0));
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
            trigger: None,
            toml_content: None,
            autostart: false,
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
            trigger: None,
            toml_content: None,
            autostart: false,
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

    #[test]
    fn feedback_processing_set_on_feedback_and_cleared_on_checkpoint_repending() {
        use otter_core::types::{CheckpointAction, RunStatus};

        let mut app = make_test_app();

        let mut run = WorkflowRun::new("wf".to_string());
        run.status = RunStatus::WaitingCheckpoint;
        let run_id = run.id;

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Running,
            runs: vec![run],
            expanded: true,
            trigger: None,
            toml_content: None,
            autostart: false,
        });
        app.cursor = CursorTarget::Run(0, 0);

        // Simulate checkpoint pending
        app.handle_daemon_event(DaemonEvent::CheckpointPending {
            run_id,
            step_index: 0,
            message: "Review?".to_string(),
            feedback_available: true,
        });
        assert!(app.pending_checkpoints.contains_key(&run_id));
        assert!(!app.pending_checkpoints[&run_id].processing);

        // WHEN user submits feedback
        app.respond_checkpoint(CheckpointAction::Feedback("fix this".to_string()));

        // THEN processing is set, checkpoint still pending
        assert!(app.pending_checkpoints[&run_id].processing);

        // WHEN agent finishes and checkpoint re-presents
        app.handle_daemon_event(DaemonEvent::CheckpointPending {
            run_id,
            step_index: 0,
            message: "Review?".to_string(),
            feedback_available: true,
        });

        // THEN processing is cleared
        assert!(!app.pending_checkpoints[&run_id].processing);
    }

    #[test]
    fn selected_workflow_toml_returns_content_when_cursor_on_workflow() {
        let mut app = make_test_app();

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
            trigger: None,
            toml_content: Some("name = \"wf\"\ntype = \"looping\"\n".to_string()),
            autostart: false,
        });

        app.cursor = CursorTarget::Workflow(0);
        assert_eq!(
            app.selected_workflow_toml(),
            Some("name = \"wf\"\ntype = \"looping\"\n")
        );
    }

    #[test]
    fn selected_workflow_toml_returns_none_when_toml_absent() {
        let mut app = make_test_app();

        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
            trigger: None,
            toml_content: None,
            autostart: false,
        });

        app.cursor = CursorTarget::Workflow(0);
        assert_eq!(app.selected_workflow_toml(), None);
    }

    #[test]
    fn selected_workflow_toml_returns_none_when_cursor_on_run() {
        let mut app = make_test_app();

        let run = WorkflowRun::new("wf".to_string());
        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run],
            expanded: true,
            trigger: None,
            toml_content: Some("name = \"wf\"\n".to_string()),
            autostart: false,
        });

        app.cursor = CursorTarget::Run(0, 0);
        assert_eq!(app.selected_workflow_toml(), None);
    }

    fn snap(name: &str, toml_content: Option<&str>, enabled: bool) -> WorkflowStatus {
        WorkflowStatus {
            name: name.to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            trigger: None,
            toml_content: toml_content.map(str::to_string),
            enabled,
        }
    }

    #[test]
    fn workflows_snapshot_stores_toml_content() {
        let mut app = make_test_app();

        app.handle_daemon_event(DaemonEvent::WorkflowsSnapshot(vec![snap(
            "wf",
            Some("name = \"wf\"\ntype = \"looping\"\n"),
            false,
        )]));

        assert_eq!(app.workflows.len(), 1);
        assert_eq!(
            app.workflows[0].toml_content.as_deref(),
            Some("name = \"wf\"\ntype = \"looping\"\n")
        );
    }

    #[test]
    fn step_progress_accumulates_and_persists_after_log() {
        // GIVEN
        let mut app = make_test_app();
        let run_id = Uuid::new_v4();

        // WHEN progress arrives
        app.handle_daemon_event(DaemonEvent::StepProgress {
            run_id,
            step_index: 0,
            chunk: ProgressChunk::Status("Thinking...".to_string()),
        });
        app.handle_daemon_event(DaemonEvent::StepProgress {
            run_id,
            step_index: 0,
            chunk: ProgressChunk::Status("Using tool: Read".to_string()),
        });

        // THEN — progress accumulates
        assert_eq!(app.progress.get(&run_id).unwrap().len(), 2);

        // WHEN a LogAppended arrives for the same run
        app.handle_daemon_event(DaemonEvent::LogAppended(LogEntry {
            run_id,
            iteration: 0,
            step_index: 0,
            step_type: "agent".to_string(),
            stdout: "done".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            accepted: None,
            feedback: None,
            timestamp: Utc::now(),
        }));

        // THEN — progress persists (not cleared)
        assert_eq!(app.progress.get(&run_id).unwrap().len(), 2);
        // AND the log entry was added
        assert_eq!(app.logs.get(&run_id).unwrap().len(), 1);
    }

    #[test]
    fn snapshot_preserves_runs_and_expanded_for_existing_workflow() {
        // GIVEN a workflow with a run and expanded=true
        let mut app = make_test_app();
        let run = WorkflowRun::new("wf".to_string());
        let run_id = run.id;
        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![run],
            expanded: true,
            trigger: None,
            toml_content: None,
            autostart: false,
        });

        // WHEN a snapshot arrives that still contains wf (with a state change and toml)
        app.handle_daemon_event(DaemonEvent::WorkflowsSnapshot(vec![WorkflowStatus {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Running,
            trigger: None,
            toml_content: Some("name = \"wf\"\n".to_string()),
            enabled: true,
        }]));

        // THEN runs and expanded are preserved; state, toml, autostart updated
        assert_eq!(app.workflows.len(), 1);
        assert_eq!(app.workflows[0].runs.len(), 1);
        assert_eq!(app.workflows[0].runs[0].id, run_id);
        assert!(app.workflows[0].expanded);
        assert_eq!(app.workflows[0].state, WorkflowState::Running);
        assert_eq!(
            app.workflows[0].toml_content.as_deref(),
            Some("name = \"wf\"\n")
        );
        assert!(app.workflows[0].autostart);
    }

    #[test]
    fn snapshot_removes_workflows_not_in_payload() {
        // GIVEN two workflows
        let mut app = make_test_app();
        app.workflows.push(WorkflowEntry {
            name: "wf-a".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![WorkflowRun::new("wf-a".to_string())],
            expanded: false,
            trigger: None,
            toml_content: None,
            autostart: false,
        });
        app.workflows.push(WorkflowEntry {
            name: "wf-b".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
            trigger: None,
            toml_content: None,
            autostart: false,
        });

        // WHEN a snapshot arrives that only contains wf-b
        app.handle_daemon_event(DaemonEvent::WorkflowsSnapshot(vec![snap(
            "wf-b", None, false,
        )]));

        // THEN wf-a is gone, wf-b remains
        assert_eq!(app.workflows.len(), 1);
        assert_eq!(app.workflows[0].name, "wf-b");
    }

    #[test]
    fn snapshot_adds_new_workflows_with_empty_runs() {
        // GIVEN an empty app
        let mut app = make_test_app();

        // WHEN a snapshot arrives with a new workflow
        app.handle_daemon_event(DaemonEvent::WorkflowsSnapshot(vec![snap(
            "wf-new", None, false,
        )]));

        // THEN the workflow is added with empty runs
        assert_eq!(app.workflows.len(), 1);
        assert_eq!(app.workflows[0].name, "wf-new");
        assert!(app.workflows[0].runs.is_empty());
        assert!(!app.workflows[0].expanded);
    }

    #[test]
    fn toggle_enable_selected_enables_workflow() {
        // GIVEN a disabled workflow
        let (tx, mut rx) = mpsc::channel(32);
        let mut app = App::new(tx);
        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
            trigger: None,
            toml_content: None,
            autostart: false,
        });
        app.cursor = CursorTarget::Workflow(0);

        // WHEN toggled
        app.toggle_enable_selected();

        // THEN enabled flips to true, pending_workflow_start set, EnableWorkflow sent
        assert!(app.workflows[0].autostart);
        assert_eq!(app.pending_workflow_start.as_deref(), Some("wf"));
        let cmd = rx.try_recv().expect("command sent");
        assert!(matches!(cmd, DaemonCommand::EnableWorkflow { name } if name == "wf"));
    }

    #[test]
    fn toggle_enable_selected_disables_workflow() {
        // GIVEN an enabled workflow
        let (tx, mut rx) = mpsc::channel(32);
        let mut app = App::new(tx);
        app.workflows.push(WorkflowEntry {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
            trigger: None,
            toml_content: None,
            autostart: true,
        });
        app.cursor = CursorTarget::Workflow(0);

        // WHEN toggled
        app.toggle_enable_selected();

        // THEN enabled flips to false, DisableWorkflow sent
        assert!(!app.workflows[0].autostart);
        let cmd = rx.try_recv().expect("command sent");
        assert!(matches!(cmd, DaemonCommand::DisableWorkflow { name } if name == "wf"));
    }

    #[test]
    fn snapshot_stores_enabled_flag() {
        // GIVEN an app with no workflows
        let mut app = make_test_app();

        // WHEN a snapshot arrives with enabled=true
        app.handle_daemon_event(DaemonEvent::WorkflowsSnapshot(vec![snap("wf", None, true)]));

        // THEN the entry has autostart=true
        assert!(app.workflows[0].autostart);
    }

    #[test]
    fn toggle_expanded_is_noop_on_workflow_without_runs() {
        // GIVEN a workflow with no runs
        let mut app = make_test_app();
        app.workflows.push(WorkflowEntry {
            name: "empty".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Dormant,
            runs: vec![],
            expanded: false,
            trigger: None,
            toml_content: None,
            autostart: false,
        });
        app.cursor = CursorTarget::Workflow(0);

        // WHEN toggle_expanded is called
        app.toggle_expanded();

        // THEN expanded stays false
        assert!(!app.workflows[0].expanded);
    }
}
