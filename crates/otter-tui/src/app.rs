use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use chrono::Utc;
use otter_core::types::{
    CheckpointAction, DaemonCommand, DaemonEvent, LogEntry, MarketplaceOrigin, MarketplaceStatus,
    MarketplaceWorkflowEntry, ProgressChunk, TriggerDef, WorkflowRun, WorkflowState,
    WorkflowStatus, WorkflowType,
};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::first_launch::FirstLaunchState;
use crate::help_modal::HelpModal;

#[derive(Debug, PartialEq)]
pub enum Mode {
    Normal,
    FeedbackInput,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Modal {
    Help,
}

impl Modal {
    /// Stable identifier used by [`FirstLaunchState`] to remember whether
    /// the user has already seen this modal on a prior launch.
    pub fn first_launch_id(&self) -> &'static str {
        match self {
            Modal::Help => HelpModal::FIRST_LAUNCH_ID,
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Focus {
    Left,
    Right,
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
    pub update_available: Option<String>,
    pub origin: Option<MarketplaceOrigin>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum CursorTarget {
    Workflow(usize),                   // index into workflows vec
    Run(usize, usize),                 // workflow_idx, run_idx
    Marketplace(usize),                // index into marketplaces vec
    MarketplaceWorkflow(usize, usize), // marketplace_idx, workflow_idx
}

/// Typed view of what the cursor currently points at.
pub enum Selection<'a> {
    Workflow(&'a WorkflowEntry),
    Run(&'a WorkflowEntry, &'a WorkflowRun),
    Marketplace(&'a MarketplaceStatus),
    MarketplaceWorkflow(&'a MarketplaceStatus, &'a MarketplaceWorkflowEntry),
    None,
}

pub struct Ui {
    pub cursor: CursorTarget,
    pub focus: Focus,
    pub mode: Mode,
    pub modal: Option<Modal>,
    pub feedback_input: String,
    pub marketplace_expanded: HashMap<String, bool>,
    /// Modals queued to be shown automatically on TUI launch — one at a
    /// time, popped from the front when the current modal is dismissed.
    /// New entries can be added by [`Ui::queue_first_launch_modal`].
    pub first_launch_queue: VecDeque<Modal>,
    /// Persisted record of which one-time modals the user has already seen.
    pub first_launch: FirstLaunchState,
    /// Name of workflow we just started; when a new run appears for it, auto-expand.
    /// View intent, not a domain fact — lives here so daemon events can consult it.
    pub pending_workflow_start: Option<String>,
    pub tick: u64,
}

impl Ui {
    fn new(config_dir: &Path) -> Self {
        let first_launch = FirstLaunchState::load(config_dir);
        let mut ui = Self {
            cursor: CursorTarget::Workflow(0),
            focus: Focus::Left,
            mode: Mode::Normal,
            modal: None,
            feedback_input: String::new(),
            marketplace_expanded: HashMap::new(),
            first_launch_queue: VecDeque::new(),
            first_launch,
            pending_workflow_start: None,
            tick: 0,
        };

        // Register one-time modals here. Order is the order they will be
        // shown to the user (one at a time, advancing on dismissal).
        ui.queue_first_launch_modal(Modal::Help);

        // Pop the first modal so something is visible on the initial draw.
        ui.modal = ui.first_launch_queue.pop_front();
        ui
    }

    /// Push `modal` onto the first-launch queue if the user hasn't already
    /// seen it. Marks the id as seen immediately so subsequent launches
    /// don't replay it, even if this session never displays the modal.
    pub fn queue_first_launch_modal(&mut self, modal: Modal) {
        let id = modal.first_launch_id();
        if self.first_launch.has_seen(id) {
            return;
        }
        self.first_launch.mark_seen(id);
        self.first_launch_queue.push_back(modal);
    }

    /// Close the current modal and advance to the next first-launch modal,
    /// if any. Called by the input handler when the user dismisses a modal.
    pub fn dismiss_modal(&mut self) {
        self.modal = self.first_launch_queue.pop_front();
    }

    pub fn is_marketplace_expanded(&self, name: &str) -> bool {
        self.marketplace_expanded
            .get(name)
            .copied()
            .unwrap_or(false)
    }
}

pub struct App {
    // Daemon-derived data
    pub workflows: Vec<WorkflowEntry>,
    pub marketplaces: Vec<MarketplaceStatus>,
    pub logs: HashMap<Uuid, Vec<LogEntry>>,
    pub pending_checkpoints: HashMap<Uuid, PendingCheckpoint>,
    pub consumed_triggers: HashMap<String, Vec<String>>,
    pub progress: HashMap<Uuid, Vec<(usize, ProgressChunk)>>,
    pub update_available: Option<String>,

    // View state
    pub ui: Ui,

    // Lifecycle / I/O / config
    pub should_quit: bool,
    pub cmd_tx: mpsc::Sender<DaemonCommand>,
    /// Filesystem root for resolving marketplace clones when previewing.
    pub data_dir: PathBuf,
    /// Configuration root (`~/.config/otter/`); used to locate installed
    /// workflow packages for the rich preview.
    pub config_dir: PathBuf,
}

impl App {
    pub fn new(
        cmd_tx: mpsc::Sender<DaemonCommand>,
        data_dir: PathBuf,
        config_dir: PathBuf,
    ) -> Self {
        let ui = Ui::new(&config_dir);
        Self {
            workflows: Vec::new(),
            marketplaces: Vec::new(),
            logs: HashMap::new(),
            pending_checkpoints: HashMap::new(),
            consumed_triggers: HashMap::new(),
            progress: HashMap::new(),
            update_available: None,
            ui,
            should_quit: false,
            cmd_tx,
            data_dir,
            config_dir,
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

    pub fn selection(&self) -> Selection<'_> {
        match self.ui.cursor {
            CursorTarget::Workflow(wi) => self
                .workflows
                .get(wi)
                .map(Selection::Workflow)
                .unwrap_or(Selection::None),
            CursorTarget::Run(wi, ri) => self
                .workflows
                .get(wi)
                .and_then(|e| e.runs.get(ri).map(|r| Selection::Run(e, r)))
                .unwrap_or(Selection::None),
            CursorTarget::Marketplace(mi) => self
                .marketplaces
                .get(mi)
                .map(Selection::Marketplace)
                .unwrap_or(Selection::None),
            CursorTarget::MarketplaceWorkflow(mi, wi) => self
                .marketplaces
                .get(mi)
                .and_then(|m| {
                    m.workflows
                        .get(wi)
                        .map(|w| Selection::MarketplaceWorkflow(m, w))
                })
                .unwrap_or(Selection::None),
        }
    }

    pub fn selected_workflow(&self) -> Option<&WorkflowEntry> {
        match self.selection() {
            Selection::Workflow(e) | Selection::Run(e, _) => Some(e),
            Selection::Marketplace(_) | Selection::MarketplaceWorkflow(_, _) | Selection::None => {
                None
            }
        }
    }

    /// Returns true if a workflow with `name` is already installed locally.
    pub fn is_workflow_installed(&self, name: &str) -> bool {
        self.workflows.iter().any(|w| w.name == name)
    }

    /// When the named workflow is installed AND its origin marketplace
    /// advertises a newer version, returns that latest version.
    pub fn workflow_update_available(&self, name: &str) -> Option<&str> {
        self.workflows
            .iter()
            .find(|w| w.name == name)
            .and_then(|w| w.update_available.as_deref())
    }

    pub fn handle_daemon_event(&mut self, event: DaemonEvent, panels: &mut crate::panel::PanelSet) {
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
                        if self.ui.pending_workflow_start.as_ref() == Some(&entry.name) {
                            entry.expanded = true;
                            self.ui.pending_workflow_start = None;
                            self.ui.cursor = CursorTarget::Run(wi, 0);
                        }
                    }
                }
            }
            DaemonEvent::LogAppended(entry) => {
                self.logs.entry(entry.run_id).or_default().push(entry);
            }
            DaemonEvent::WorkflowsSnapshot(snapshot) => self.apply_workflows_snapshot(snapshot),
            DaemonEvent::MarketplacesSnapshot(snapshot) => {
                self.apply_marketplaces_snapshot(snapshot)
            }
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
                    .position(|t| *t == self.ui.cursor);
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
                let len = self.selected_consumed_triggers().len();
                panels.right.clamp_consumed_cursor(len);
            }
            DaemonEvent::UpdateAvailable { latest, .. } => {
                self.update_available = Some(latest);
            }
        }
    }

