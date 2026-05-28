use crossterm::event::{KeyCode, KeyEvent};
use otter_core::types::CheckpointAction;
use ratatui::{layout::Rect, Frame};

use super::panel::Panel;
use super::status_bar::PanelHint;
use crate::app::App;

#[derive(Default)]
pub struct FeedbackPrompt {
    input: String,
    open: bool,
}

impl FeedbackPrompt {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn open(&mut self) {
        self.open = true;
        self.input.clear();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.input.clear();
    }
}

impl Panel for FeedbackPrompt {
    fn render(&mut self, _f: &mut Frame, _app: &App, _area: Rect, _focused: bool) {
        // Rendered as part of the status bar (see ui.rs).
    }

    fn handle_key(&mut self, app: &mut App, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                self.open = false;
                app.respond_checkpoint(CheckpointAction::Feedback(text));
                true
            }
            KeyCode::Esc => {
                self.close();
                true
            }
            KeyCode::Backspace => {
                self.input.pop();
                true
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                true
            }
            _ => false,
        }
    }

    fn hints(&self, _app: &App) -> Vec<PanelHint> {
        Vec::new()
    }
}
