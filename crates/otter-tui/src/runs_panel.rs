use std::collections::HashSet;

use chrono::Local;
use crossterm::event::{KeyCode, KeyEvent};
use otter_core::types::{RunStatus, TriggerDef, WorkflowState, WorkflowType};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{List, ListItem, ListState},
    Frame,
};

use crate::app::{App, CursorTarget, Selection};
use crate::list_row::list_row;
use crate::panel::Panel;
use crate::status_bar::PanelHint;
use crate::styles::{base_style, spinner_frame};
use crate::theme;

#[derive(Default)]
pub struct RunsPanel {
    expanded: HashSet<String>,
    pending_start: Option<String>,
}

impl RunsPanel {
    pub fn is_expanded(&self, name: &str) -> bool {
        self.expanded.contains(name)
    }

    pub fn toggle(&mut self, name: &str) -> bool {
        if self.expanded.remove(name) {
            false
        } else {
            self.expanded.insert(name.to_string());
            true
        }
    }

    pub fn retain<F: Fn(&str) -> bool>(&mut self, keep: F) {
        self.expanded.retain(|k| keep(k));
    }

    /// Called when a new run is inserted for `workflow_name`. If we just
    /// started this workflow ourselves, expand it; returns true when an
    /// auto-expand happened so the caller can move the cursor to the new run.
    pub fn on_run_added(&mut self, workflow_name: &str) -> bool {
        if self.pending_start.as_deref() == Some(workflow_name) {
            self.expanded.insert(workflow_name.to_string());
            self.pending_start = None;
            true
        } else {
            false
        }
    }

    /// Toggle `autostart` on the currently-selected workflow row. Returns the
    /// new autostart value, or `None` when the cursor isn't on a workflow row.
    pub fn toggle_autostart_for_selected(&mut self, app: &mut App) -> Option<bool> {
        use otter_core::types::DaemonCommand;

        let CursorTarget::Workflow(wi) = app.ui.cursor else {
            return None;
        };
        let entry = app.workflows.get_mut(wi)?;
        let name = entry.name.clone();
        entry.autostart = !entry.autostart;
        let autostart = entry.autostart;
        if autostart {
            self.pending_start = Some(name.clone());
            let _ = app.cmd_tx.try_send(DaemonCommand::EnableWorkflow { name });
        } else {
            let _ = app.cmd_tx.try_send(DaemonCommand::DisableWorkflow { name });
        }
        Some(autostart)
    }

    /// Start the currently-selected workflow (Enter on a Dormant workflow row).
    pub fn start_selected(&mut self, app: &mut App) {
        use otter_core::types::DaemonCommand;

        let Some(entry) = app.selected_workflow() else {
            return;
        };
        let name = entry.name.clone();
        self.pending_start = Some(name.clone());
        let _ = app.cmd_tx.try_send(DaemonCommand::Start { name });
    }
}

fn workflow_state_color(
    state: &WorkflowState,
    run_status: Option<&RunStatus>,
    kind: Option<&WorkflowType>,
    trigger: Option<&TriggerDef>,
    tick: u64,
) -> (String, Color) {
    match state {
        WorkflowState::Dormant => ("·".to_string(), theme::current().dormant),
        WorkflowState::Running => match run_status {
            Some(RunStatus::WaitingCheckpoint) => ("~".to_string(), theme::current().waiting_cp),
            // Triggered workflow: engine alive but between trigger events (last run may have failed)
            Some(RunStatus::Completed)
            | Some(RunStatus::Failed)
            | Some(RunStatus::Stopped)
            | None
                if matches!(kind, Some(WorkflowType::Triggered)) =>
            {
                match trigger {
                    Some(TriggerDef::Polling { .. }) => ("⏲".to_string(), theme::current().dormant),
                    _ => ("·".to_string(), theme::current().dormant),
                }
            }
            Some(RunStatus::Failed) | Some(RunStatus::Stopped) => {
                ("✗".to_string(), theme::current().failed)
            }
            Some(RunStatus::Completed) => ("✓".to_string(), theme::current().completed),
            _ => (spinner_frame(tick).to_string(), theme::current().running),
        },
    }
}

impl Panel for RunsPanel {
    fn render(&mut self, f: &mut Frame, app: &App, area: Rect, _focused: bool) {
        render_runs(f, app, self, area);
    }

