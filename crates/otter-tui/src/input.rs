use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use otter_core::types::CheckpointAction;

use crate::app::{App, CursorTarget, Focus, Modal};
use crate::panel::{Panel, PanelSet};

pub fn handle_key(app: &mut App, panels: &mut PanelSet, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    // Modals capture all input while open
    if app.ui.modal.is_some() {
        handle_modal(app, panels, key);
        return;
    }

    if panels.feedback.is_open() {
        panels.feedback.handle_key(app, key);
        return;
    }

    handle_normal(app, panels, key);
}

fn handle_normal(app: &mut App, panels: &mut PanelSet, key: KeyEvent) {
    // Always-available global keys
    if key.code == KeyCode::Char('q') {
        app.should_quit = true;
        return;
    }
    if key.code == KeyCode::Char('?') {
        app.ui.modal = Some(Modal::Help);
        panels.help.open();
        return;
    }

    // When right panel has focus, delegate everything to it
    if app.ui.focus == Focus::Right {
        panels.right.handle_key(app, key);
        return;
    }

    // Checkpoint actions take priority when a checkpoint is active
    let has_checkpoint = app.active_checkpoint().is_some();
    if has_checkpoint {
        match key.code {
            KeyCode::Char('c') => {
                app.respond_checkpoint(CheckpointAction::Continue);
                return;
            }
            KeyCode::Char('s') => {
                app.respond_checkpoint(CheckpointAction::Stop);
                return;
            }
            KeyCode::Char('f')
                if app
                    .active_checkpoint()
                    .is_some_and(|cp| cp.feedback_available) =>
            {
                panels.feedback.open();
                return;
            }
            _ => {}
        }
    }

    // Cross-panel keys handled by the dispatcher
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if app.move_cursor_up(panels) {
                panels.right.reset();
            }
            return;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.move_cursor_down(panels) {
                panels.right.reset();
            }
            return;
        }
        KeyCode::Tab | KeyCode::Right => {
            app.ui.focus = Focus::Right;
            panels.right.reset();
            return;
        }
        KeyCode::Char('t') if !has_checkpoint => {
            if app.open_consumed_triggers() {
                panels.right.show_consumed_triggers();
            }
            return;
        }
        _ => {}
    }

    if has_checkpoint {
        // The remaining keys (Space/Del/Enter/'a') would be panel actions; while a
        // checkpoint is active, only the cross-panel keys above are allowed.
        return;
    }

    // Delegate to the panel that owns the current cursor row.
    match app.ui.cursor {
        CursorTarget::Marketplace(_) | CursorTarget::MarketplaceWorkflow(_, _) => {
            panels.marketplaces.handle_key(app, key);
        }
        CursorTarget::Workflow(_) | CursorTarget::Run(_, _) => {
            panels.runs.handle_key(app, key);
        }
    }
}

fn handle_modal(app: &mut App, panels: &mut PanelSet, key: KeyEvent) {
    let Some(modal) = app.ui.modal else { return };
    let handled = match modal {
        Modal::Help => panels.help.handle_key(app, key),
    };
    if !handled {
        app.ui.dismiss_modal();
    }
}
