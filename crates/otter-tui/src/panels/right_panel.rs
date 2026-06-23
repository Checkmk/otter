use chrono::Local;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Padding, Paragraph},
    Frame,
};

use otter_core::types::{MarketplaceOrigin, ProgressChunk, StepDef, WorkflowDef};

use super::panel::Panel;
use super::status_bar::PanelHint;
use crate::app::{App, Focus, Selection};
use crate::styles::{base_style, panel, panel_focused, step_color};
use crate::text::wrap_into_chunks;
use crate::theme;

#[derive(Debug, PartialEq, Clone)]
pub enum RightPanelContent {
    /// Content is derived from the current left-pane selection.
    Contextual,
    /// The consumed-triggers list for the selected polling workflow.
    ConsumedTriggers,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum DefinitionView {
    Preview,
    Raw,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum RenderMode {
    Logs,
    Definition(DefinitionView),
    Marketplace,
    ConsumedTriggers,
}

pub struct RightPanel {
    pub content: RightPanelContent,
    pub definition_view: DefinitionView,
    /// Selected row in the consumed-triggers list.
    pub cursor: usize,
    /// Scroll offset for logs / definition modes.
    pub scroll: usize,
    /// Inner height in rows; updated by the renderer each frame.
    pub height: usize,
}

impl Default for RightPanel {
    fn default() -> Self {
        Self {
            content: RightPanelContent::Contextual,
            definition_view: DefinitionView::Preview,
            cursor: 0,
            scroll: 0,
            height: 0,
        }
    }
}

impl RightPanel {
    pub fn render_mode(&self, selection: Selection<'_>) -> RenderMode {
        match self.content {
            RightPanelContent::ConsumedTriggers => RenderMode::ConsumedTriggers,
            RightPanelContent::Contextual => match selection {
                Selection::Run(_, _) => RenderMode::Logs,
                Selection::Marketplace(_) => RenderMode::Marketplace,
                Selection::Workflow(_) | Selection::MarketplaceWorkflow(_, _) | Selection::None => {
                    RenderMode::Definition(self.definition_view)
                }
            },
        }
    }

    pub fn reset(&mut self) {
        self.content = RightPanelContent::Contextual;
        self.scroll = 0;
    }

    pub fn show_consumed_triggers(&mut self) {
        self.content = RightPanelContent::ConsumedTriggers;
        self.cursor = 0;
    }

    pub fn toggle_definition_view(&mut self, mode: RenderMode) {
        if !matches!(mode, RenderMode::Definition(_)) {
            return;
        }
        self.definition_view = match self.definition_view {
            DefinitionView::Preview => DefinitionView::Raw,
            DefinitionView::Raw => DefinitionView::Preview,
        };
        self.scroll = 0;
    }

    pub fn move_up(&mut self, mode: RenderMode, consumed_len: usize) {
        match mode {
            RenderMode::ConsumedTriggers => self.consumed_cursor_step(consumed_len, -1),
            RenderMode::Logs => self.scroll += 1,
            RenderMode::Definition(_) | RenderMode::Marketplace => {
                self.scroll = self.scroll.saturating_sub(1)
            }
        }
    }

    pub fn move_down(&mut self, mode: RenderMode, consumed_len: usize) {
        match mode {
            RenderMode::ConsumedTriggers => self.consumed_cursor_step(consumed_len, 1),
            RenderMode::Logs => self.scroll = self.scroll.saturating_sub(1),
            RenderMode::Definition(_) | RenderMode::Marketplace => self.scroll += 1,
        }
    }

    pub fn page_up(&mut self, mode: RenderMode) {
        self.scroll_by(mode, self.height, ScrollDir::Up);
    }

    pub fn page_down(&mut self, mode: RenderMode) {
        self.scroll_by(mode, self.height, ScrollDir::Down);
    }

    pub fn half_page_up(&mut self, mode: RenderMode) {
        self.scroll_by(mode, (self.height / 2).max(1), ScrollDir::Up);
    }

    pub fn half_page_down(&mut self, mode: RenderMode) {
        self.scroll_by(mode, (self.height / 2).max(1), ScrollDir::Down);
    }

    pub fn scroll_top(&mut self, mode: RenderMode) {
        // Logs scroll lines-from-bottom: top of history is usize::MAX
        // (the renderer clamps it to auto_bottom).
        self.scroll = if Self::scrolls_top_down(mode) {
            0
        } else {
            usize::MAX
        };
    }

    pub fn scroll_bottom(&mut self, mode: RenderMode) {
        self.scroll = if Self::scrolls_top_down(mode) {
            usize::MAX
        } else {
            0
        };
    }

    /// Clamp the consumed-triggers cursor after the list size changes.
    pub fn clamp_consumed_cursor(&mut self, len: usize) {
        if len == 0 {
            self.cursor = 0;
        } else {
            self.cursor = self.cursor.min(len - 1);
        }
    }

    fn consumed_cursor_step(&mut self, len: usize, dir: i32) {
        if len == 0 {
            return;
        }
        let delta = if dir < 0 { len - 1 } else { 1 };
        self.cursor = (self.cursor + delta) % len;
    }

    fn scroll_by(&mut self, mode: RenderMode, amount: usize, dir: ScrollDir) {
        let top_down = Self::scrolls_top_down(mode);
        let forward = matches!(dir, ScrollDir::Down) == top_down;
        if forward {
            self.scroll += amount;
        } else {
            self.scroll = self.scroll.saturating_sub(amount);
        }
    }

    fn scrolls_top_down(mode: RenderMode) -> bool {
        !matches!(mode, RenderMode::Logs)
    }
}

enum ScrollDir {
    Up,
    Down,
}

#[derive(Debug, PartialEq)]
enum WrappedLogLine {
    Header {
        time: String,
        step_type: String,
        text: String,
    },
    Continuation {
        text: String,
    },
}

fn format_log_entry(
    time: &str,
    step_type: &str,
    text: &str,
    panel_width: usize,
) -> Vec<WrappedLogLine> {
    // "[HH:MM:SS] step_type: " — chars consumed by the header prefix
    let prefix_len = 1 + time.len() + 2 + step_type.len() + 2;
    let first_width = panel_width.saturating_sub(prefix_len);
    let cont_width = panel_width.saturating_sub(2);

    let mut result: Vec<WrappedLogLine> = Vec::new();
    let mut header_emitted = false;

    for raw in text.lines() {
        let content = raw.replace('\r', "");
        if !header_emitted {
            // Take only the first `first_width` chars for the header, then
            // re-split the remainder at cont_width (not first_width).
            let first: String = content.chars().take(first_width).collect();
            let rest: String = content.chars().skip(first_width).collect();
            result.push(WrappedLogLine::Header {
                time: time.to_string(),
                step_type: step_type.to_string(),
                text: first,
            });
            header_emitted = true;
            if !rest.is_empty() {
                for chunk in wrap_into_chunks(&rest, cont_width) {
                    result.push(WrappedLogLine::Continuation { text: chunk });
                }
            }
        } else {
            for chunk in wrap_into_chunks(&content, cont_width) {
                result.push(WrappedLogLine::Continuation { text: chunk });
            }
        }
    }

    if !header_emitted {
        result.push(WrappedLogLine::Header {
            time: time.to_string(),
            step_type: step_type.to_string(),
            text: String::new(),
        });
    }

    result
}

pub fn with_scroll_indicators(
    lines: Vec<Line>,
    scroll_offset: usize,
    inner_height: usize,
) -> Vec<Line> {
    let total = lines.len();
    if total == 0 {
        return lines;
    }

    let above = scroll_offset;
    let below = total.saturating_sub(scroll_offset + inner_height);
    let dim = Style::default()
        .fg(theme::current().dim)
        .bg(theme::current().background);

    let mut visible: Vec<Line> = lines
        .into_iter()
        .skip(scroll_offset)
        .take(inner_height)
        .collect();

    if above > 0 && !visible.is_empty() {
        visible[0] = Line::from(Span::styled(format!("  … {above} more ↑"), dim));
    }
    if below > 0 && !visible.is_empty() {
        let last = visible.len() - 1;
        visible[last] = Line::from(Span::styled(format!("  … {below} more ↓"), dim));
    }

    visible
}

impl Panel for RightPanel {
    fn render(&mut self, f: &mut Frame, app: &App, area: Rect, focused: bool) {
        let mode = self.render_mode(app.selection());
        let title = match mode {
            RenderMode::ConsumedTriggers => "Consumed Triggers",
            RenderMode::Logs => "Run log",
            RenderMode::Definition(DefinitionView::Preview) => "Definition (preview)",
            RenderMode::Definition(DefinitionView::Raw) => "Definition (raw)",
            RenderMode::Marketplace => "Marketplace",
        };
        // 1 col of left padding separates content from the border.
        let block = if focused {
            panel_focused(title)
        } else {
            panel(title)
        }
        .padding(Padding::left(1));
        let inner = block.inner(area);
        f.render_widget(block, area);

        match mode {
            RenderMode::ConsumedTriggers => render_consumed_triggers(self, f, app, inner, focused),
            RenderMode::Logs => render_logs(self, f, app, inner, focused),
            RenderMode::Definition(view) => {
                render_definition_preview(self, f, app, inner, focused, view)
            }
            RenderMode::Marketplace => render_marketplace_summary(self, f, app, inner, focused),
        }
    }

    fn handle_key(&mut self, app: &mut App, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let consumed_len = app.selected_consumed_triggers().len();
        let mode = self.render_mode(app.selection());
        match key.code {
            KeyCode::Esc | KeyCode::Tab | KeyCode::Left => {
                app.ui.focus = Focus::Left;
                self.reset();
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_up(mode, consumed_len),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(mode, consumed_len),
            KeyCode::PageUp => self.page_up(mode),
            KeyCode::PageDown => self.page_down(mode),
            KeyCode::Char('b') if ctrl => self.page_up(mode),
            KeyCode::Char('f') if ctrl => self.page_down(mode),
            KeyCode::Char('u') if ctrl => self.half_page_up(mode),
            KeyCode::Char('d') if ctrl => self.half_page_down(mode),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_top(mode),
            KeyCode::End | KeyCode::Char('G') => self.scroll_bottom(mode),
            KeyCode::Delete => app.delete_selected_consumed_trigger(self),
            KeyCode::Char('w') => self.toggle_definition_view(mode),
            _ => return false,
        }
        true
    }

    fn hints(&self, app: &App) -> Vec<PanelHint> {
        match self.render_mode(app.selection()) {
            RenderMode::Logs => vec![
                PanelHint::new("[↑↓]", "Scroll"),
                PanelHint::new("[Home/End]", "Top/Bottom"),
            ],
            RenderMode::Definition(view) => vec![
                PanelHint::new("[↑↓]", "Scroll"),
                PanelHint::new("[Home/End]", "Top/Bottom"),
                match view {
                    DefinitionView::Preview => PanelHint::new("[W]", "Show raw workflow"),
                    DefinitionView::Raw => PanelHint::new("[W]", "Show workflow preview"),
                },
            ],
            RenderMode::Marketplace => vec![
                PanelHint::new("[↑↓]", "Scroll"),
                PanelHint::new("[Home/End]", "Top/Bottom"),
            ],
            RenderMode::ConsumedTriggers => vec![
                PanelHint::new("[↑↓]", "Scroll"),
                PanelHint::new("[Del]", "Delete trigger"),
                PanelHint::new("[Esc]", "Close"),
            ],
        }
    }
}

fn render_progress_lines<'a>(
    chunk: &ProgressChunk,
    inner_width: usize,
    dim_style: Style,
    stderr_style: Style,
) -> Vec<Line<'a>> {
    let (prefix, text, style) = match chunk {
        ProgressChunk::Status(s) => ("│ ", s.as_str(), dim_style),
        ProgressChunk::Stdout(s) => ("│ ", s.as_str(), dim_style),
        ProgressChunk::Stderr(s) => ("│ ", s.as_str(), stderr_style),
    };
    // "  │ " = 4 chars of prefix per line
    let text_width = inner_width.saturating_sub(4);
    let mut result = Vec::new();
    for raw in text.lines() {
        let content = raw.replace('\r', "");
        if text_width == 0 || content.is_empty() {
            result.push(Line::from(vec![
                Span::styled(format!("  {prefix}"), dim_style),
                Span::styled(content, style),
            ]));
        } else {
            for chunk in wrap_into_chunks(&content, text_width) {
                result.push(Line::from(vec![
                    Span::styled(format!("  {prefix}"), dim_style),
                    Span::styled(chunk, style),
                ]));
            }
        }
    }
    if result.is_empty() {
        result.push(Line::from(Span::styled(format!("  {prefix}"), dim_style)));
    }
    result
}

fn build_log_lines<'a>(
    logs: &[otter_core::types::LogEntry],
    progress: &[(usize, ProgressChunk)],
    inner_width: usize,
    dim_style: Style,
    stderr_style: Style,
) -> Vec<Line<'a>> {
    if logs.is_empty() && progress.is_empty() {
        return vec![Line::from(Span::styled(
            "Select a run to view logs",
            dim_style,
        ))];
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut progress_cursor = 0;

    for (i, entry) in logs.iter().enumerate() {
        let time = entry
            .timestamp
            .with_timezone(&Local)
            .format("%H:%M:%S")
            .to_string();
        let text = if !entry.stdout.is_empty() {
            &entry.stdout
        } else if !entry.stderr.is_empty() {
            &entry.stderr
        } else if let Some(ref fb) = entry.feedback {
            fb
        } else {
            ""
        };

        // If this is a subsequent log entry for the same step, flush pending progress
        // BEFORE emitting it so progress appears between the first and last log entries
        // of the same step (e.g. "Running agent..." → progress → final output).
        if i > 0 && logs[i - 1].step_index == entry.step_index {
            while progress_cursor < progress.len()
                && progress[progress_cursor].0 <= entry.step_index
            {
                lines.extend(render_progress_lines(
                    &progress[progress_cursor].1,
                    inner_width,
                    dim_style,
                    stderr_style,
                ));
                progress_cursor += 1;
            }
        }

        for wl in format_log_entry(&time, &entry.step_type, text, inner_width) {
            match wl {
                WrappedLogLine::Header {
                    time,
                    step_type,
                    text,
                } => lines.push(Line::from(vec![
                    Span::styled(
                        format!("[{}] ", time),
                        Style::default()
                            .fg(theme::current().dim)
                            .bg(theme::current().background),
                    ),
                    Span::styled(
                        format!("{}:", step_type),
                        Style::default()
                            .fg(step_color(&step_type))
                            .bg(theme::current().background),
                    ),
                    Span::styled(format!(" {}", text), base_style()),
                ])),
                WrappedLogLine::Continuation { text } => lines.push(Line::from(vec![
                    Span::styled("  ", base_style()),
                    Span::styled(text, base_style()),
                ])),
            }
        }

        // Emit progress between step groups: when the next log has a higher
        // step_index, drain progress chunks that belong before it.
        // This prevents run_start (step_index usize::MAX) from absorbing all progress.
        let next_step = logs.get(i + 1).map(|e| e.step_index);
        let boundary = next_step.unwrap_or(usize::MAX);
        while progress_cursor < progress.len() && progress[progress_cursor].0 < boundary {
            lines.extend(render_progress_lines(
                &progress[progress_cursor].1,
                inner_width,
                dim_style,
                stderr_style,
            ));
            progress_cursor += 1;
        }
    }

    // Remaining progress chunks (for in-flight steps)
    while progress_cursor < progress.len() {
        lines.extend(render_progress_lines(
            &progress[progress_cursor].1,
            inner_width,
            dim_style,
            stderr_style,
        ));
        progress_cursor += 1;
    }

    lines
}

