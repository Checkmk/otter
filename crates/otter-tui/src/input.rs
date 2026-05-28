use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use otter_core::types::{CheckpointAction, RunStatus, WorkflowState};

use crate::app::{App, CursorTarget, Focus, Modal, Mode};

pub fn handle_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    // Modals capture all input while open
    if app.ui.modal.is_some() {
        handle_modal(app, key);
        return;
    }

    match app.ui.mode {
        Mode::Normal => handle_normal(app, key),
        Mode::FeedbackInput => handle_feedback(app, key),
    }
}

fn handle_normal(app: &mut App, key: KeyEvent) {
    // These are always available regardless of focus
    if key.code == KeyCode::Char('q') {
        app.should_quit = true;
        return;
    }
    if key.code == KeyCode::Char('?') {
        app.ui.modal = Some(Modal::Help { scroll: 0 });
        return;
    }

    // When right panel has focus, delegate to right panel handler
    if app.ui.focus == Focus::Right {
        handle_right_panel(app, key);
        return;
    }

    // Checkpoint actions take priority when a checkpoint is active
    let has_checkpoint = app.active_checkpoint().is_some();

    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            app.move_cursor_up();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.move_cursor_down();
        }
        KeyCode::Char(' ') if !has_checkpoint => {
            // Space toggles expanded state of workflow (only on workflow rows)
            app.toggle_expanded();
        }
        KeyCode::Delete if !has_checkpoint => {
            // Del key deletes a run (only on run rows)
            app.delete_selected_run();
        }
        KeyCode::Char('c') if has_checkpoint => app.respond_checkpoint(CheckpointAction::Continue),
        KeyCode::Char('f')
            if has_checkpoint
                && app
                    .active_checkpoint()
                    .is_some_and(|cp| cp.feedback_available) =>
        {
            app.ui.mode = Mode::FeedbackInput;
            app.ui.feedback_input.clear();
        }
        // Enter: context-sensitive — run row stops active run, workflow row starts/stops workflow
        KeyCode::Enter if !has_checkpoint => match app.ui.cursor {
            CursorTarget::Run(wi, ri) => {
                let is_active = app
                    .workflows
                    .get(wi)
                    .and_then(|e| e.runs.get(ri))
                    .map(|r| matches!(r.status, RunStatus::Running | RunStatus::WaitingCheckpoint))
                    .unwrap_or(false);
                if is_active {
                    app.stop_selected_run();
                }
            }
            CursorTarget::Workflow(_) => match app.selected_workflow_state() {
                Some(WorkflowState::Dormant) => app.start_selected(),
                Some(WorkflowState::Running) => app.stop_selected(),
                _ => {}
            },
            CursorTarget::Marketplace(_) | CursorTarget::MarketplaceWorkflow(_, _) => {}
        },
        // When checkpoint is active, 's' stops the checkpoint
        KeyCode::Char('s') if has_checkpoint => app.respond_checkpoint(CheckpointAction::Stop),
        KeyCode::Char('t') if !has_checkpoint => {
            app.open_consumed_triggers();
        }
        KeyCode::Char('a') if !has_checkpoint => {
            if matches!(app.ui.cursor, CursorTarget::Workflow(_)) {
                app.toggle_enable_selected();
            }
        }
        KeyCode::Tab | KeyCode::Right => {
            app.ui.enter_right_panel();
        }
        _ => {}
    }
}

fn handle_modal(app: &mut App, key: KeyEvent) {
    let handled = if let Some(Modal::Help { scroll }) = &mut app.ui.modal {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *scroll = scroll.saturating_sub(1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *scroll += 1;
                true
            }
            _ => false,
        }
    } else {
        false
    };
    if !handled {
        app.ui.dismiss_modal();
    }
}

fn handle_right_panel(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let cursor = app.ui.cursor;
    let consumed_len = app.selected_consumed_triggers().len();
    match key.code {
        KeyCode::Esc | KeyCode::Tab | KeyCode::Left => app.ui.close_right_panel(),
        KeyCode::Up | KeyCode::Char('k') => app.ui.right.move_up(cursor, consumed_len),
        KeyCode::Down | KeyCode::Char('j') => app.ui.right.move_down(cursor, consumed_len),
        KeyCode::PageUp => app.ui.right.page_up(cursor),
        KeyCode::PageDown => app.ui.right.page_down(cursor),
        KeyCode::Char('b') if ctrl => app.ui.right.page_up(cursor),
        KeyCode::Char('f') if ctrl => app.ui.right.page_down(cursor),
        KeyCode::Char('u') if ctrl => app.ui.right.half_page_up(cursor),
        KeyCode::Char('d') if ctrl => app.ui.right.half_page_down(cursor),
        KeyCode::Home | KeyCode::Char('g') => app.ui.right.scroll_top(cursor),
        KeyCode::End | KeyCode::Char('G') => app.ui.right.scroll_bottom(cursor),
        KeyCode::Delete => app.delete_selected_consumed_trigger(),
        KeyCode::Char('w') => app.ui.right.toggle_definition_view(cursor),
        _ => {}
    }
}

fn handle_feedback(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let text = app.ui.feedback_input.drain(..).collect::<String>();
            app.ui.mode = Mode::Normal;
            app.respond_checkpoint(CheckpointAction::Feedback(text));
        }
        KeyCode::Esc => {
            app.ui.mode = Mode::Normal;
            app.ui.feedback_input.clear();
        }
        KeyCode::Backspace => {
            app.ui.feedback_input.pop();
        }
        KeyCode::Char(c) => {
            app.ui.feedback_input.push(c);
        }
        _ => {}
    }
}
