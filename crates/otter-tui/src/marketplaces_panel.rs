use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::{App, CursorTarget, Focus};
use crate::list_row::list_row;
use crate::panel::Panel;
use crate::status_bar::PanelHint;
use crate::styles::base_style;
use crate::theme;

#[derive(Default)]
pub struct MarketplacesPanel {
    expanded: std::collections::HashSet<String>,
}

impl MarketplacesPanel {
    pub fn is_expanded(&self, name: &str) -> bool {
        self.expanded.contains(name)
    }

    pub fn toggle_expanded(&mut self, name: &str) -> bool {
        if self.expanded.remove(name) {
            false
        } else {
            self.expanded.insert(name.to_string());
            true
        }
    }

    pub fn retain_expanded<F: Fn(&str) -> bool>(&mut self, keep: F) {
        self.expanded.retain(|k| keep(k));
    }
}

impl Panel for MarketplacesPanel {
    fn render(&mut self, _f: &mut Frame, _app: &App, _area: Rect, _focused: bool) {
        // The marketplaces panel is laid out from ui.rs alongside the runs
        // panel, so the dispatcher does not call this method directly.
        // ui.rs invokes `render_marketplaces` after carving out its slot.
    }

    fn handle_key(&mut self, app: &mut App, key: KeyEvent) -> bool {
        if !matches!(key.code, KeyCode::Char(' ')) {
            return false;
        }
        let CursorTarget::Marketplace(mi) = app.ui.cursor else {
            return true;
        };
        let Some(m) = app.marketplaces.get(mi) else {
            return true;
        };
        if m.workflows.is_empty() {
            return true;
        }
        if !self.toggle_expanded(&m.name) {
            // Collapsed: snap cursor back to the marketplace row.
            app.ui.cursor = CursorTarget::Marketplace(mi);
        }
        true
    }

    fn hints(&self, app: &App) -> Vec<PanelHint> {
        let mut hints = Vec::new();
        match app.ui.cursor {
            CursorTarget::Marketplace(mi) => {
                if let Some(m) = app.marketplaces.get(mi) {
                    if !m.workflows.is_empty() {
                        let label = if self.is_expanded(&m.name) {
                            "Hide workflows"
                        } else {
                            "Show workflows"
                        };
                        hints.push(PanelHint::new("[Space]", label));
                    }
                }
            }
            CursorTarget::MarketplaceWorkflow(_, _) => {}
            _ => {}
        }
        hints
    }
}

pub fn footer_height(app: &App, panel: &MarketplacesPanel, panel_height: u16) -> u16 {
    if app.marketplaces.is_empty() {
        return 0;
    }
    let natural = visible_rows(app, panel).len() as u16 + 2; // +1 divider line, +1 trailing blank
    let cap = (panel_height / 3).max(3);
    natural.min(cap)
}

/// A marketplace workflow is shown when it's either not installed yet
/// (advertisement value) or installed-but-out-of-date (update value).
pub(crate) fn workflow_is_visible(
    app: &App,
    entry: &otter_core::types::MarketplaceWorkflowEntry,
) -> bool {
    !app.is_workflow_installed(&entry.name) || app.workflow_update_available(&entry.name).is_some()
}

/// Builds the flat list of (CursorTarget, line) rows the panel will render.
/// Used both for rendering and for the height calculation above.
fn visible_rows(app: &App, panel: &MarketplacesPanel) -> Vec<(CursorTarget, RowKind)> {
    let mut rows = Vec::new();
    for (mi, m) in app.marketplaces.iter().enumerate() {
        rows.push((CursorTarget::Marketplace(mi), RowKind::Marketplace));
        if panel.is_expanded(&m.name) {
            for (wi, w) in m.workflows.iter().enumerate() {
                if !workflow_is_visible(app, w) {
                    continue;
                }
                rows.push((CursorTarget::MarketplaceWorkflow(mi, wi), RowKind::Workflow));
            }
        }
    }
    rows
}

#[derive(Debug, Clone, Copy)]
enum RowKind {
    Marketplace,
    Workflow,
}