fn render_logs(right: &mut RightPanel, f: &mut Frame, app: &App, inner: Rect, is_focused: bool) {
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;
    right.height = inner_height;
    let logs = app.selected_logs();

    let progress = app.selected_progress();
    let dim_style = Style::default()
        .fg(theme::current().dim)
        .bg(theme::current().background);
    let stderr_style = Style::default()
        .fg(ratatui::style::Color::Red)
        .bg(theme::current().background);

    let lines: Vec<Line> = build_log_lines(logs, progress, inner_width, dim_style, stderr_style);

    let auto_bottom = lines.len().saturating_sub(inner_height);
    // Clamp panel state so pressing ↓ always has a visible effect after scrolling up.
    right.scroll = right.scroll.min(auto_bottom);
    let scroll_offset = if is_focused {
        auto_bottom - right.scroll
    } else {
        auto_bottom
    } as u16;

    let visible = with_scroll_indicators(lines, scroll_offset as usize, inner_height);
    f.render_widget(Paragraph::new(visible), inner);
}

/// Distinguishes a marketplace-advertised workflow from a locally-installed
/// one when rendering the rich preview. The shared sections (REQUIRES,
/// TRIGGER, WORKSPACE, STEPS, FINALLY, README) are identical between the two;
/// only the header and the call-to-action section vary.
pub(crate) enum PreviewSource<'a> {
    Marketplace {
        marketplace_name: &'a str,
        installed: bool,
        update_available: Option<&'a str>,
        pkg_dir_missing: bool,
    },
    Installed {
        origin: Option<&'a MarketplaceOrigin>,
        update_available: Option<&'a str>,
    },
}

