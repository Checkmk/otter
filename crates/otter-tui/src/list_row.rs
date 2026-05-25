use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::ListItem,
};

use crate::scroll::{scroll_spans, truncate_spans};
use crate::styles::base_style;
use crate::theme;

/// Renders one row in a left-panel list (workflows or marketplaces) using the
/// shared layout: `<prefix><name>...<padding><trailing>`. The name is
/// truncated when not selected and animated-scrolled when selected. Trailing
/// segments — status icons, version badges, suffix markers — are appended
/// after right-padding the name area.
pub fn list_row(
    prefix: &str,
    name: &str,
    trailing: &[(String, Style)],
    name_style: Style,
    is_selected: bool,
    inner_width: usize,
    tick: u64,
) -> ListItem<'static> {
    let prefix_len = prefix.chars().count();
    let trailing_len: usize = trailing.iter().map(|(t, _)| t.chars().count()).sum();
    let available = inner_width.saturating_sub(prefix_len + trailing_len);

    let style = if is_selected {
        Style::default()
            .fg(theme::current().background)
            .bg(theme::current().foreground)
            .add_modifier(Modifier::BOLD)
    } else {
        name_style.patch(Style::default().bg(theme::current().background))
    };

    let name_spans = vec![Span::styled(name.to_string(), style)];
    let rendered = if is_selected {
        scroll_spans(name_spans, available, tick)
    } else {
        truncate_spans(name_spans, available)
    };
    let displayed_len: usize = rendered.iter().map(|s| s.content.chars().count()).sum();
    let padding = " ".repeat(available.saturating_sub(displayed_len));

    let mut spans: Vec<Span<'static>> = vec![Span::styled(prefix.to_string(), base_style())];
    spans.extend(rendered);
    spans.push(Span::styled(padding, base_style()));
    for (text, style) in trailing {
        spans.push(Span::styled(text.clone(), *style));
    }
    ListItem::new(Line::from(spans))
}
