use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use orchestr8r_core::types::{CheckpointAction, WorkflowKind, WorkflowState};

use crate::app::{App, Mode};

pub fn handle_key(app: &mut App, key: KeyEvent) {
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
    // Checkpoint actions take priority when a checkpoint is active
    let has_checkpoint = app.active_checkpoint().is_some();

    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Up | KeyCode::Char('k') => {
            if app.selected_run > 0 {
                app.selected_run -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.selected_run + 1 < app.workflow_count() {
                app.selected_run += 1;
            }
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
        KeyCode::Char('s') if !has_checkpoint => {
            let state = app.selected_workflow_state();
            match state {
                Some(WorkflowState::Dormant) => app.start_selected(),
                Some(WorkflowState::Paused) => {
                    if let Some((name, _, _)) = app.selected_workflow() {
                        let name = name.clone();
                        let _ = app.cmd_tx.try_send(
                            orchestr8r_core::types::DaemonCommand::Resume { name }
                        );
                    }
                }
                _ => {}
            }
        }
        KeyCode::Char('p') if !has_checkpoint => {
            if matches!(
                app.selected_workflow_state(),
                Some(WorkflowState::Running)
            ) && matches!(
                app.selected_workflow_kind(),
                Some(WorkflowKind::Indefinite)
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