/// Builds the workflow definition preview as a flat list of `Line`s. Pure (no
/// I/O for the parsed def — caller resolves files via `read_file`) to keep
/// the rendering testable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_workflow_preview_lines<'a, F>(
    source: PreviewSource<'_>,
    workflow_name: &str,
    version: Option<&str>,
    description: Option<&str>,
    def: Option<&WorkflowDef>,
    readme: Option<&str>,
    inner_width: usize,
    mut read_file: F,
) -> Vec<Line<'a>>
where
    F: FnMut(&str) -> Option<String>,
{
    let dim = Style::default()
        .fg(theme::current().dim)
        .bg(theme::current().background);
    let bold = base_style().add_modifier(ratatui::style::Modifier::BOLD);
    let mut lines: Vec<Line> = Vec::new();

    // Header: name, version, origin, installed/update status
    let mut header_spans = vec![Span::styled(workflow_name.to_string(), bold)];
    if let Some(v) = version {
        header_spans.push(Span::styled(format!("  v{v}"), dim));
    }
    match &source {
        PreviewSource::Marketplace {
            marketplace_name,
            installed,
            ..
        } => {
            header_spans.push(Span::styled(format!("  ·  from {marketplace_name}"), dim));
            if *installed {
                header_spans.push(Span::styled("  ·  installed".to_string(), dim));
            }
        }
        PreviewSource::Installed { origin, .. } => {
            if let Some(o) = origin {
                header_spans.push(Span::styled(format!("  ·  from {}", o.marketplace), dim));
                if o.dangling {
                    header_spans.push(Span::styled("  (marketplace removed)".to_string(), dim));
                }
            }
        }
    }
    lines.push(Line::from(header_spans));

    if let Some(desc) = description {
        lines.push(Line::from(""));
        for chunk in wrap_into_chunks(desc, inner_width) {
            lines.push(Line::from(Span::styled(chunk, base_style())));
        }
    }

    if matches!(
        source,
        PreviewSource::Marketplace {
            pkg_dir_missing: true,
            ..
        }
    ) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "(marketplace clone missing on disk — run `otter marketplace add` again)".to_string(),
            dim,
        )));
    }

    // Call-to-action: UPDATE AVAILABLE when an installed workflow has a newer
    // upstream version. The install command is qualified with `@<marketplace>`
    // so it works verbatim even when multiple marketplaces ship the same name.
    let update_info: Option<(&str, Option<&str>)> = match &source {
        PreviewSource::Installed {
            origin: Some(o),
            update_available: Some(latest),
        } if !o.dangling => Some((*latest, Some(o.marketplace.as_str()))),
        PreviewSource::Marketplace {
            marketplace_name,
            update_available: Some(latest),
            installed: true,
            ..
        } => Some((*latest, Some(*marketplace_name))),
        _ => None,
    };
    if let Some((latest, marketplace)) = update_info {
        let green_bold = Style::default()
            .fg(theme::current().completed)
            .bg(theme::current().background)
            .add_modifier(ratatui::style::Modifier::BOLD);
        let suffix = marketplace.map(|m| format!("@{m}")).unwrap_or_default();
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("UPDATE AVAILABLE".to_string(), green_bold),
            Span::styled(format!("  → v{latest}"), dim),
        ]));
        lines.push(Line::from(Span::styled(
            format!("  otter workflow install {workflow_name}{suffix}"),
            base_style(),
        )));
    }

    let pkg_dir_missing = matches!(
        source,
        PreviewSource::Marketplace {
            pkg_dir_missing: true,
            ..
        }
    );

    append_readme_lines(&mut lines, readme, inner_width);

    let Some(def) = def else {
        if !pkg_dir_missing {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "(could not parse workflow.toml)".to_string(),
                dim,
            )));
        }
        return lines;
    };

    // REQUIRES
    if matches!(source, PreviewSource::Marketplace { .. }) {
        if let Some(req) = def.require.as_ref() {
            if !req.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled("REQUIRES".to_string(), bold)));
                for (name, entry) in req.iter() {
                    let kind = if entry.sensitive { "secret" } else { "param" };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {name} "), base_style()),
                        Span::styled(format!("({kind})"), dim),
                        Span::styled(" — ".to_string(), dim),
                        Span::styled(entry.description.clone(), base_style()),
                    ]));
                }
            }
        }
    }

    // TRIGGER
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("TRIGGER".to_string(), bold)));
    match def.trigger.as_ref() {
        None => lines.push(Line::from(Span::styled(
            "  (no trigger — looping workflow or manual)".to_string(),
            dim,
        ))),
        Some(otter_core::types::TriggerDef::Manual) => {
            lines.push(Line::from(Span::styled(
                "  manual".to_string(),
                base_style(),
            )));
        }
        Some(otter_core::types::TriggerDef::Polling {
            poll_command,
            context_command,
            interval_secs,
            ..
        }) => {
            lines.push(Line::from(Span::styled(
                format!("  polling, every {interval_secs}s"),
                base_style(),
            )));
            lines.push(Line::from(Span::styled(
                format!("  poll: {}", poll_command.join(" ")),
                base_style(),
            )));
            if let Some(ctx) = context_command {
                lines.push(Line::from(Span::styled(
                    format!("  context: {}", ctx.join(" ")),
                    base_style(),
                )));
            }
        }
        Some(otter_core::types::TriggerDef::Dispatch) => {
            lines.push(Line::from(Span::styled(
                "  dispatch (started by another workflow)".to_string(),
                base_style(),
            )));
        }
    }

    // WORKSPACE
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("WORKSPACE".to_string(), bold)));
    match def.workspace.as_ref() {
        None => lines.push(Line::from(Span::styled(
            "  scratch (default)".to_string(),
            base_style(),
        ))),
        Some(w) => {
            let s = match &w.source {
                otter_core::types::WorkspaceSource::Scratch => "scratch".to_string(),
                otter_core::types::WorkspaceSource::Fixed { path } => format!("fixed: {path}"),
                otter_core::types::WorkspaceSource::Script { command, .. } => {
                    format!("script: {}", command.join(" "))
                }
                otter_core::types::WorkspaceSource::Git { base_repo, ref_ } => match ref_ {
                    Some(r) => format!("git: {base_repo} @ {r}"),
                    None => format!("git: {base_repo}"),
                },
            };
            lines.push(Line::from(Span::styled(format!("  {s}"), base_style())));
            if w.pool.is_some() {
                lines.push(Line::from(Span::styled("  (pooled)".to_string(), dim)));
            }
        }
    }

    // STEPS
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("STEPS".to_string(), bold)));
    for (idx, step) in def.steps.iter().enumerate() {
        append_step_lines(&mut lines, idx + 1, step, inner_width, &mut read_file);
    }
    if !def.finally.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("FINALLY".to_string(), bold)));
        for (idx, fs) in def.finally.iter().enumerate() {
            append_step_lines(&mut lines, idx + 1, &fs.step, inner_width, &mut read_file);
        }
    }

    lines
}

