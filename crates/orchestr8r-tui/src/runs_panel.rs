use chrono::Local;
use orchestr8r_core::types::{RunStatus, TriggerDef, WorkflowType, WorkflowState};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState},
};

use crate::app::{App, CursorTarget};
use crate::scroll::scroll_text;
use crate::styles::{
    c_background, c_completed, c_dormant, c_failed, c_foreground, c_paused, c_running,
    c_waiting_cp, panel, spinner_frame,
};

fn workflow_state_color(
    state: &WorkflowState,
    run_status: Option<&RunStatus>,
    kind: Option<&WorkflowType>,
    trigger: Option<&TriggerDef>,
    tick: u64,
) -> (String, Color) {
    match state {
        WorkflowState::Paused  => ("=".to_string(), c_paused()),
        WorkflowState::Dormant => ("·".to_string(), c_dormant()),
        WorkflowState::Running => match run_status {
            Some(RunStatus::WaitingCheckpoint) => ("~".to_string(), c_waiting_cp()),
            Some(RunStatus::Failed)            => ("✗".to_string(), c_failed()),
            // Triggered workflow: engine alive but between trigger events
            Some(RunStatus::Completed) | None
                if matches!(kind, Some(WorkflowType::Triggered)) =>
            {
                match trigger {
                    Some(TriggerDef::Polling { .. }) => ("⏲".to_string(), c_dormant()),
                    _                                => ("·".to_string(), c_dormant()),
                }
            }
            Some(RunStatus::Completed) => ("✓".to_string(), c_completed()),
            _                          => (spinner_frame(tick).to_string(), c_running()),
        },
    }
}

pub fn render_runs(f: &mut Frame, app: &App, area: Rect) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let name_width  = inner_width.saturating_sub(1);
    let tick = app.tick;

    let make_item = |prefix: &str, content: &str, icon: String, icon_color: Color, is_selected: bool| {
        let prefix_len = prefix.chars().count();
        let available_for_content = name_width.saturating_sub(prefix_len);

        let (display_content, padding) = scroll_text(content, available_for_content, tick);

        let content_style = if is_selected {
            Style::default().fg(c_background()).bg(c_foreground()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(c_foreground()).bg(c_background())
        };

        ListItem::new(Line::from(vec![
            Span::styled(prefix.to_string(), Style::default().fg(c_foreground()).bg(c_background())),
            Span::styled(display_content, content_style),
            Span::styled(padding, Style::default().fg(c_foreground()).bg(c_background())),
            Span::styled(icon,   Style::default().fg(icon_color).bg(c_background())),
        ]))
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
            if entry.expanded { "▼ " } else { "▶ " }
        } else {
            "  "
        };
        let running = RunStatus::Running;
        let first_run_status = entry.runs.first().map(|r| {
            if app.pending_checkpoints.get(&r.id).is_some_and(|cp| cp.processing) { &running } else { &r.status }
        });
        let (state_icon, state_color) = workflow_state_color(&entry.state, first_run_status, Some(&entry.kind), entry.trigger.as_ref(), app.tick);
        items.push(make_item(&expand_char, &entry.name, state_icon, state_color, is_workflow_selected));

        if entry.expanded {
            for (ri, run) in entry.runs.iter().enumerate() {
                let is_run_selected = app.cursor == CursorTarget::Run(wi, ri);
                if is_run_selected {
                    selected_index = Some(items.len());
                }

                let datetime = run.started_at.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string();
                let trigger_info = run.trigger_payload.as_ref()
                    .map(|p| format!("  {}", &p[..p.len().min(8)]))
                    .unwrap_or_default();
                let run_content = format!("{}{}", datetime, trigger_info);
                let running = RunStatus::Running;
                let effective_run_status = if app.pending_checkpoints.get(&run.id).is_some_and(|cp| cp.processing) { &running } else { &run.status };
                let (run_icon, run_color) = workflow_state_color(
                    &WorkflowState::Running,
                    Some(effective_run_status),
                    None,
                    None,
                    app.tick,
                );
                items.push(make_item("   ", &run_content, run_icon, run_color, is_run_selected));
            }
        }
    }

    let mut state = ListState::default();
    if let Some(idx) = selected_index {
        state.select(Some(idx));
    }

    let list = List::new(items)
        .block(panel("Workflows"));

    f.render_stateful_widget(list, area, &mut state);
}
