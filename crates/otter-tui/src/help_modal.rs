use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::right_panel::with_scroll_indicators;
use crate::styles::{base_style, c_background, c_dim, c_foreground, panel_focused};

pub fn render_help_modal(f: &mut Frame, area: Rect, scroll: &mut usize) {
    let key = Style::default().fg(c_foreground()).bg(c_background()).add_modifier(Modifier::BOLD);
    let label = Style::default().fg(c_dim()).bg(c_background());
    let heading = Style::default().fg(c_foreground()).bg(c_background());

    let row = |k: &'static str, l: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("  {:<11} ", k), key),
            Span::styled(format!(" {}", l), label),
        ])
    };

    let section = |title: &'static str, suffix: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(title, heading.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)),
            Span::raw(suffix),
        ])
    };

    let note = |text: &'static str| -> Line<'static> {
        Line::from(Span::styled(format!("  {}", text), label))
    };

    let lines: Vec<Line> = vec![
        Line::from(""),
        section("Navigation", ""),
        Line::from(""),
        row("[q]", "Quit"),
        row("[↑↓] / [jk]", "Move cursor / scroll"),
        row("[Tab]", "Switch panel focus"),
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
    ];

    let inner_height = area.height.saturating_sub(2) as usize;
    let max_scroll = lines.len().saturating_sub(inner_height);
    *scroll = (*scroll).min(max_scroll);

    let visible = with_scroll_indicators(lines, *scroll, inner_height);
    let para = Paragraph::new(visible).block(panel_focused(" Help ")).style(base_style());
    f.render_widget(para, area);
}