fn append_prefixed_wrapped<'a>(
    lines: &mut Vec<Line<'a>>,
    content: &str,
    prefix: &str,
    prefix_style: Style,
    body_style: Style,
    width: usize,
) {
    let body_width = width.saturating_sub(prefix.chars().count());
    for raw in content.lines() {
        let line = raw.replace('\r', "");
        if line.is_empty() {
            lines.push(Line::from(Span::styled(prefix.to_string(), prefix_style)));
            continue;
        }
        for chunk in wrap_into_chunks(&line, body_width) {
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), prefix_style),
                Span::styled(chunk, body_style),
            ]));
        }
    }
}

fn append_readme_lines<'a>(lines: &mut Vec<Line<'a>>, readme: Option<&str>, inner_width: usize) {
    let Some(readme) = readme else {
        return;
    };
    let bold = base_style().add_modifier(ratatui::style::Modifier::BOLD);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("README.md".to_string(), bold)));
    append_prefixed_wrapped(lines, readme, "  ", base_style(), base_style(), inner_width);
}

fn append_step_lines<'a, F>(
    lines: &mut Vec<Line<'a>>,
    n: usize,
    step: &StepDef,
    inner_width: usize,
    read_file: &mut F,
) where
    F: FnMut(&str) -> Option<String>,
{
    let dim = Style::default()
        .fg(theme::current().dim)
        .bg(theme::current().background);
    let step_type = step.step_type.to_string();
    let mut header = format!("  {n}. {step_type}");
    if let Some(provider) = step.agent.provider.as_deref() {
        header.push_str(&format!(" ({provider})"));
    }
    if let Some(session) = step.session.as_deref() {
        header.push_str(&format!(" [session: {session}]"));
    }

    // Inline message_file contents (or full message if it spans multiple lines).
    let body: Option<(String, String)> = if let Some(file) = step.message_file.as_deref() {
        match read_file(file) {
            Some(content) => Some((file.to_string(), content)),
            None => Some((file.to_string(), String::from("(message_file not found)"))),
        }
    } else {
        step.message.as_ref().and_then(|m| {
            if m.lines().count() > 1 {
                Some(("inline message".to_string(), m.clone()))
            } else {
                None
            }
        })
    };

    // Only show a summary in the header when no body will be rendered below.
    let summary = if body.is_some() {
        None
    } else if let Some(cmd) = step.command.as_ref() {
        Some(cmd.join(" "))
    } else {
        step.message
            .as_deref()
            .map(|msg| msg.lines().next().unwrap_or("").to_string())
    };
    let header_line = match summary {
        Some(s) if !s.is_empty() => format!("{header}: {s}"),
        _ => header,
    };
    for chunk in wrap_into_chunks(&header_line, inner_width) {
        lines.push(Line::from(Span::styled(
            chunk,
            base_style().add_modifier(ratatui::style::Modifier::BOLD),
        )));
    }
    if let Some((source, content)) = body {
        lines.push(Line::from(Span::styled(format!("     ({source})"), dim)));
        append_prefixed_wrapped(lines, &content, "     │ ", dim, base_style(), inner_width);
    }
}

