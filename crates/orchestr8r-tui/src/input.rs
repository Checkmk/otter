use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use orchestr8r_core::types::{CheckpointAction, WorkflowType, WorkflowState};

use crate::app::{App, Focus, Mode};

pub fn handle_key(app: &mut App, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    match app.mode {
        Mode::Normal => handle_normal(app, key),
        Mode::FeedbackInput => handle_feedback(app, key),
    }
}

fn handle_normal(app: &mut App, key: KeyEvent) {
    // Quit is always available regardless of focus
    if key.code == KeyCode::Char('q') {
        app.should_quit = true;
        return;
    }

    // When right panel has focus, delegate to right panel handler
    if app.focus == Focus::Right {
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
        KeyCode::Char('c') if has_checkpoint => {
            app.respond_checkpoint(CheckpointAction::Continue)
        }
        KeyCode::Char('f') if has_checkpoint => {
            if app.active_checkpoint().map_or(false, |cp| cp.feedback_available) {
                app.mode = Mode::FeedbackInput;
                app.feedback_input.clear();
            }
        }
        // Workflow management keybindings (only when no checkpoint is pending)
        KeyCode::Enter if !has_checkpoint => {
            let state = app.selected_workflow_state();
            match state {
                Some(WorkflowState::Dormant) | Some(WorkflowState::Paused) => app.start_selected(),
                Some(WorkflowState::Running) => app.stop_selected(),
                _ => {}
            }
        }
        KeyCode::Char('p') if !has_checkpoint => {
            if matches!(
                app.selected_workflow_state(),
                Some(WorkflowState::Running)
            ) && matches!(
                app.selected_workflow_kind(),
                Some(WorkflowType::Looping)
            ) {
                app.pause_selected();
            }
        }
        KeyCode::Char('x') if !has_checkpoint => {
            if matches!(
                app.selected_workflow_state(),
                Some(WorkflowState::Running) | Some(WorkflowState::Paused)
            ) {
                app.stop_selected();
            }
        }
        // When checkpoint is active, 's' stops the checkpoint
        KeyCode::Char('s') if has_checkpoint => {
            app.respond_checkpoint(CheckpointAction::Stop)
        }
        KeyCode::Char('t') if !has_checkpoint => {
            app.open_consumed_triggers();
        }
        KeyCode::Tab => {
            app.enter_right_panel();
        }
        _ => {}
    }
}

fn handle_right_panel(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Tab => app.close_right_panel(),
        KeyCode::Up | KeyCode::Char('k') => app.move_right_up(),
        KeyCode::Down | KeyCode::Char('j') => app.move_right_down(),
        KeyCode::Delete => app.delete_selected_consumed_trigger(),
        _ => {}
    }
}

fn handle_feedback(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let text = app.feedback_input.drain(..).collect::<String>();
            app.mode = Mode::Normal;
            app.respond_checkpoint(CheckpointAction::Feedback(text));
        }
        KeyCode::Esc => {
            app.mode = Mode::Normal;
            app.feedback_input.clear();
        }
        KeyCode::Backspace => {
            app.feedback_input.pop();
        }
        KeyCode::Char(c) => {
            app.feedback_input.push(c);
        }
        _ => {}
    }
}