pub fn render_marketplaces(f: &mut Frame, app: &App, panel: &MarketplacesPanel, area: Rect) {
    if app.marketplaces.is_empty() || area.height == 0 {
        return;
    }

    // Carve out the divider line; the rest is the list.
    let [divider_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let inner_width = area.width as usize;
    let tick = app.ui.tick;

    // Inline section divider: `── Marketplaces ──────` — colored to match
    // the surrounding panel border (which itself swaps style on focus).
    let label = "─ Marketplaces ";
    let trailing = inner_width.saturating_sub(label.chars().count());
    let divider = format!("{label}{}", "─".repeat(trailing));
    let left_focused = app.ui.modal.is_none() && app.ui.focus == Focus::Left;
    let divider_style = if left_focused {
        Style::default()
            .fg(theme::current().foreground)
            .bg(theme::current().background)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme::current().border)
            .bg(theme::current().background)
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(divider, divider_style))),
        divider_area,
    );
    let dim = Style::default()
        .fg(theme::current().dim)
        .bg(theme::current().background);
    let badge_style = Style::default()
        .fg(theme::current().completed)
        .bg(theme::current().background);

    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_index: Option<usize> = None;

    for (mi, m) in app.marketplaces.iter().enumerate() {
        let is_selected = app.ui.cursor == CursorTarget::Marketplace(mi);
        if is_selected {
            selected_index = Some(items.len());
        }
        let expanded = panel.is_expanded(&m.name);
        let visible_count = m
            .workflows
            .iter()
            .filter(|w| workflow_is_visible(app, w))
            .count();
        let update_count = m
            .workflows
            .iter()
            .filter(|w| app.workflow_update_available(&w.name).is_some())
            .count();
        let arrow = if visible_count == 0 {
            "  "
        } else if expanded {
            "▼ "
        } else {
            "▶ "
        };
        // Only signal an actionable update — bare advertisements get no suffix.
        let trailing: Vec<(String, Style)> = if update_count > 0 {
            vec![(" ▲".to_string(), badge_style)]
        } else {
            Vec::new()
        };
        items.push(list_row(
            arrow,
            &m.name,
            &trailing,
            base_style(),
            is_selected,
            inner_width,
            tick,
        ));

        if expanded {
            for (wi, w) in m.workflows.iter().enumerate() {
                if !workflow_is_visible(app, w) {
                    continue;
                }
                let is_wf_selected = app.ui.cursor == CursorTarget::MarketplaceWorkflow(mi, wi);
                if is_wf_selected {
                    selected_index = Some(items.len());
                }
                // Post-filter, every visible row is either not-installed (no
                // badge, no ✓) or installed-and-out-of-date (▲ + ✓).
                let installed = app.is_workflow_installed(&w.name);
                let name_style = if installed { dim } else { base_style() };
                let mut trailing: Vec<(String, Style)> = Vec::new();
                if installed {
                    trailing.push((" ✓".to_string(), base_style()));
                }
                if let Some(v) = app.workflow_update_available(&w.name) {
                    trailing.push((format!(" ▲ {v}"), badge_style));
                }
                items.push(list_row(
                    "   ",
                    &w.name,
                    &trailing,
                    name_style,
                    is_wf_selected,
                    inner_width,
                    tick,
                ));
            }
        }
    }

    items.push(ListItem::new(""));

    let mut state = ListState::default();
    if let Some(idx) = selected_index {
        state.select(Some(idx));
    }

    let list = List::new(items);
    f.render_stateful_widget(list, list_area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use otter_core::types::{MarketplaceStatus, MarketplaceWorkflowEntry};
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn make_app() -> App {
        let (tx, _rx) = mpsc::channel(32);
        App::new(
            tx,
            PathBuf::from("/tmp/otter-tui-test"),
            PathBuf::from("/tmp/otter-tui-test-config"),
        )
    }

    fn make_marketplace(name: &str, workflows: Vec<&str>) -> MarketplaceStatus {
        MarketplaceStatus {
            name: name.to_string(),
            url: format!("https://example.com/{name}"),
            workflow_count: workflows.len(),
            last_fetched_at: None,
            workflows: workflows
                .into_iter()
                .map(|n| MarketplaceWorkflowEntry {
                    name: n.to_string(),
                    version: Some("1.0.0".to_string()),
                    description: Some("desc".to_string()),
                    path: format!("workflows/{n}"),
                })
                .collect(),
        }
    }

    #[test]
    fn footer_height_zero_when_no_marketplaces() {
        // GIVEN no marketplaces
        let app = make_app();
        let panel = MarketplacesPanel::default();
        // WHEN
        let h = footer_height(&app, &panel, 30);
        // THEN
        assert_eq!(h, 0);
    }

    #[test]
    fn footer_height_caps_at_panel_third() {
        // GIVEN many marketplaces, all expanded
        let mut app = make_app();
        let mut panel = MarketplacesPanel::default();
        let mut ms = Vec::new();
        for i in 0..10 {
            ms.push(make_marketplace(
                &format!("mp{i}"),
                vec!["a", "b", "c", "d", "e"],
            ));
        }
        app.marketplaces = ms;
        for m in &app.marketplaces {
            panel.expanded.insert(m.name.clone());
        }
        // WHEN panel is 30 lines
        let h = footer_height(&app, &panel, 30);
        // THEN footer takes at most 1/3 of the panel
        assert!(h <= 10);
    }

    #[test]
    fn footer_height_uses_natural_size_when_small() {
        // GIVEN one collapsed marketplace
        let mut app = make_app();
        let panel = MarketplacesPanel::default();
        app.marketplaces = vec![make_marketplace("acme", vec!["a", "b"])];
        // WHEN
        let h = footer_height(&app, &panel, 30);
        // THEN one divider line + one row + one trailing blank = 3
        assert_eq!(h, 3);
    }

    #[test]
    fn workflow_is_visible_hides_installed_and_current() {
        // GIVEN one workflow installed without an update
        let mut app = make_app();
        app.marketplaces = vec![make_marketplace("acme", vec!["a", "b"])];
        app.workflows.push(crate::app::WorkflowEntry {
            name: "a".to_string(),
            kind: otter_core::types::WorkflowType::Looping,
            state: otter_core::types::WorkflowState::Dormant,
            runs: Vec::new(),
            trigger: None,
            toml_content: None,
            autostart: false,
            update_available: None,
            origin: None,
        });
        // WHEN
        let entry_a = &app.marketplaces[0].workflows[0];
        let entry_b = &app.marketplaces[0].workflows[1];
        // THEN: a is installed+current → hidden; b is not installed → shown
        assert!(!workflow_is_visible(&app, entry_a));
        assert!(workflow_is_visible(&app, entry_b));
    }

    #[test]
    fn workflow_is_visible_keeps_installed_with_update() {
        // GIVEN a workflow installed AND with an update available
        let mut app = make_app();
        app.marketplaces = vec![make_marketplace("acme", vec!["a"])];
        app.workflows.push(crate::app::WorkflowEntry {
            name: "a".to_string(),
            kind: otter_core::types::WorkflowType::Looping,
            state: otter_core::types::WorkflowState::Dormant,
            runs: Vec::new(),
            trigger: None,
            toml_content: None,
            autostart: false,
            update_available: Some("1.1.0".to_string()),
            origin: None,
        });
        // WHEN/THEN
        assert!(workflow_is_visible(&app, &app.marketplaces[0].workflows[0]));
    }

    #[test]
    fn marketplaces_hints_for_collapsed_marketplace_shows_show_workflows() {
        // GIVEN cursor on a collapsed marketplace with workflows
        let mut app = make_app();
        let panel = MarketplacesPanel::default();
        app.marketplaces = vec![make_marketplace("acme", vec!["a"])];
        app.ui.cursor = CursorTarget::Marketplace(0);
        // WHEN
        let hints = panel.hints(&app);
        // THEN
        let space = hints.iter().find(|h| h.key == "[Space]");
        assert!(space.is_some());
        assert_eq!(space.unwrap().label, "Show workflows");
    }

    #[test]
    fn marketplaces_hints_for_expanded_marketplace_shows_hide_workflows() {
        // GIVEN expanded marketplace
        let mut app = make_app();
        let mut panel = MarketplacesPanel::default();
        app.marketplaces = vec![make_marketplace("acme", vec!["a"])];
        panel.expanded.insert("acme".into());
        app.ui.cursor = CursorTarget::Marketplace(0);
        // WHEN
        let hints = panel.hints(&app);
        // THEN
        assert_eq!(
            hints.iter().find(|h| h.key == "[Space]").unwrap().label,
            "Hide workflows"
        );
    }
}
