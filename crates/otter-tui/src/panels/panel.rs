use crossterm::event::KeyEvent;
use ratatui::{layout::Rect, Frame};

use super::feedback_prompt::FeedbackPrompt;
use super::help_modal::HelpModal;
use super::marketplaces_panel::MarketplacesPanel;
use super::right_panel::RightPanel;
use super::runs_panel::RunsPanel;
use super::status_bar::PanelHint;
use crate::app::App;

pub trait Panel {
    /// Render the panel into `area`. `focused` is true when this panel
    /// currently has user focus (drives the focused-border style).
    fn render(&mut self, f: &mut Frame, app: &App, area: Rect, focused: bool);

    /// Handle a key press. Returns `true` if the key was consumed.
    fn handle_key(&mut self, app: &mut App, key: KeyEvent) -> bool;

    /// Keybinding hints this panel contributes to the status bar.
    fn hints(&self, app: &App) -> Vec<PanelHint>;
}

#[derive(Default)]
pub struct PanelSet {
    pub runs: RunsPanel,
    pub marketplaces: MarketplacesPanel,
    pub right: RightPanel,
    pub help: HelpModal,
    pub feedback: FeedbackPrompt,
}