fn render_definition_preview(
    right: &mut RightPanel,
    f: &mut Frame,
    app: &App,
    inner: Rect,
    is_focused: bool,
    view: DefinitionView,
) {
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;
    right.height = inner_height;

    let lines: Vec<Line> = match view {
        DefinitionView::Raw => build_raw_lines(app, inner_width),
        DefinitionView::Preview => match app.selection() {
            Selection::Workflow(_) => build_installed_preview(app, inner_width),
            Selection::MarketplaceWorkflow(_, _) => build_marketplace_preview(app, inner_width),
            // Marketplace is routed to `render_marketplace_summary`, and the
            // dispatcher routes Run cursors to logs.
            Selection::Marketplace(_) | Selection::Run(_, _) | Selection::None => Vec::new(),
        },
    };

    let auto_bottom = lines.len().saturating_sub(inner_height);
    // Clamp panel state so pressing ↑ always has a visible effect after scrolling down.
    right.scroll = right.scroll.min(auto_bottom);
    let scroll_offset = if is_focused { right.scroll } else { 0 };

    let visible = with_scroll_indicators(lines, scroll_offset, inner_height);
    f.render_widget(Paragraph::new(visible), inner);
}

fn render_marketplace_summary(
    right: &mut RightPanel,
    f: &mut Frame,
    app: &App,
    inner: Rect,
    is_focused: bool,
) {
    let inner_width = inner.width as usize;
    let inner_height = inner.height as usize;
    right.height = inner_height;

    let lines = build_marketplace_preview(app, inner_width);

    let auto_bottom = lines.len().saturating_sub(inner_height);
    right.scroll = right.scroll.min(auto_bottom);
    let scroll_offset = if is_focused { right.scroll } else { 0 };

    let visible = with_scroll_indicators(lines, scroll_offset, inner_height);
    f.render_widget(Paragraph::new(visible), inner);
}

