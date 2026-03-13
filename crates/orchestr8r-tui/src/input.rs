use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use orchestr8r_core::types::CheckpointAction;

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
        KeyCode::Tab => {
            let n = app.pending_checkpoints.len();
            if n > 1 {
                app.selected_checkpoint = (app.selected_checkpoint + 1) % n;
            }
        }
        KeyCode::Char('c') => app.respond_checkpoint(CheckpointAction::Continue),
        KeyCode::Char('s') => app.respond_checkpoint(CheckpointAction::Stop),
        KeyCode::Char('f') => {
            if app
                .active_checkpoint()
                .map_or(false, |cp| cp.feedback_available)
            {
                app.mode = Mode::FeedbackInput;
                app.feedback_input.clear();
            }
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