    fn handle_key(&mut self, app: &mut App, key: KeyEvent) -> bool {
        use otter_core::types::{DaemonCommand, RunStatus};

        match key.code {
            KeyCode::Char(' ') => {
                let CursorTarget::Workflow(wi) = app.ui.cursor else {
                    return true;
                };
                let Some(entry) = app.workflows.get(wi) else {
                    return true;
                };
                if entry.runs.is_empty() {
                    return true;
                }
                if !self.toggle(&entry.name) {
                    // Collapsed: snap cursor back to the workflow row.
                    app.ui.cursor = CursorTarget::Workflow(wi);
                }
                true
            }
            KeyCode::Delete => {
                app.delete_selected_run();
                true
            }
            KeyCode::Char('a') => {
                self.toggle_autostart_for_selected(app);
                true
            }
            KeyCode::Enter => {
                enum Verdict {
                    StopRun,
                    StartWorkflow,
                    StopWorkflow(String),
                    Noop,
                }
                let verdict = match app.selection() {
                    Selection::Run(_, r)
                        if matches!(
                            r.status,
                            RunStatus::Running | RunStatus::WaitingCheckpoint
                        ) =>
                    {
                        Verdict::StopRun
                    }
                    Selection::Workflow(e) => match e.state {
                        WorkflowState::Dormant => Verdict::StartWorkflow,
                        WorkflowState::Running => Verdict::StopWorkflow(e.name.clone()),
                    },
                    _ => Verdict::Noop,
                };
                match verdict {
                    Verdict::StopRun => app.stop_selected_run(),
                    Verdict::StartWorkflow => self.start_selected(app),
                    Verdict::StopWorkflow(name) => {
                        let _ = app.cmd_tx.try_send(DaemonCommand::Stop { name });
                    }
                    Verdict::Noop => {}
                }
                true
            }
            _ => false,
        }
    }

    fn hints(&self, app: &App) -> Vec<PanelHint> {
        left_panel_hints(app, self)
    }
}

pub(crate) fn render_runs(f: &mut Frame, app: &App, panel: &RunsPanel, area: Rect) {
    let inner_width = area.width as usize;
    let tick = app.ui.tick;

    let make_item =
        |prefix: &str, content: &str, icon: String, icon_color: Color, is_selected: bool| {
            let trailing = [(
                icon,
                Style::default()
                    .fg(icon_color)
                    .bg(theme::current().background),
            )];
            list_row(
                prefix,
                content,
                &trailing,
                base_style(),
                is_selected,
                inner_width,
                tick,
            )
        };

    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_index: Option<usize> = None;

    for (wi, entry) in app.workflows.iter().enumerate() {
        let current_index = items.len();

        let is_workflow_selected = app.ui.cursor == CursorTarget::Workflow(wi);
        if is_workflow_selected {
            selected_index = Some(current_index);
        }

        let expand_char = if !entry.runs.is_empty() {
            if panel.is_expanded(&entry.name) {
                "▼ "
            } else {
                "▶ "
            }
        } else {
            "  "
        };
        let running = RunStatus::Running;
        let first_run_status = entry.runs.first().map(|r| {
            if app
                .pending_checkpoints
                .get(&r.id)
                .is_some_and(|cp| cp.processing)
            {
                &running
            } else {
                &r.status
            }
        });
        let (state_icon, state_color) = workflow_state_color(
            &entry.state,
            first_run_status,
            Some(&entry.kind),
            entry.trigger.as_ref(),
            app.ui.tick,
        );
        let name_content = entry.name.clone();
        let prefixed_icon = if entry.autostart {
            format!(" (A) {state_icon}")
        } else {
            format!(" {state_icon}")
        };
        items.push(make_item(
            expand_char,
            &name_content,
            prefixed_icon,
            state_color,
            is_workflow_selected,
        ));

        if panel.is_expanded(&entry.name) {
            for (ri, run) in entry.runs.iter().enumerate() {
                let is_run_selected = app.ui.cursor == CursorTarget::Run(wi, ri);
                if is_run_selected {
                    selected_index = Some(items.len());
                }

                let datetime = run
                    .started_at
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string();
                let run_content = match run.trigger_payload.as_deref() {
                    Some(p) if !p.is_empty() => format!("{} ({})", p, datetime),
                    _ => datetime,
                };
                let running = RunStatus::Running;
                let effective_run_status = if app
                    .pending_checkpoints
                    .get(&run.id)
                    .is_some_and(|cp| cp.processing)
                {
                    &running
                } else {
                    &run.status
                };
                let (run_icon, run_color) = workflow_state_color(
                    &WorkflowState::Running,
                    Some(effective_run_status),
                    None,
                    None,
                    app.ui.tick,
                );
                items.push(make_item(
                    "   ",
                    &run_content,
                    format!(" {run_icon}"),
                    run_color,
                    is_run_selected,
                ));
            }
        }
    }

    let mut state = ListState::default();
    if let Some(idx) = selected_index {
        state.select(Some(idx));
    }

    let list = List::new(items);
    f.render_stateful_widget(list, area, &mut state);
}

