use ratatui::{style::Style, text::Line};

use otter_core::types::{MarketplaceOrigin, ProgressChunk, WorkflowDef};

use crate::app::{App, CursorTarget, Selection, WorkflowEntry};

use crate::panel::Panel;
use crate::right_panel::{
    build_log_lines, build_marketplace_preview, build_workflow_preview_lines, format_log_entry,
    render_progress_lines, with_scroll_indicators, DefinitionView, PreviewSource, RenderMode,
    RightPanel, RightPanelContent, WrappedLogLine,
};
use chrono::Utc;
use otter_core::types::{LogEntry, MarketplaceStatus, WorkflowRun, WorkflowState, WorkflowType};
use std::path::PathBuf;
use tokio::sync::mpsc;
use uuid::Uuid;

struct SelectionFixture {
    entry: WorkflowEntry,
    run: WorkflowRun,
    marketplace: MarketplaceStatus,
}

impl SelectionFixture {
    fn new() -> Self {
        Self {
            entry: WorkflowEntry {
                name: "wf".to_string(),
                kind: WorkflowType::Looping,
                state: WorkflowState::Dormant,
                runs: Vec::new(),
                trigger: None,
                toml_content: None,
                autostart: false,
                update_available: None,
                origin: None,
            },
            run: WorkflowRun::new("wf".to_string()),
            marketplace: MarketplaceStatus {
                name: "acme".to_string(),
                url: "https://example.com/acme".to_string(),
                workflow_count: 0,
                last_fetched_at: None,
                workflows: Vec::new(),
            },
        }
    }

    fn workflow_selection(&self) -> Selection<'_> {
        Selection::Workflow(&self.entry)
    }

    fn run_selection(&self) -> Selection<'_> {
        Selection::Run(&self.entry, &self.run)
    }

    fn marketplace_selection(&self) -> Selection<'_> {
        Selection::Marketplace(&self.marketplace)
    }
}

#[test]
fn render_mode_routes_run_cursor_to_logs() {
    let p = RightPanel::default();
    let fx = SelectionFixture::new();
    assert_eq!(p.render_mode(fx.run_selection()), RenderMode::Logs);
}

#[test]
fn render_mode_routes_non_run_cursors_to_definition() {
    let p = RightPanel::default();
    let fx = SelectionFixture::new();
    assert_eq!(
        p.render_mode(fx.workflow_selection()),
        RenderMode::Definition(DefinitionView::Preview)
    );
    assert_eq!(
        p.render_mode(fx.marketplace_selection()),
        RenderMode::Definition(DefinitionView::Preview)
    );
}

#[test]
fn render_mode_consumed_triggers_overrides_cursor() {
    let mut p = RightPanel::default();
    p.content = RightPanelContent::ConsumedTriggers;
    let fx = SelectionFixture::new();
    assert_eq!(
        p.render_mode(fx.run_selection()),
        RenderMode::ConsumedTriggers
    );
    assert_eq!(
        p.render_mode(fx.workflow_selection()),
        RenderMode::ConsumedTriggers
    );
}

fn workflow_mode() -> RenderMode {
    RenderMode::Definition(DefinitionView::Preview)
}

fn run_mode() -> RenderMode {
    RenderMode::Logs
}

fn consumed_mode() -> RenderMode {
    RenderMode::ConsumedTriggers
}

#[test]
fn reset_returns_to_contextual_at_scroll_top() {
    // GIVEN a panel scrolled away from the top, showing consumed triggers
    let mut p = RightPanel::default();
    p.scroll = 10;
    p.content = RightPanelContent::ConsumedTriggers;

    // WHEN reset
    p.reset();

    // THEN content reverts to contextual and scroll snaps to top
    assert_eq!(p.content, RightPanelContent::Contextual);
    assert_eq!(p.scroll, 0);
}

#[test]
fn show_consumed_triggers_resets_cursor() {
    let mut p = RightPanel::default();
    p.cursor = 5;

    p.show_consumed_triggers();

    assert_eq!(p.content, RightPanelContent::ConsumedTriggers);
    assert_eq!(p.cursor, 0);
}

