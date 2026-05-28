use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Clear, Paragraph},
    Frame,
};

use crate::app::{App, CursorTarget, Focus, Modal};
use crate::marketplaces_panel::{footer_height, render_marketplaces};
use crate::panel::{Panel, PanelSet};
use crate::runs_panel::render_runs;
use crate::status_bar::{render_status_bar, PanelHint, StatusBarMode};
use crate::styles::base_style;
use crate::styles::{panel, panel_focused};
use crate::text::wrap_into_chunks;

fn centered_rect(width: Constraint, height: Constraint, area: Rect) -> Rect {
    let [_, v, _] =
        Layout::vertical([Constraint::Fill(1), height, Constraint::Fill(1)]).areas(area);
    let [_, h, _] = Layout::horizontal([Constraint::Fill(1), width, Constraint::Fill(1)]).areas(v);
    h
}

fn active_left_panel<'a>(app: &App, panels: &'a mut PanelSet) -> &'a mut dyn Panel {
    match app.ui.cursor {
        CursorTarget::Marketplace(_) | CursorTarget::MarketplaceWorkflow(_, _) => {
            &mut panels.marketplaces
        }
        CursorTarget::Workflow(_) | CursorTarget::Run(_, _) => &mut panels.runs,
    }
}

pub fn render(f: &mut Frame, app: &mut App, panels: &mut PanelSet) {
    f.render_widget(Paragraph::new("").style(base_style()), f.area());

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top padding
            Constraint::Min(0),    // content
            Constraint::Length(1), // bottom padding
        ])
        .split(f.area());

    // render_header(f, rows[1]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(2), // left padding
            Constraint::Min(0),    // content
            Constraint::Length(2), // right padding
        ])
        .split(rows[1]);

    let content = cols[1];

    let status_bar_height = if panels.feedback.is_open() {
        let panel_width = content.width as usize;
        let overhead = 15;
        let available_width = panel_width.saturating_sub(overhead);
        let actual_width = available_width.max(20);
        let input_lines = if panels.feedback.input().is_empty() {
            1
        } else {
            let wrapped = wrap_into_chunks(panels.feedback.input(), actual_width);
            wrapped.len()
        };
        (2 + input_lines).min(7) as u16
    } else {
        3
    };

    let inner = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(status_bar_height)])
        .split(content);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(35), Constraint::Min(1)])
        .split(inner[0]);

    let left_panel = main[0];
    let left_focused = app.ui.modal.is_none() && app.ui.focus == Focus::Left;
    let left_block = if left_focused {
        panel_focused("Workflows")
    } else {
        panel("Workflows")
    };
    let left_inner = left_block.inner(left_panel);
    f.render_widget(left_block, left_panel);

    let mp_height = footer_height(app, &panels.marketplaces, left_inner.height);
    let left_split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(mp_height)])
        .split(left_inner);

    render_runs(f, app, &panels.runs, left_split[0]);
    if mp_height > 0 {
        render_marketplaces(f, app, &panels.marketplaces, left_split[1]);
    }
    let right_focused = app.ui.modal.is_none() && app.ui.focus == Focus::Right;
    panels.right.render(f, app, main[1], right_focused);

    if let Some(modal) = &app.ui.modal {
        match modal {
            Modal::Help => {
                let popup_area = centered_rect(
                    Constraint::Percentage(80),
                    Constraint::Percentage(80),
                    inner[0],
                );
                f.render_widget(Clear, popup_area);
                panels.help.render_overlay(f, popup_area);
            }
        }
    }

    let status_mode = if let Some(modal) = &app.ui.modal {
        match modal {
            Modal::Help => StatusBarMode::Modal {
                hints: panels.help.hints(app),
                close: PanelHint::new("[Any]", "Close help"),
                tick: app.ui.tick,
            },
        }
    } else if panels.feedback.is_open() {
        let overhead = 15;
        let available_width = (inner[1].width as usize).saturating_sub(overhead);
        StatusBarMode::Prompt {
            input: panels.feedback.input(),
            available_width,
            tick: app.ui.tick,
        }
    } else if app.ui.focus == Focus::Right {
        StatusBarMode::Normal {
            panel_hints: panels.right.hints(app),
            other_checkpoints: 0,
            tick: app.ui.tick,
            update_available: app.update_available.as_deref(),
        }
    } else {
        match app.active_checkpoint() {
            Some(cp) => StatusBarMode::Action {
                feedback_available: cp.feedback_available,
            },
            None => StatusBarMode::Normal {
                panel_hints: active_left_panel(app, panels).hints(app),
                other_checkpoints: app.other_checkpoint_count(),
                tick: app.ui.tick,
                update_available: app.update_available.as_deref(),
            },
        }
    };

    render_status_bar(f, status_mode, inner[1]);
}
