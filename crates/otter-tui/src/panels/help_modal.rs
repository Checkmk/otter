use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::panel::Panel;
use super::right_panel::with_scroll_indicators;
use super::status_bar::PanelHint;
use crate::app::App;
use crate::styles::{base_style, panel_focused};
use crate::theme;

#[derive(Default)]
pub struct HelpModal {
    pub scroll: usize,
    pub open: bool,
}

impl HelpModal {
    pub const FIRST_LAUNCH_ID: &'static str = "help";

    pub fn open(&mut self) {
        self.open = true;
        self.scroll = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    fn lines() -> Vec<Line<'static>> {
        let key = Style::default()
            .fg(theme::current().foreground)
            .bg(theme::current().background)
            .add_modifier(Modifier::BOLD);
        let label = Style::default()
            .fg(theme::current().dim)
            .bg(theme::current().background);
        let heading = Style::default()
            .fg(theme::current().foreground)
            .bg(theme::current().background);

        let row = |k: &'static str, l: &'static str| -> Line<'static> {
            Line::from(vec![
                Span::styled(format!("  {:<11} ", k), key),
                Span::styled(format!(" {}", l), label),
            ])
        };

        let section = |title: &'static str, suffix: &'static str| -> Line<'static> {
            Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    title,
                    heading.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                ),
                Span::raw(suffix),
            ])
        };

        let note = |text: &'static str| -> Line<'static> {
            Line::from(Span::styled(format!("  {}", text), label))
        };

        vec![
            Line::from(""),
            section("Navigation", ""),
            Line::from(""),
            row("[?]", "Help"),
            row("[q]", "Quit"),
            row("[↑↓] / [jk]", "Move cursor / scroll"),
            row("[Tab] / [←→]", "Switch panel focus"),
            Line::from(""),
            section("Managing workflows", " (CLI)"),
            Line::from(""),
            row("otter workflow help", "See available commands"),
            Line::from(""),
            section("Trigger", ""),
            Line::from(""),
            note("Polling triggers run a command that returns a list of hashes."),
            note("Each new hash fires one run. Already-seen hashes are \"consumed\""),
            note("and won't fire again — even across daemon restarts."),
            note("Delete a consumed trigger (via [T] → [Del]) to re-trigger a run."),
            Line::from(""),
        ]
    }

    pub fn render_overlay(&mut self, f: &mut Frame, area: Rect) {
        let block = panel_focused(" Otter ");
        let inner = block.inner(area);
        f.render_widget(block, area);

        let lines = Self::lines();
        let inner_height = inner.height as usize;
        let max_scroll = lines.len().saturating_sub(inner_height);
        self.scroll = self.scroll.min(max_scroll);

        let visible = with_scroll_indicators(lines, self.scroll, inner_height);
        let para = Paragraph::new(visible).style(base_style());
        f.render_widget(para, inner);
    }
}

impl Panel for HelpModal {
    fn render(&mut self, _f: &mut Frame, _app: &App, _area: Rect, _focused: bool) {
        // Rendered as an overlay; see render_overlay above and ui.rs.
    }

    fn handle_key(&mut self, _app: &mut App, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll += 1;
                true
            }
            _ => {
                self.close();
                false
            }
        }
    }

    fn hints(&self, _app: &App) -> Vec<PanelHint> {
        vec![PanelHint::new("[↑↓]", "Scroll")]
    }
}