/// Renders the unparsed workflow TOML
fn build_raw_lines<'a>(app: &App, inner_width: usize) -> Vec<Line<'a>> {
    let dim = Style::default()
        .fg(theme::current().dim)
        .bg(theme::current().background);

    let toml = match app.selection() {
        Selection::Workflow(e) => e.toml_content.clone(),
        Selection::MarketplaceWorkflow(_, _) => app
            .selected_marketplace_pkg_dir()
            .and_then(|d| std::fs::read_to_string(d.join("workflow.toml")).ok()),
        Selection::Marketplace(_) | Selection::Run(_, _) | Selection::None => None,
    };

    let Some(toml) = toml else {
        return vec![Line::from(Span::styled("No config available", dim))];
    };

    toml.lines()
        .flat_map(|raw_line| {
            let line = raw_line.replace('\r', "");
            if inner_width == 0 || line.len() <= inner_width {
                vec![Line::from(Span::styled(line, base_style()))]
            } else {
                wrap_into_chunks(&line, inner_width)
                    .into_iter()
                    .map(|chunk| Line::from(Span::styled(chunk, base_style())))
                    .collect()
            }
        })
        .collect()
}

fn build_installed_preview<'a>(app: &App, inner_width: usize) -> Vec<Line<'a>> {
    let dim = Style::default()
        .fg(theme::current().dim)
        .bg(theme::current().background);
    let Some(entry) = app.selected_workflow() else {
        return vec![Line::from(Span::styled("No workflow selected", dim))];
    };

    let def = entry
        .toml_content
        .as_deref()
        .and_then(|t| toml::from_str::<WorkflowDef>(t).ok());
    let pkg_dir = app.selected_workflow_pkg_dir();
    let readme = pkg_dir
        .as_ref()
        .and_then(|d| std::fs::read_to_string(d.join("README.md")).ok());
    let version = def.as_ref().and_then(|d| d.version.as_deref());
    let description = def.as_ref().and_then(|d| d.description.as_deref());

    build_workflow_preview_lines(
        PreviewSource::Installed {
            origin: entry.origin.as_ref(),
            update_available: entry.update_available.as_deref(),
        },
        &entry.name,
        version,
        description,
        def.as_ref(),
        readme.as_deref(),
        inner_width,
        |rel| {
            pkg_dir
                .as_ref()
                .and_then(|d| std::fs::read_to_string(d.join(rel)).ok())
        },
    )
}