/// Returns the keybinding hints this panel contributes to the status bar.
pub fn left_panel_hints(app: &App, panel: &RunsPanel) -> Vec<PanelHint> {
    let mut hints: Vec<PanelHint> = vec![];

    if let CursorTarget::Workflow(wi) = app.ui.cursor {
        if let Some(entry) = app.workflows.get(wi) {
            let expanded = panel.is_expanded(&entry.name);
            if expanded && !entry.runs.is_empty() {
                hints.push(PanelHint::new("[Space]", "Hide runs"));
            } else if !entry.runs.is_empty() {
                hints.push(PanelHint::new("[Space]", "Show runs"));
            }
        }
    }

    let mut enter_hints: Vec<PanelHint> = vec![];
    if let CursorTarget::Run(wi, ri) = app.ui.cursor {
        let run_status = app
            .workflows
            .get(wi)
            .and_then(|e| e.runs.get(ri))
            .map(|r| &r.status);
        if matches!(
            run_status,
            Some(RunStatus::Running) | Some(RunStatus::WaitingCheckpoint)
        ) {
            enter_hints.push(PanelHint::new("[Enter]", "Stop"));
        }
        hints.push(PanelHint::new("[Del]", "Delete"));
    } else if let Some(entry) = app.selected_workflow() {
        match entry.state {
            WorkflowState::Dormant => {
                enter_hints.push(PanelHint::new("[Enter]", "Start"));
            }
            WorkflowState::Running => {
                enter_hints.push(PanelHint::new("[Enter]", "Stop"));
            }
        }
    }

    if app.cursor_is_polling_workflow() {
        hints.push(PanelHint::new("[T]", "Consumed triggers"));
    }

    if let CursorTarget::Workflow(wi) = app.ui.cursor {
        if let Some(entry) = app.workflows.get(wi) {
            if entry.autostart {
                hints.push(PanelHint::new("[A]", "Disable auto-start"));
            } else {
                hints.push(PanelHint::new("[A]", "Enable auto-start"));
            }
        }
    }

    hints.extend(enter_hints);
    hints
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::WorkflowEntry;
    use otter_core::types::{WorkflowRun, WorkflowState, WorkflowType};
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn make_app() -> App {
        let (tx, _rx) = mpsc::channel(32);
        App::new(
            tx,
            PathBuf::from("/tmp/otter-tui-test"),
            PathBuf::from("/tmp/otter-tui-test-config"),
        )
    }

    fn make_entry(
        name: &str,
        kind: WorkflowType,
        state: WorkflowState,
        runs: Vec<WorkflowRun>,
    ) -> WorkflowEntry {
        WorkflowEntry {
            name: name.to_string(),
            kind,
            state,
            runs,
            trigger: None,
            toml_content: None,
            autostart: false,
            update_available: None,
            origin: None,
        }
    }

    fn hint_keys(hints: &[PanelHint]) -> Vec<&str> {
        hints.iter().map(|h| h.key.as_ref()).collect()
    }

    #[test]
    fn left_panel_hints_empty_workflow_has_no_space_hint() {
        // GIVEN a workflow with no runs
        let mut app = make_app();
        let panel = RunsPanel::default();
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![],
        ));
        app.ui.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);

        // THEN no [Space] hint
        assert!(!hint_keys(&hints).contains(&"[Space]"));
    }

    #[test]
    fn left_panel_hints_collapsed_workflow_with_runs_shows_show_hint() {
        // GIVEN a collapsed workflow with runs
        let mut app = make_app();
        let panel = RunsPanel::default();
        let run = WorkflowRun::new("wf".to_string());
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![run],
        ));
        app.ui.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);

        // THEN [Space] Show runs is present
        let space = hints.iter().find(|h| h.key == "[Space]");
        assert!(space.is_some());
        assert_eq!(space.unwrap().label, "Show runs");
    }

    #[test]
    fn left_panel_hints_expanded_workflow_shows_hide_hint() {
        // GIVEN an expanded workflow
        let mut app = make_app();
        let mut panel = RunsPanel::default();
        let run = WorkflowRun::new("wf".to_string());
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![run],
        ));
        panel.expanded.insert("wf".into());
        app.ui.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);

        // THEN [Space] Hide runs is present
        let space = hints.iter().find(|h| h.key == "[Space]");
        assert!(space.is_some());
        assert_eq!(space.unwrap().label, "Hide runs");
    }

    #[test]
    fn left_panel_hints_expanded_workflow_no_runs_shows_no_space_hint() {
        // GIVEN an expanded workflow with no runs
        let mut app = make_app();
        let mut panel = RunsPanel::default();
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![],
        ));
        panel.expanded.insert("wf".into());
        app.ui.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);

        // THEN no [Space] hint
        assert!(!hint_keys(&hints).contains(&"[Space]"));
    }

    #[test]
    fn left_panel_hints_run_cursor_shows_delete_hint() {
        // GIVEN cursor on a run
        let mut app = make_app();
        let mut panel = RunsPanel::default();
        let run = WorkflowRun::new("wf".to_string());
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![run],
        ));
        panel.expanded.insert("wf".into());
        app.ui.cursor = CursorTarget::Run(0, 0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);

        // THEN [Del] Delete run hint present
        assert!(hint_keys(&hints).contains(&"[Del]"));
    }

    #[test]
    fn left_panel_hints_dormant_workflow_shows_start() {
        // GIVEN a dormant workflow
        let mut app = make_app();
        let panel = RunsPanel::default();
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![],
        ));
        app.ui.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);

        // THEN [Enter] Start
        let enter = hints.iter().find(|h| h.key == "[Enter]");
        assert!(enter.is_some());
        assert_eq!(enter.unwrap().label, "Start");
    }

    #[test]
    fn left_panel_hints_running_shows_stop() {
        // GIVEN a running workflow
        let mut app = make_app();
        let panel = RunsPanel::default();
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Running,
            vec![],
        ));
        app.ui.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);
        let keys = hint_keys(&hints);

        // THEN [Enter] Stop, no [p] pause
        assert!(!keys.contains(&"[p]"));
        assert!(hints
            .iter()
            .any(|h| h.key == "[Enter]" && h.label == "Stop"));
    }

    #[test]
    fn left_panel_hints_active_run_shows_stop_and_delete() {
        // GIVEN cursor on a running run
        let mut app = make_app();
        let mut panel = RunsPanel::default();
        let mut run = WorkflowRun::new("wf".to_string());
        run.status = RunStatus::Running;
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Running,
            vec![run],
        ));
        panel.expanded.insert("wf".into());
        app.ui.cursor = CursorTarget::Run(0, 0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);
        let keys = hint_keys(&hints);

        // THEN [Enter] Stop and [Del] Delete, exactly one Stop (no duplicate workflow-level hint)
        assert!(hints
            .iter()
            .any(|h| h.key == "[Enter]" && h.label == "Stop"));
        assert!(keys.contains(&"[Del]"));
        assert_eq!(
            hints
                .iter()
                .filter(|h| h.key == "[Enter]" && h.label == "Stop")
                .count(),
            1
        );
    }

    #[test]
    fn left_panel_hints_shows_enable_hint_for_disabled_workflow() {
        // GIVEN a disabled workflow
        let mut app = make_app();
        let panel = RunsPanel::default();
        let mut entry = make_entry("wf", WorkflowType::Looping, WorkflowState::Dormant, vec![]);
        entry.autostart = false;
        app.workflows.push(entry);
        app.ui.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);

        // THEN [A] Enable auto-start
        let e_hint = hints.iter().find(|h| h.key == "[A]");
        assert!(e_hint.is_some());
        assert_eq!(e_hint.unwrap().label, "Enable auto-start");
    }

    #[test]
    fn left_panel_hints_shows_disable_hint_for_enabled_workflow() {
        // GIVEN an enabled workflow
        let mut app = make_app();
        let panel = RunsPanel::default();
        let mut entry = make_entry("wf", WorkflowType::Looping, WorkflowState::Dormant, vec![]);
        entry.autostart = true;
        app.workflows.push(entry);
        app.ui.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);

        // THEN [A] Disable auto-start
        let e_hint = hints.iter().find(|h| h.key == "[A]");
        assert!(e_hint.is_some());
        assert_eq!(e_hint.unwrap().label, "Disable auto-start");
    }

    #[test]
    fn left_panel_hints_completed_run_shows_only_delete() {
        // GIVEN cursor on a completed run
        let mut app = make_app();
        let mut panel = RunsPanel::default();
        let mut run = WorkflowRun::new("wf".to_string());
        run.status = RunStatus::Completed;
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![run],
        ));
        panel.expanded.insert("wf".into());
        app.ui.cursor = CursorTarget::Run(0, 0);

        // WHEN
        let hints = left_panel_hints(&app, &panel);
        let keys = hint_keys(&hints);

        // THEN [Del] Delete run, no [Enter] Stop run
        assert!(!hints
            .iter()
            .any(|h| h.key == "[Enter]" && h.label == "Stop run"));
        assert!(keys.contains(&"[Del]"));
    }
}
