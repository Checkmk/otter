use chrono::Local;
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use otter_core::types::{MarketplaceOrigin, ProgressChunk, StepDef, WorkflowDef};

use crate::app::{App, CursorTarget, Focus, RightPanelContent};
use crate::status_bar::PanelHint;
use crate::styles::{base_style, panel, panel_focused, step_color};
use crate::text::wrap_into_chunks;
use crate::theme;

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

pub fn render_right_panel(f: &mut Frame, app: &mut App, area: Rect) {
    match &app.right_panel_content {
        RightPanelContent::ConsumedTriggers => {
            let is_focused = app.modal.is_none() && app.focus == Focus::Right;
            render_consumed_triggers(f, app, area, is_focused);
        }
        RightPanelContent::Contextual => {
            let is_focused = app.modal.is_none() && app.focus == Focus::Right;
            match app.cursor {
                CursorTarget::Run(_, _) => render_logs(f, app, area, is_focused),
                CursorTarget::Workflow(_)
                | CursorTarget::Marketplace(_)
                | CursorTarget::MarketplaceWorkflow(_, _) => {
                    render_definition_preview(f, app, area, is_focused)
                }
            }
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

fn render_logs(f: &mut Frame, app: &mut App, area: Rect, is_focused: bool) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;
    app.right_panel_height = inner_height;
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
    // Clamp app state so pressing ↓ always has a visible effect after scrolling up.
    app.right_scroll = app.right_scroll.min(auto_bottom);
    let scroll_offset = if is_focused {
        auto_bottom - app.right_scroll
    } else {
        auto_bottom
    } as u16;

    let visible = with_scroll_indicators(lines, scroll_offset as usize, inner_height);
    let block = if is_focused {
        panel_focused("Run log")
    } else {
        panel("Run log")
    };
    let para = Paragraph::new(visible).block(block);

    f.render_widget(para, area);
}

/// Distinguishes a marketplace-advertised workflow from a locally-installed
/// one when rendering the rich preview. The shared sections (REQUIRES,
/// TRIGGER, WORKSPACE, STEPS, FINALLY, README) are identical between the two;
/// only the header and the call-to-action section vary.
pub(crate) enum PreviewSource<'a> {
    Marketplace {
        marketplace_name: &'a str,
        installed: bool,
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

    // Call-to-action section: INSTALL for marketplace advertisements, UPDATE
    // when an installed workflow has a newer upstream version. Installed
    // workflows that are current (or have no origin) get nothing — the header
    // already conveys their status.
    match &source {
        PreviewSource::Marketplace {
            marketplace_name, ..
        } => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("INSTALL".to_string(), bold)));
            lines.push(Line::from(Span::styled(
                format!("  otter workflow install {workflow_name}@{marketplace_name}"),
                base_style(),
            )));
        }
        PreviewSource::Installed {
            origin: Some(o),
            update_available: Some(latest),
        } if !o.dangling => {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("UPDATE AVAILABLE".to_string(), bold),
                Span::styled(format!("  → v{latest}"), dim),
            ]));
            lines.push(Line::from(Span::styled(
                format!("  otter workflow install {workflow_name}"),
                base_style(),
            )));
        }
        PreviewSource::Installed { .. } => {}
    }

    let pkg_dir_missing = matches!(
        source,
        PreviewSource::Marketplace {
            pkg_dir_missing: true,
            ..
        }
    );

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

    // README
    if let Some(readme) = readme {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("README.md".to_string(), bold)));
        for raw in readme.lines() {
            let line = raw.replace('\r', "");
            for chunk in wrap_into_chunks(&line, inner_width.saturating_sub(2)) {
                lines.push(Line::from(vec![
                    Span::styled("  ".to_string(), base_style()),
                    Span::styled(chunk, base_style()),
                ]));
            }
            if line.is_empty() {
                lines.push(Line::from(""));
            }
        }
    }

    lines
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
        for raw in content.lines() {
            let line = raw.replace('\r', "");
            for chunk in wrap_into_chunks(&line, inner_width.saturating_sub(7)) {
                lines.push(Line::from(vec![
                    Span::styled("     │ ".to_string(), dim),
                    Span::styled(chunk, base_style()),
                ]));
            }
            if line.is_empty() {
                lines.push(Line::from(Span::styled("     │ ".to_string(), dim)));
            }
        }
    }
}