    fn apply_workflows_snapshot(&mut self, snapshot: Vec<WorkflowStatus>) {
        let old_pos = self
            .build_flat_list()
            .iter()
            .position(|t| *t == self.ui.cursor);
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
                    entry.update_available = incoming.update_available;
                    entry.origin = incoming.origin;
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
                        update_available: incoming.update_available,
                        origin: incoming.origin,
                    });
                }
            }
        }

        self.ensure_cursor_valid(old_pos);
    }

    pub(crate) fn apply_marketplaces_snapshot(&mut self, snapshot: Vec<MarketplaceStatus>) {
        let old_pos = self
            .build_flat_list()
            .iter()
            .position(|t| *t == self.ui.cursor);
        // Drop expand state for marketplaces that disappeared.
        let names: std::collections::HashSet<&str> =
            snapshot.iter().map(|m| m.name.as_str()).collect();
        self.ui
            .marketplace_expanded
            .retain(|k, _| names.contains(k.as_str()));
        self.marketplaces = snapshot;
        self.ensure_cursor_valid(old_pos);
    }

    fn ensure_cursor_valid(&mut self, preferred_pos: Option<usize>) {
        let flat = self.build_flat_list();
        if flat.is_empty() {
            self.ui.cursor = CursorTarget::Workflow(0);
            return;
        }
        if !flat.contains(&self.ui.cursor) {
            let target = preferred_pos
                .map(|p| p.saturating_sub(1).min(flat.len() - 1))
                .unwrap_or(flat.len() - 1);
            self.ui.cursor = flat[target];
        }
    }

    pub fn start_selected(&mut self) {
        if let Some(entry) = self.selected_workflow() {
            let name = entry.name.clone();
            self.ui.pending_workflow_start = Some(name.clone());
            let _ = self.cmd_tx.try_send(DaemonCommand::Start { name });
        }
    }

    pub fn stop_selected_run(&mut self) {
        if let Some(run_id) = self.selected_run_id() {
            let _ = self.cmd_tx.try_send(DaemonCommand::StopRun { run_id });
        }
    }

    pub fn stop_selected(&mut self) {
        if let Some(entry) = self.selected_workflow() {
            let name = entry.name.clone();
            let _ = self.cmd_tx.try_send(DaemonCommand::Stop { name });
        }
    }

    pub fn toggle_enable_selected(&mut self) {
        // Only the workflow row itself toggles auto-start; a Run cursor would
        // otherwise resolve back to the parent workflow via selected_workflow()
        // and silently flip it from a run row, which is not what the keybinding
        // means.
        let CursorTarget::Workflow(wi) = self.ui.cursor else {
            return;
        };
        let Some(entry) = self.workflows.get_mut(wi) else {
            return;
        };
        let name = entry.name.clone();
        if entry.autostart {
            entry.autostart = false;
            let _ = self
                .cmd_tx
                .try_send(DaemonCommand::DisableWorkflow { name });
        } else {
            entry.autostart = true;
            self.ui.pending_workflow_start = Some(name.clone());
            let _ = self.cmd_tx.try_send(DaemonCommand::EnableWorkflow { name });
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
        match self.selection() {
            Selection::Run(_, r) => Some(r.id),
            _ => None,
        }
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

    /// Returns the on-disk package directory for the currently-selected
    /// installed workflow, if any. Used by the rich preview to surface README
    /// and inlined message-file contents.
    pub fn selected_workflow_pkg_dir(&self) -> Option<PathBuf> {
        let entry = match self.selection() {
            Selection::Workflow(e) => e,
            _ => return None,
        };
        let dir = self.config_dir.join("workflows").join(&entry.name);
        if dir.is_dir() {
            Some(dir)
        } else {
            None
        }
    }

    /// Build a flat list of all cursor targets in navigation order
    pub(crate) fn build_flat_list(&self) -> Vec<CursorTarget> {
        let mut list = Vec::new();
        for (wi, entry) in self.workflows.iter().enumerate() {
            list.push(CursorTarget::Workflow(wi));
            if entry.expanded {
                for (ri, _) in entry.runs.iter().enumerate() {
                    list.push(CursorTarget::Run(wi, ri));
                }
            }
        }
        for (mi, m) in self.marketplaces.iter().enumerate() {
            list.push(CursorTarget::Marketplace(mi));
            if self.ui.is_marketplace_expanded(&m.name) {
                for (wi, w) in m.workflows.iter().enumerate() {
                    if !crate::marketplaces_panel::workflow_is_visible(self, w) {
                        continue;
                    }
                    list.push(CursorTarget::MarketplaceWorkflow(mi, wi));
                }
            }
        }
        list
    }

    /// Navigate up one position in the unified flat list (wraps to bottom).
    /// Returns true if the cursor moved.
    pub fn move_cursor_up(&mut self) -> bool {
        self.ui.focus = Focus::Left;
        let flat = self.build_flat_list();
        if flat.is_empty() {
            return false;
        }
        if let Some(current_pos) = flat.iter().position(|t| *t == self.ui.cursor) {
            self.ui.cursor = flat[(current_pos + flat.len() - 1) % flat.len()];
            true
        } else {
            false
        }
    }

    /// Navigate down one position in the unified flat list (wraps to top).
    /// Returns true if the cursor moved.
    pub fn move_cursor_down(&mut self) -> bool {
        self.ui.focus = Focus::Left;
        let flat = self.build_flat_list();
        if flat.is_empty() {
            return false;
        }
        if let Some(current_pos) = flat.iter().position(|t| *t == self.ui.cursor) {
            self.ui.cursor = flat[(current_pos + 1) % flat.len()];
            true
        } else {
            false
        }
    }

    /// Toggle expanded state of the workflow or marketplace at the current cursor
    pub fn toggle_expanded(&mut self) {
        match self.ui.cursor {
            CursorTarget::Workflow(wi) => {
                if let Some(entry) = self.workflows.get_mut(wi) {
                    if entry.runs.is_empty() {
                        return;
                    }
                    entry.expanded = !entry.expanded;
                    // Collapse: snap cursor to the workflow row
                    if !entry.expanded {
                        self.ui.cursor = CursorTarget::Workflow(wi);
                    }
                }
            }
            CursorTarget::Marketplace(mi) => {
                if let Some(m) = self.marketplaces.get(mi) {
                    if m.workflows.is_empty() {
                        return;
                    }
                    let name = m.name.clone();
                    let new_state = !self.ui.is_marketplace_expanded(&name);
                    self.ui.marketplace_expanded.insert(name, new_state);
                    if !new_state {
                        self.ui.cursor = CursorTarget::Marketplace(mi);
                    }
                }
            }
            _ => {}
        }
    }

    pub fn cursor_is_polling_workflow(&self) -> bool {
        matches!(
            self.selected_workflow().and_then(|e| e.trigger.as_ref()),
            Some(TriggerDef::Polling { .. })
        )
    }

    /// Open the consumed-triggers right-panel view for the currently
    /// selected polling workflow. Returns true if the request was
    /// dispatched (caller is responsible for switching the right panel into
    /// consumed-triggers mode via [`crate::right_panel::RightPanel::show_consumed_triggers`]).
    pub fn open_consumed_triggers(&mut self) -> bool {
        if !self.cursor_is_polling_workflow() {
            return false;
        }
        if let Some(name) = self.selected_workflow().map(|e| e.name.clone()) {
            self.consumed_triggers.entry(name.clone()).or_default();
            self.ui.focus = Focus::Right;
            let _ = self
                .cmd_tx
                .try_send(DaemonCommand::ListConsumedTriggers { workflow: name });
            true
        } else {
            false
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

    pub fn delete_selected_consumed_trigger(&mut self, right: &mut crate::right_panel::RightPanel) {
        let workflow = match self.selected_workflow().map(|e| e.name.clone()) {
            Some(n) => n,
            None => return,
        };
        let cursor = right.cursor;
        let triggers = match self.consumed_triggers.get_mut(&workflow) {
            Some(t) => t,
            None => return,
        };
        if cursor >= triggers.len() {
            return;
        }
        let trigger = triggers.remove(cursor);
        let new_len = triggers.len();
        right.clamp_consumed_cursor(new_len);
        let _ = self
            .cmd_tx
            .try_send(DaemonCommand::DeleteConsumedTrigger { workflow, trigger });
    }

    /// Returns the on-disk package directory for the selected marketplace
    /// workflow, if the cursor is on one and the clone exists.
    pub fn selected_marketplace_pkg_dir(&self) -> Option<PathBuf> {
        let Selection::MarketplaceWorkflow(m, w) = self.selection() else {
            return None;
        };
        let dir = self
            .data_dir
            .join("marketplaces")
            .join(&m.name)
            .join(&w.path);
        if dir.is_dir() {
            Some(dir)
        } else {
            None
        }
    }

    /// Delete the currently selected run (only if cursor is on a run)
    pub fn delete_selected_run(&mut self) {
        if let Some(run_id) = self.selected_run_id() {
            let _ = self.cmd_tx.try_send(DaemonCommand::DeleteRun { run_id });
        }
    }
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod app_tests;
