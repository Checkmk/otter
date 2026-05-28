use crossterm::event::{KeyCode, KeyEvent};
use otter_core::types::CheckpointAction;
use ratatui::{layout::Rect, Frame};

use crate::app::{App, Mode};
use crate::panel::Panel;
use crate::status_bar::PanelHint;

/// The feedback prompt overlay shown at the bottom of the screen during a
/// checkpoint. Followup will move state (`input`, `open`) here.
#[derive(Default)]
pub struct FeedbackPrompt;

impl Panel for FeedbackPrompt {
    fn render(&mut self, _f: &mut Frame, _app: &App, _area: Rect, _focused: bool) {
        // Rendered as part of the status bar (see ui.rs).
    }

    fn handle_key(&mut self, app: &mut App, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter => {
                let text = app.ui.feedback_input.drain(..).collect::<String>();
                app.ui.mode = Mode::Normal;
                app.respond_checkpoint(CheckpointAction::Feedback(text));
                true
            }
            KeyCode::Esc => {
                app.ui.mode = Mode::Normal;
                app.ui.feedback_input.clear();
                true
            }
            KeyCode::Backspace => {
                app.ui.feedback_input.pop();
                true
            }
            KeyCode::Char(c) => {
                app.ui.feedback_input.push(c);
                true
            }
            _ => false,
        }
    }

    fn hints(&self, _app: &App) -> Vec<PanelHint> {
        Vec::new()
    }
}