#[test]
fn toggle_definition_view_flips_between_preview_and_raw_and_resets_scroll() {
    // GIVEN a panel showing the structured preview, scrolled away from the top
    let mut p = RightPanel::default();
    assert_eq!(p.definition_view, DefinitionView::Preview);
    p.scroll = 12;

    // WHEN toggled
    p.toggle_definition_view(workflow_mode());

    // THEN it flips to raw and scroll returns to the top
    assert_eq!(p.definition_view, DefinitionView::Raw);
    assert_eq!(p.scroll, 0);

    // AND a second toggle flips back
    p.toggle_definition_view(workflow_mode());
    assert_eq!(p.definition_view, DefinitionView::Preview);
}

#[test]
fn toggle_definition_view_is_no_op_when_showing_logs() {
    // GIVEN cursor on a run (logs mode) with scroll set
    let mut p = RightPanel::default();
    p.scroll = 5;
    let fx = SelectionFixture::new();
    assert_eq!(p.render_mode(fx.run_selection()), RenderMode::Logs);

    // WHEN toggle is invoked
    p.toggle_definition_view(run_mode());

    // THEN preference is unchanged and scroll is preserved
    assert_eq!(p.definition_view, DefinitionView::Preview);
    assert_eq!(p.scroll, 5);
}

#[test]
fn move_consumed_cursor_wraps_in_both_directions() {
    let mut p = RightPanel::default();
    p.content = RightPanelContent::ConsumedTriggers;

    p.move_down(consumed_mode(), 3);
    assert_eq!(p.cursor, 1);
    p.move_down(consumed_mode(), 3);
    p.move_down(consumed_mode(), 3);
    assert_eq!(p.cursor, 0, "wrap forward");

    p.move_up(consumed_mode(), 3);
    assert_eq!(p.cursor, 2, "wrap backward");
}

#[test]
fn move_consumed_cursor_is_noop_on_empty_list() {
    let mut p = RightPanel::default();
    p.content = RightPanelContent::ConsumedTriggers;
    p.move_down(consumed_mode(), 0);
    p.move_up(consumed_mode(), 0);
    assert_eq!(p.cursor, 0);
}

#[test]
fn logs_scroll_is_inverted_relative_to_definition() {
    // Logs are bottom-up: ↑ steps into history (scroll += 1)
    let mut p = RightPanel::default();
    p.move_up(run_mode(), 0);
    assert_eq!(p.scroll, 1);
    p.move_down(run_mode(), 0);
    assert_eq!(p.scroll, 0);

    // Definition is top-down: ↓ steps forward (scroll += 1)
    let mut p = RightPanel::default();
    p.move_down(workflow_mode(), 0);
    assert_eq!(p.scroll, 1);
    p.move_up(workflow_mode(), 0);
    assert_eq!(p.scroll, 0);
}

#[test]
fn page_scroll_uses_panel_height_and_respects_mode_direction() {
    let mut p = RightPanel::default();
    p.height = 20;

    // Definition mode (top-down): page_down increases scroll
    p.page_down(workflow_mode());
    assert_eq!(p.scroll, 20);
    p.page_up(workflow_mode());
    assert_eq!(p.scroll, 0);

    // Logs mode (bottom-up): page_down decreases scroll
    p.scroll = 30;
    p.page_down(run_mode());
    assert_eq!(p.scroll, 10);
    p.page_up(run_mode());
    assert_eq!(p.scroll, 30);
}

#[test]
fn half_page_scroll_uses_half_panel_height_minimum_one() {
    let mut p = RightPanel::default();
    // Height = 0 → half = max(0, 1) = 1
    p.half_page_down(workflow_mode());
    assert_eq!(p.scroll, 1);

    p.height = 20;
    p.half_page_down(workflow_mode());
    assert_eq!(p.scroll, 11);
    p.half_page_up(workflow_mode());
    assert_eq!(p.scroll, 1);
}