fn render_definition_preview(f: &mut Frame, app: &mut App, area: Rect, is_focused: bool) {
    let inner_width = area.width.saturating_sub(2) as usize;
    let inner_height = area.height.saturating_sub(2) as usize;
    app.right_panel_height = inner_height;

    let lines: Vec<Line> = match app.cursor {
        CursorTarget::Workflow(_) => build_installed_preview(app, inner_width),
        CursorTarget::Marketplace(_) | CursorTarget::MarketplaceWorkflow(_, _) => {
            build_marketplace_preview(app, inner_width)
        }
        CursorTarget::Run(_, _) => Vec::new(),
    };

    let auto_bottom = lines.len().saturating_sub(inner_height);
    // Clamp app state so pressing ↑ always has a visible effect after scrolling down.
    app.right_scroll = app.right_scroll.min(auto_bottom);
    let scroll_offset = if is_focused { app.right_scroll } else { 0 };

    let visible = with_scroll_indicators(lines, scroll_offset, inner_height);
    let block = if is_focused {
        panel_focused("Definition")
    } else {
        panel("Definition")
    };
    let para = Paragraph::new(visible).block(block);
    f.render_widget(para, area);
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
    if let Some((m, w)) = app.selected_marketplace_workflow() {
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

        build_workflow_preview_lines(
            PreviewSource::Marketplace {
                marketplace_name: &m.name,
                installed,
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
    } else if app.selected_marketplace().is_some() {
        Vec::new()
    } else {
        vec![Line::from(Span::styled(
            "No marketplace selected".to_string(),
            dim,
        ))]
    }
}

fn render_consumed_triggers(f: &mut Frame, app: &mut App, area: Rect, is_focused: bool) {
    let triggers = app.selected_consumed_triggers();
    let block = if is_focused {
        panel_focused("Consumed Triggers")
    } else {
        panel("Consumed Triggers")
    };

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
                if is_focused && i == app.right_cursor {
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
    let inner_height = area.height.saturating_sub(2) as usize;
    let scroll_offset = if triggers.is_empty() || !is_focused {
        0
    } else {
        app.right_cursor
            .saturating_sub(inner_height.saturating_sub(1)) as u16
    };

    let para = Paragraph::new(lines)
        .block(block)
        .scroll((scroll_offset, 0));

    f.render_widget(para, area);
}

/// Returns the keybinding hints this panel contributes to the status bar.
pub fn right_panel_hints(app: &App) -> Vec<PanelHint> {
    match app.right_panel_content {
        RightPanelContent::Contextual => vec![
            PanelHint::new("[↑↓]", "Scroll"),
            PanelHint::new("[Home/End]", "Top/Bottom"),
        ],
        RightPanelContent::ConsumedTriggers => vec![
            PanelHint::new("[↑↓]", "Scroll"),
            PanelHint::new("[Del]", "Delete trigger"),
            PanelHint::new("[Esc]", "Close"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use otter_core::types::LogEntry;
    use std::path::PathBuf;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    fn make_app() -> App {
        let (tx, _rx) = mpsc::channel(32);
        App::new(
            tx,
            PathBuf::from("/tmp/otter-tui-test"),
            PathBuf::from("/tmp/otter-tui-test-config"),
        )
    }

    fn make_log(step_index: usize, step_type: &str, stdout: &str) -> LogEntry {
        LogEntry {
            run_id: Uuid::nil(),
            iteration: 0,
            step_index,
            step_type: step_type.to_string(),
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: None,
            accepted: None,
            feedback: None,
            timestamp: Utc::now(),
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn is_progress_line(line: &Line) -> bool {
        // Progress lines start with "  │ "
        line_text(line).starts_with("  \u{2502} ")
    }

    #[test]
    fn progress_appears_before_completion_log_not_after() {
        // GIVEN an agent step with two log entries (start + completion) and progress in between
        let dim = Style::default();
        let red = Style::default();
        let logs = vec![
            make_log(usize::MAX, "run_start", "Run started"),
            make_log(0, "agent", "Running claude agent..."),
            make_log(0, "agent", "Final output"),
            make_log(1, "shell", "Next step"),
        ];
        let progress = vec![
            (0, ProgressChunk::Status("Thinking...".to_string())),
            (0, ProgressChunk::Status("Using tool: Read".to_string())),
        ];

        // WHEN
        let lines = build_log_lines(&logs, &progress, 80, dim, red);

        // THEN progress lines appear after "Running claude agent..." and before "Final output"
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let idx_start = texts
            .iter()
            .position(|t| t.contains("Running claude agent..."))
            .unwrap();
        let idx_prog0 = texts
            .iter()
            .position(|t| t.contains("Thinking..."))
            .unwrap();
        let idx_prog1 = texts
            .iter()
            .position(|t| t.contains("Using tool: Read"))
            .unwrap();
        let idx_final = texts
            .iter()
            .position(|t| t.contains("Final output"))
            .unwrap();
        let idx_next = texts.iter().position(|t| t.contains("Next step")).unwrap();

        assert!(
            idx_start < idx_prog0,
            "progress must come after agent start"
        );
        assert!(idx_prog0 < idx_prog1, "progress chunks preserve order");
        assert!(
            idx_prog1 < idx_final,
            "progress must come before final output"
        );
        assert!(idx_final < idx_next, "final output before next step");
    }

    #[test]
    fn progress_appears_after_start_log_when_step_in_flight() {
        // GIVEN an agent step with only its start log (not yet completed) and live progress
        let dim = Style::default();
        let red = Style::default();
        let logs = vec![
            make_log(usize::MAX, "run_start", "Run started"),
            make_log(0, "agent", "Running claude agent..."),
        ];
        let progress = vec![(0, ProgressChunk::Status("Thinking...".to_string()))];

        // WHEN
        let lines = build_log_lines(&logs, &progress, 80, dim, red);

        // THEN progress appears after the start log (in-flight position)
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let idx_start = texts
            .iter()
            .position(|t| t.contains("Running claude agent..."))
            .unwrap();
        let idx_prog = texts
            .iter()
            .position(|t| t.contains("Thinking..."))
            .unwrap();

        assert!(idx_start < idx_prog);
        assert!(is_progress_line(&lines[idx_prog]));
    }

    #[test]
    fn long_progress_chunk_wraps_to_multiple_lines() {
        // GIVEN a progress chunk whose text exceeds the panel width
        let dim = Style::default();
        let red = Style::default();
        // inner_width=20, prefix="  │ "=4 chars → text_width=16
        let long_text = "a".repeat(40);
        let chunk = ProgressChunk::Status(long_text.clone());

        // WHEN
        let lines = render_progress_lines(&chunk, 20, dim, red);

        // THEN multiple lines are returned, each with the progress prefix
        assert!(
            lines.len() > 1,
            "expected wrapping but got {} line(s)",
            lines.len()
        );
        for line in &lines {
            assert!(
                is_progress_line(line),
                "each wrapped line must have the progress prefix"
            );
        }
        // All characters of the original text are preserved across lines
        let total_text: String = lines
            .iter()
            .map(|l| line_text(l).trim_start_matches("  │ ").to_string())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(total_text, long_text);
    }

    #[test]
    fn multiline_progress_chunk_renders_each_line_with_prefix() {
        // GIVEN a progress chunk with embedded newlines
        let dim = Style::default();
        let red = Style::default();
        let chunk = ProgressChunk::Stdout("line one\nline two".to_string());

        // WHEN
        let lines = render_progress_lines(&chunk, 80, dim, red);

        // THEN two lines, both with prefix
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).contains("line one"));
        assert!(line_text(&lines[1]).contains("line two"));
        assert!(is_progress_line(&lines[0]));
        assert!(is_progress_line(&lines[1]));
    }

    #[test]
    fn scroll_indicators_no_truncation() {
        // GIVEN lines that fit exactly in the viewport
        let lines: Vec<Line> = (0..3).map(|i| Line::from(format!("line {i}"))).collect();
        // WHEN no overflow
        let visible = with_scroll_indicators(lines, 0, 3);
        // THEN all lines returned unmodified
        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0].spans[0].content, "line 0");
    }

    #[test]
    fn scroll_indicators_above_only() {
        // GIVEN 5 lines, scrolled to show lines 2-4 (2 above)
        let lines: Vec<Line> = (0..5).map(|i| Line::from(format!("line {i}"))).collect();
        // WHEN 2 above, 0 below
        let visible = with_scroll_indicators(lines, 2, 3);
        // THEN first line replaced with "above" indicator
        assert_eq!(visible.len(), 3);
        assert!(visible[0].spans[0].content.contains("2 more ↑"));
        assert_eq!(visible[2].spans[0].content, "line 4");
    }

    #[test]
    fn scroll_indicators_below_only() {
        // GIVEN 5 lines, showing first 3 (2 below)
        let lines: Vec<Line> = (0..5).map(|i| Line::from(format!("line {i}"))).collect();
        // WHEN 0 above, 2 below
        let visible = with_scroll_indicators(lines, 0, 3);
        // THEN last line replaced with "below" indicator
        assert_eq!(visible.len(), 3);
        assert_eq!(visible[0].spans[0].content, "line 0");
        assert!(visible[2].spans[0].content.contains("2 more ↓"));
    }

    #[test]
    fn scroll_indicators_both_sides() {
        // GIVEN 7 lines, showing middle 3 (2 above, 2 below)
        let lines: Vec<Line> = (0..7).map(|i| Line::from(format!("line {i}"))).collect();
        // WHEN 2 above, 2 below
        let visible = with_scroll_indicators(lines, 2, 3);
        // THEN both first and last lines are indicators
        assert!(visible[0].spans[0].content.contains("2 more ↑"));
        assert!(visible[2].spans[0].content.contains("2 more ↓"));
    }

    #[test]
    fn right_panel_hints_consumed_triggers_shows_delete() {
        // GIVEN consumed triggers panel content
        let mut app = make_app();
        app.right_panel_content = RightPanelContent::ConsumedTriggers;

        // WHEN
        let hints = right_panel_hints(&app);

        // THEN scroll, delete, and close hints
        assert_eq!(hints.len(), 3);
        assert_eq!(hints[0].key, "[↑↓]");
        assert_eq!(hints[1].key, "[Del]");
        assert_eq!(hints[2].key, "[Esc]");
    }

    #[test]
    fn short_message_produces_single_header_line() {
        let lines = format_log_entry("06:00:00", "agent", "hello", 80);
        assert_eq!(
            lines,
            vec![WrappedLogLine::Header {
                time: "06:00:00".into(),
                step_type: "agent".into(),
                text: "hello".into()
            },]
        );
    }

    #[test]
    fn long_message_wraps_to_continuation() {
        // panel_width=40, prefix "[06:00:00] agent: " = 18 chars → first_width=22, cont_width=38
        // 30-char message: first 22 go in the header, remaining 8 re-split at 38 → one continuation
        let msg = "a".repeat(30);
        let lines = format_log_entry("06:00:00", "agent", &msg, 40);
        assert_eq!(lines.len(), 2);
        assert!(matches!(&lines[0], WrappedLogLine::Header { text, .. } if text.len() == 22));
        assert!(matches!(&lines[1], WrappedLogLine::Continuation { text } if text.len() == 8));
    }

    #[test]
    fn long_message_continuation_uses_full_width() {
        // panel_width=40, first_width=22, cont_width=38
        // 80-char message: header gets 22, remaining 58 chars → ceil(58/38) = 2 continuations
        let msg = "a".repeat(80);
        let lines = format_log_entry("06:00:00", "agent", &msg, 40);
        assert_eq!(lines.len(), 3);
        assert!(matches!(&lines[0], WrappedLogLine::Header { text, .. } if text.len() == 22));
        assert!(matches!(&lines[1], WrappedLogLine::Continuation { text } if text.len() == 38));
        assert!(matches!(&lines[2], WrappedLogLine::Continuation { text } if text.len() == 20));
    }

    #[test]
    fn multiline_body_each_line_can_wrap() {
        let msg = "line one\nline two";
        let lines = format_log_entry("06:00:00", "shell", msg, 80);
        assert_eq!(lines.len(), 2);
        assert!(matches!(&lines[0], WrappedLogLine::Header { text, .. } if text == "line one"));
        assert!(matches!(&lines[1], WrappedLogLine::Continuation { text } if text == "line two"));
    }

    #[test]
    fn empty_text_produces_header_with_empty_text() {
        let lines = format_log_entry("06:00:00", "agent", "", 80);
        assert_eq!(
            lines,
            vec![WrappedLogLine::Header {
                time: "06:00:00".into(),
                step_type: "agent".into(),
                text: String::new()
            },]
        );
    }

    #[test]
    fn carriage_returns_stripped() {
        let lines = format_log_entry("06:00:00", "shell", "ok\r", 80);
        assert!(matches!(&lines[0], WrappedLogLine::Header { text, .. } if text == "ok"));
    }

    fn parse_def(toml: &str) -> WorkflowDef {
        toml::from_str(toml).expect("parses")
    }

    fn collect_text(lines: &[Line]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn preview_includes_install_command_and_requires() {
        // GIVEN a workflow with a [require] entry
        let def = parse_def(
            r#"
name = "polling-simple"
type = "triggered"
schema = 1

[require.JIRA_PAT]
description = "Jira PAT"
sensitive = true

[trigger]
type = "manual"

[[steps]]
type = "shell"
command = ["echo", "hi"]
"#,
        );

        // WHEN
        let lines = build_workflow_preview_lines(
            PreviewSource::Marketplace {
                marketplace_name: "acme",
                installed: false,
                pkg_dir_missing: false,
            },
            "polling-simple",
            Some("1.0.0"),
            None,
            Some(&def),
            None,
            80,
            |_| None,
        );

        // THEN it shows the install command and the require entry
        let text = collect_text(&lines);
        assert!(text.contains("otter workflow install polling-simple@acme"));
        assert!(text.contains("JIRA_PAT"));
        assert!(text.contains("(secret)"));
    }

    #[test]
    fn preview_inlines_message_file_contents() {
        // GIVEN an agent step that references a message_file
        let def = parse_def(
            r#"
name = "wf"
type = "looping"
schema = 1

[[steps]]
type = "agent"
provider = "claude"
message_file = "prompts/follow-up.md"
"#,
        );

        // WHEN read_file returns content for that path
        let lines = build_workflow_preview_lines(
            PreviewSource::Marketplace {
                marketplace_name: "acme",
                installed: false,
                pkg_dir_missing: false,
            },
            "wf",
            None,
            None,
            Some(&def),
            None,
            80,
            |rel| {
                if rel == "prompts/follow-up.md" {
                    Some("First line\nSecond line".to_string())
                } else {
                    None
                }
            },
        );

        // THEN the file contents are inlined under the step
        let text = collect_text(&lines);
        assert!(text.contains("prompts/follow-up.md"));
        assert!(text.contains("First line"));
        assert!(text.contains("Second line"));
    }

    #[test]
    fn preview_marks_missing_message_file_gracefully() {
        // GIVEN a message_file that the resolver returns None for
        let def = parse_def(
            r#"
name = "wf"
type = "looping"
schema = 1

[[steps]]
type = "agent"
provider = "claude"
message_file = "missing.md"
"#,
        );

        // WHEN
        let lines = build_workflow_preview_lines(
            PreviewSource::Marketplace {
                marketplace_name: "acme",
                installed: false,
                pkg_dir_missing: false,
            },
            "wf",
            None,
            None,
            Some(&def),
            None,
            80,
            |_| None,
        );

        // THEN a "not found" line is rendered instead of crashing
        let text = collect_text(&lines);
        assert!(text.contains("missing.md"));
        assert!(text.contains("not found"));
    }

    #[test]
    fn preview_handles_missing_clone() {
        // GIVEN pkg_dir_missing = true (clone removed) and no def parsed
        let lines = build_workflow_preview_lines(
            PreviewSource::Marketplace {
                marketplace_name: "acme",
                installed: false,
                pkg_dir_missing: true,
            },
            "wf",
            Some("1.0.0"),
            None,
            None,
            None,
            80,
            |_| None,
        );

        // THEN it shows install command + clone-missing hint, no crash
        let text = collect_text(&lines);
        assert!(text.contains("otter workflow install wf@acme"));
        assert!(text.contains("marketplace clone missing"));
    }

    #[test]
    fn installed_preview_shows_origin_and_update_section_when_outdated() {
        // GIVEN an installed workflow from marketplace 'acme' with a newer version upstream
        let def = parse_def(
            r#"
name = "jira-sync"
type = "looping"
schema = 1

[[steps]]
type = "shell"
command = ["echo", "hi"]
"#,
        );
        let origin = MarketplaceOrigin {
            marketplace: "acme".to_string(),
            dangling: false,
        };

        // WHEN
        let lines = build_workflow_preview_lines(
            PreviewSource::Installed {
                origin: Some(&origin),
                update_available: Some("1.2.0"),
            },
            "jira-sync",
            Some("1.0.0"),
            None,
            Some(&def),
            None,
            80,
            |_| None,
        );

        // THEN the header notes the origin, and an UPDATE AVAILABLE section is shown
        let text = collect_text(&lines);
        assert!(text.contains("from acme"));
        assert!(text.contains("UPDATE AVAILABLE"));
        assert!(text.contains("→ v1.2.0"));
        assert!(text.contains("otter workflow install jira-sync"));
        // No marketplace-style INSTALL command with @marketplace syntax
        assert!(!text.contains("install jira-sync@acme"));
    }

    #[test]
    fn installed_preview_omits_update_section_when_current() {
        // GIVEN an installed workflow with origin but no update available
        let def = parse_def(
            r#"
name = "wf"
type = "looping"
schema = 1

[[steps]]
type = "shell"
command = ["echo", "hi"]
"#,
        );
        let origin = MarketplaceOrigin {
            marketplace: "acme".to_string(),
            dangling: false,
        };

        // WHEN
        let lines = build_workflow_preview_lines(
            PreviewSource::Installed {
                origin: Some(&origin),
                update_available: None,
            },
            "wf",
            None,
            None,
            Some(&def),
            None,
            80,
            |_| None,
        );

        // THEN no INSTALL/UPDATE section is rendered
        let text = collect_text(&lines);
        assert!(text.contains("from acme"));
        assert!(!text.contains("UPDATE AVAILABLE"));
        assert!(!text.contains("INSTALL"));
    }
}