fn build_marketplace_preview<'a>(app: &App, inner_width: usize) -> Vec<Line<'a>> {
    let dim = Style::default()
        .fg(theme::current().dim)
        .bg(theme::current().background);
    match app.selection() {
        Selection::MarketplaceWorkflow(m, w) => {
            let pkg_dir = app.selected_marketplace_pkg_dir();
            let pkg_dir_missing = pkg_dir.is_none();
            let def = pkg_dir.as_ref().and_then(|d| {
                let toml = std::fs::read_to_string(d.join("workflow.toml")).ok()?;
                toml::from_str::<WorkflowDef>(&toml).ok()
            });
            let readme = pkg_dir
                .as_ref()
                .and_then(|d| std::fs::read_to_string(d.join("README.md")).ok());
            let installed = app.is_workflow_installed(&w.name);
            let update_available = app.workflow_update_available(&w.name);

            build_workflow_preview_lines(
                PreviewSource::Marketplace {
                    marketplace_name: &m.name,
                    installed,
                    update_available,
                    pkg_dir_missing,
                },
                &w.name,
                w.version.as_deref(),
                w.description.as_deref(),
                def.as_ref(),
                readme.as_deref(),
                inner_width,
                |rel| {
                    pkg_dir
                        .as_ref()
                        .and_then(|d| std::fs::read_to_string(d.join(rel)).ok())
                },
            )
        }
        Selection::Marketplace(m) => {
            let bold = base_style().add_modifier(ratatui::style::Modifier::BOLD);
            let installed = m
                .workflows
                .iter()
                .filter(|w| app.is_workflow_installed(&w.name))
                .count();
            let updates = m
                .workflows
                .iter()
                .filter(|w| app.workflow_update_available(&w.name).is_some())
                .count();

            let mut workflows_value =
                format!("{} published · {installed} installed", m.workflow_count);
            if updates > 0 {
                workflows_value.push_str(&format!(" · {updates} update"));
                if updates > 1 {
                    workflows_value.push('s');
                }
            }
            let last_fetched = m
                .last_fetched_at
                .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "never".to_string());

            let label = |text: &str| Span::styled(format!("  {text:<13}"), dim);
            vec![
                Line::from(Span::styled(m.name.clone(), bold)),
                Line::from(""),
                Line::from(vec![
                    label("URL"),
                    Span::styled(m.url.clone(), base_style()),
                ]),
                Line::from(vec![
                    label("Workflows"),
                    Span::styled(workflows_value, base_style()),
                ]),
                Line::from(vec![
                    label("Last fetched"),
                    Span::styled(last_fetched, base_style()),
                ]),
            ]
        }
        _ => vec![Line::from(Span::styled(
            "No marketplace selected".to_string(),
            dim,
        ))],
    }
}

