use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::right_panel::with_scroll_indicators;
use crate::styles::{base_style, panel_focused};
use crate::theme;

pub fn render_help_modal(f: &mut Frame, area: Rect, scroll: &mut usize) {
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

    let lines: Vec<Line> = vec![
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
    ];

    let block = panel_focused(" Otter ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let inner_height = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(inner_height);
    *scroll = (*scroll).min(max_scroll);

    let visible = with_scroll_indicators(lines, *scroll, inner_height);
    let para = Paragraph::new(visible).style(base_style());
    f.render_widget(para, inner);
}