#[test]
fn scroll_top_and_bottom_invert_between_modes() {
    // Definition mode: top = 0, bottom = MAX
    let mut p = RightPanel::default();
    p.scroll_top(workflow_mode());
    assert_eq!(p.scroll, 0);
    p.scroll_bottom(workflow_mode());
    assert_eq!(p.scroll, usize::MAX);

    // Logs mode: top of history = MAX (renderer clamps to auto_bottom),
    // bottom = 0 (latest)
    let mut p = RightPanel::default();
    p.scroll_top(run_mode());
    assert_eq!(p.scroll, usize::MAX);
    p.scroll_bottom(run_mode());
    assert_eq!(p.scroll, 0);
}

#[test]
fn clamp_consumed_cursor_resets_to_zero_when_empty() {
    let mut p = RightPanel::default();
    p.cursor = 5;
    p.clamp_consumed_cursor(0);
    assert_eq!(p.cursor, 0);
}

#[test]
fn clamp_consumed_cursor_caps_at_last_index() {
    let mut p = RightPanel::default();
    p.cursor = 5;
    p.clamp_consumed_cursor(3);
    assert_eq!(p.cursor, 2);
}

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

#[test]
fn marketplace_preview_shows_stats_when_marketplace_selected() {
    // GIVEN a marketplace with two workflows, one of them installed
    let mut app = make_app();
    app.marketplaces = vec![otter_core::types::MarketplaceStatus {
        name: "acme".to_string(),
        url: "/home/user/acme".to_string(),
        workflow_count: 2,
        last_fetched_at: None,
        workflows: vec![
            otter_core::types::MarketplaceWorkflowEntry {
                name: "installed-wf".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                path: "installed-wf".to_string(),
            },
            otter_core::types::MarketplaceWorkflowEntry {
                name: "fresh-wf".to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                path: "fresh-wf".to_string(),
            },
        ],
    }];
    app.workflows.push(crate::app::WorkflowEntry {
        name: "installed-wf".to_string(),
        kind: otter_core::types::WorkflowType::Looping,
        state: otter_core::types::WorkflowState::Dormant,
        runs: Vec::new(),
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });
    app.ui.cursor = CursorTarget::Marketplace(0);

    // WHEN the marketplace row (not a workflow) is selected
    let lines = build_marketplace_preview(&app, 80);

    // THEN the panel shows the marketplace stats
    let text: String = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
    assert!(text.contains("acme"));
    assert!(text.contains("/home/user/acme"));
    assert!(text.contains("2 published · 1 installed"));
    assert!(text.contains("never"));
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
    let app = make_app();
    let mut panel = RightPanel::default();
    panel.content = RightPanelContent::ConsumedTriggers;

    // WHEN
    let hints = panel.hints(&app);

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
fn marketplace_preview_includes_requires_but_not_install_command() {
    // GIVEN a marketplace workflow with a [require] entry
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

    // THEN it shows the require entry but not the install command
    // (the install command is shown in the status bar instead)
    let text = collect_text(&lines);
    assert!(!text.contains("otter workflow install"));
    assert!(!text.contains("INSTALL"));
    assert!(text.contains("JIRA_PAT"));
    assert!(text.contains("(secret)"));
}

#[test]
fn installed_preview_omits_requires_section() {
    // GIVEN an installed workflow with a [require] entry
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
        PreviewSource::Installed {
            origin: None,
            update_available: None,
        },
        "polling-simple",
        Some("1.0.0"),
        None,
        Some(&def),
        None,
        80,
        |_| None,
    );

    // THEN the REQUIRES section is omitted (values are already configured)
    let text = collect_text(&lines);
    assert!(!text.contains("REQUIRES"));
    assert!(!text.contains("JIRA_PAT"));
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

    // THEN the clone-missing hint is shown, no crash
    let text = collect_text(&lines);
    assert!(!text.contains("INSTALL"));
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