fn render_consumed_triggers(
    right: &mut RightPanel,
    f: &mut Frame,
    app: &App,
    inner: Rect,
    is_focused: bool,
) {
    let triggers = app.selected_consumed_triggers();

    let lines: Vec<Line> = if triggers.is_empty() {
        vec![Line::from(Span::styled(
            "No consumed triggers",
            Style::default()
                .fg(theme::current().dim)
                .bg(theme::current().background),
        ))]
    } else {
        triggers
            .iter()
            .enumerate()
            .map(|(i, hash)| {
                if is_focused && i == right.cursor {
                    Line::from(Span::styled(
                        hash.clone(),
                        Style::default()
                            .fg(theme::current().background)
                            .bg(theme::current().foreground),
                    ))
                } else {
                    Line::from(Span::styled(hash.clone(), base_style()))
                }
            })
            .collect()
    };

    // Scroll to keep right_cursor visible
    let inner_height = inner.height as usize;
    let scroll_offset = if triggers.is_empty() || !is_focused {
        0
    } else {
        right.cursor.saturating_sub(inner_height.saturating_sub(1)) as u16
    };

    let para = Paragraph::new(lines).scroll((scroll_offset, 0));

    f.render_widget(para, inner);
}

#[cfg(test)]
#[path = "right_panel_tests.rs"]
mod right_panel_tests;
