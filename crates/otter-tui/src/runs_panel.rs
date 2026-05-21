use chrono::Local;
use otter_core::types::{RunStatus, TriggerDef, WorkflowState, WorkflowType};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState},
    Frame,
};

use crate::app::{App, CursorTarget, Focus};
use crate::scroll::scroll_spans;
use crate::status_bar::PanelHint;
use crate::styles::{panel, panel_focused, spinner_frame};
use crate::theme;

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

pub fn render_runs(f: &mut Frame, app: &App, area: Rect) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let tick = app.tick;

    let make_item =
        |prefix: &str, content: &str, icon: String, icon_color: Color, is_selected: bool| {
            let prefix_len = prefix.chars().count();
            let icon_len = icon.chars().count();
            let available_for_content = inner_width.saturating_sub(prefix_len + icon_len);

            let content_style = if is_selected {
                Style::default()
                    .fg(theme::current().background)
                    .bg(theme::current().foreground)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme::current().foreground)
                    .bg(theme::current().background)
            };

            let scrolled = scroll_spans(
                vec![Span::styled(content.to_string(), content_style)],
                available_for_content,
                tick,
            );
            let displayed_len: usize = scrolled.iter().map(|s| s.content.chars().count()).sum();
            let padding = " ".repeat(available_for_content.saturating_sub(displayed_len));

            let mut spans = vec![Span::styled(
                prefix.to_string(),
                Style::default()
                    .fg(theme::current().foreground)
                    .bg(theme::current().background),
            )];
            spans.extend(scrolled);
            spans.push(Span::styled(
                padding,
                Style::default()
                    .fg(theme::current().foreground)
                    .bg(theme::current().background),
            ));
            spans.push(Span::styled(
                icon,
                Style::default()
                    .fg(icon_color)
                    .bg(theme::current().background),
            ));
            ListItem::new(Line::from(spans))
        };

    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_index: Option<usize> = None;

    for (wi, entry) in app.workflows.iter().enumerate() {
        let current_index = items.len();

        let is_workflow_selected = app.cursor == CursorTarget::Workflow(wi);
        if is_workflow_selected {
            selected_index = Some(current_index);
        }

        let expand_char = if !entry.runs.is_empty() {
            if entry.expanded {
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
            app.tick,
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

        if entry.expanded {
            for (ri, run) in entry.runs.iter().enumerate() {
                let is_run_selected = app.cursor == CursorTarget::Run(wi, ri);
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
                    app.tick,
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

    let block = if app.modal.is_none() && app.focus == Focus::Left {
        panel_focused("Workflows")
    } else {
        panel("Workflows")
    };
    let list = List::new(items).block(block);

    f.render_stateful_widget(list, area, &mut state);
}

/// Returns the keybinding hints this panel contributes to the status bar.
pub fn left_panel_hints(app: &App) -> Vec<PanelHint> {
    let mut hints: Vec<PanelHint> = vec![];

    if let CursorTarget::Workflow(wi) = app.cursor {
        if let Some(entry) = app.workflows.get(wi) {
            if entry.expanded && !entry.runs.is_empty() {
                hints.push(PanelHint::new("[Space]", "Hide runs"));
            } else if !entry.runs.is_empty() {
                hints.push(PanelHint::new("[Space]", "Show runs"));
            }
        }
    }

    let mut enter_hints: Vec<PanelHint> = vec![];
    if let CursorTarget::Run(wi, ri) = app.cursor {
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
    } else if let Some(state) = app.selected_workflow_state() {
        match state {
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

    if let CursorTarget::Workflow(wi) = app.cursor {
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
    use tokio::sync::mpsc;

    fn make_app() -> App {
        let (tx, _rx) = mpsc::channel(32);
        App::new(tx)
    }

    fn make_entry(
        name: &str,
        kind: WorkflowType,
        state: WorkflowState,
        runs: Vec<WorkflowRun>,
        expanded: bool,
    ) -> WorkflowEntry {
        WorkflowEntry {
            name: name.to_string(),
            kind,
            state,
            runs,
            expanded,
            trigger: None,
            toml_content: None,
            autostart: false,
        }
    }

    fn hint_keys(hints: &[PanelHint]) -> Vec<&str> {
        hints.iter().map(|h| h.key).collect()
    }

    #[test]
    fn left_panel_hints_empty_workflow_has_no_space_hint() {
        // GIVEN a workflow with no runs
        let mut app = make_app();
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![],
            false,
        ));
        app.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app);

        // THEN no [Space] hint
        assert!(!hint_keys(&hints).contains(&"[Space]"));
    }

    #[test]
    fn left_panel_hints_collapsed_workflow_with_runs_shows_show_hint() {
        // GIVEN a collapsed workflow with runs
        let mut app = make_app();
        let run = WorkflowRun::new("wf".to_string());
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![run],
            false,
        ));
        app.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app);

        // THEN [Space] Show runs is present
        let space = hints.iter().find(|h| h.key == "[Space]");
        assert!(space.is_some());
        assert_eq!(space.unwrap().label, "Show runs");
    }

    #[test]
    fn left_panel_hints_expanded_workflow_shows_hide_hint() {
        // GIVEN an expanded workflow
        let mut app = make_app();
        let run = WorkflowRun::new("wf".to_string());
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![run],
            true,
        ));
        app.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app);

        // THEN [Space] Hide runs is present
        let space = hints.iter().find(|h| h.key == "[Space]");
        assert!(space.is_some());
        assert_eq!(space.unwrap().label, "Hide runs");
    }

    #[test]
    fn left_panel_hints_expanded_workflow_no_runs_shows_no_space_hint() {
        // GIVEN an expanded workflow with no runs
        let mut app = make_app();
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![],
            true,
        ));
        app.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app);

        // THEN no [Space] hint
        assert!(!hint_keys(&hints).contains(&"[Space]"));
    }

    #[test]
    fn left_panel_hints_run_cursor_shows_delete_hint() {
        // GIVEN cursor on a run
        let mut app = make_app();
        let run = WorkflowRun::new("wf".to_string());
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![run],
            true,
        ));
        app.cursor = CursorTarget::Run(0, 0);

        // WHEN
        let hints = left_panel_hints(&app);

        // THEN [Del] Delete run hint present
        assert!(hint_keys(&hints).contains(&"[Del]"));
    }

    #[test]
    fn left_panel_hints_dormant_workflow_shows_start() {
        // GIVEN a dormant workflow
        let mut app = make_app();
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![],
            false,
        ));
        app.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app);

        // THEN [Enter] Start
        let enter = hints.iter().find(|h| h.key == "[Enter]");
        assert!(enter.is_some());
        assert_eq!(enter.unwrap().label, "Start");
    }

    #[test]
    fn left_panel_hints_running_shows_stop() {
        // GIVEN a running workflow
        let mut app = make_app();
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Running,
            vec![],
            false,
        ));
        app.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app);
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
        let mut run = WorkflowRun::new("wf".to_string());
        run.status = RunStatus::Running;
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Running,
            vec![run],
            true,
        ));
        app.cursor = CursorTarget::Run(0, 0);

        // WHEN
        let hints = left_panel_hints(&app);
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
        let mut entry = make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![],
            false,
        );
        entry.autostart = false;
        app.workflows.push(entry);
        app.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app);

        // THEN [A] Enable auto-start
        let e_hint = hints.iter().find(|h| h.key == "[A]");
        assert!(e_hint.is_some());
        assert_eq!(e_hint.unwrap().label, "Enable auto-start");
    }

    #[test]
    fn left_panel_hints_shows_disable_hint_for_enabled_workflow() {
        // GIVEN an enabled workflow
        let mut app = make_app();
        let mut entry = make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![],
            false,
        );
        entry.autostart = true;
        app.workflows.push(entry);
        app.cursor = CursorTarget::Workflow(0);

        // WHEN
        let hints = left_panel_hints(&app);

        // THEN [A] Disable auto-start
        let e_hint = hints.iter().find(|h| h.key == "[A]");
        assert!(e_hint.is_some());
        assert_eq!(e_hint.unwrap().label, "Disable auto-start");
    }

    #[test]
    fn left_panel_hints_completed_run_shows_only_delete() {
        // GIVEN cursor on a completed run
        let mut app = make_app();
        let mut run = WorkflowRun::new("wf".to_string());
        run.status = RunStatus::Completed;
        app.workflows.push(make_entry(
            "wf",
            WorkflowType::Looping,
            WorkflowState::Dormant,
            vec![run],
            true,
        ));
        app.cursor = CursorTarget::Run(0, 0);

        // WHEN
        let hints = left_panel_hints(&app);
        let keys = hint_keys(&hints);

        // THEN [Del] Delete run, no [Enter] Stop run
        assert!(!hints
            .iter()
            .any(|h| h.key == "[Enter]" && h.label == "Stop run"));
        assert!(keys.contains(&"[Del]"));
    }
}
