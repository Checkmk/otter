use super::*;
use crate::panels::PanelSet;
use otter_core::types::{DaemonEvent, WorkflowState, WorkflowType};
use tokio::sync::mpsc;

fn make_test_app() -> App {
    // Use a fresh tempdir for config so [[FirstLaunchState]] starts clean
    // each test run; leak the TempDir to keep the path alive for the test.
    let cfg = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let data = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let (tx, _rx) = mpsc::channel(32);
    App::new(tx, data.path().to_path_buf(), cfg.path().to_path_buf())
}

fn dispatch(app: &mut App, panels: &mut PanelSet, ev: DaemonEvent) {
    app.handle_daemon_event(ev, panels);
}

#[test]
fn cursor_navigation_moves_through_workflows_only_when_collapsed() {
    let mut app = make_test_app();
    let panels = PanelSet::default();

    // Add two workflows
    app.workflows.push(WorkflowEntry {
        name: "wf1".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });
    app.workflows.push(WorkflowEntry {
        name: "wf2".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Start at first workflow
    app.ui.cursor = CursorTarget::Workflow(0);

    // Move down
    app.move_cursor_down(&panels);
    assert_eq!(app.ui.cursor, CursorTarget::Workflow(1));

    // Move down again (wraps to first)
    app.move_cursor_down(&panels);
    assert_eq!(app.ui.cursor, CursorTarget::Workflow(0));

    // Move up (wraps to last)
    app.move_cursor_up(&panels);
    assert_eq!(app.ui.cursor, CursorTarget::Workflow(1));
}

#[test]
fn cursor_navigation_includes_runs_when_expanded() {
    use chrono::Duration;

    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    // Add workflow with runs
    let run1 = WorkflowRun::new("wf1".to_string());
    let mut run2 = WorkflowRun::new("wf1".to_string());
    run2.started_at = run1.started_at + Duration::seconds(1);

    app.workflows.push(WorkflowEntry {
        name: "wf1".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![run2.clone(), run1.clone()], // newest first
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Navigate through expanded workflow
    // Flat list should be: Workflow(0), Run(0, 0), Run(0, 1)
    panels.runs.toggle("wf1");
    app.ui.cursor = CursorTarget::Workflow(0);

    app.move_cursor_down(&panels);
    assert_eq!(app.ui.cursor, CursorTarget::Run(0, 0));

    app.move_cursor_down(&panels);
    assert_eq!(app.ui.cursor, CursorTarget::Run(0, 1));

    // Move down from last wraps to first
    app.move_cursor_down(&panels);
    assert_eq!(app.ui.cursor, CursorTarget::Workflow(0));

    // Move up from first wraps to last
    app.move_cursor_up(&panels);
    assert_eq!(app.ui.cursor, CursorTarget::Run(0, 1));

    app.move_cursor_up(&panels);
    assert_eq!(app.ui.cursor, CursorTarget::Run(0, 0));
}

#[test]
fn toggle_expanded_expands_and_collapses_workflow() {
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    let run = WorkflowRun::new("wf".to_string());
    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![run],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Select the workflow
    app.ui.cursor = CursorTarget::Workflow(0);

    // Toggle to expand
    panels.runs.toggle("wf");
    assert!(panels.runs.is_expanded("wf"));

    // Toggle to collapse
    panels.runs.toggle("wf");
    assert!(!panels.runs.is_expanded("wf"));
}

#[test]
fn selected_run_id_reflects_cursor_target() {
    let mut app = make_test_app();

    let run = WorkflowRun::new("wf".to_string());
    let run_id = run.id;

    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![run],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    app.ui.cursor = CursorTarget::Workflow(0);
    assert!(app.selected_run_id().is_none());

    app.ui.cursor = CursorTarget::Run(0, 0);
    assert_eq!(app.selected_run_id(), Some(run_id));
}

#[test]
fn handle_daemon_event_run_deleted_removes_run_from_workflows() {
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    let run1 = WorkflowRun::new("wf".to_string());
    let run2 = WorkflowRun::new("wf".to_string());
    let run1_id = run1.id;

    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![run2.clone(), run1.clone()],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Handle RunDeleted event
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::RunDeleted { run_id: run1_id },
    );

    // run1 should be removed
    assert_eq!(app.workflows[0].runs.len(), 1);
    assert_eq!(app.workflows[0].runs[0].id, run2.id);
}

#[test]
fn handle_daemon_event_run_updated_inserts_and_sorts_by_started_at() {
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Add runs in non-chronological order
    let mut run1 = WorkflowRun::new("wf".to_string());
    let mut run2 = WorkflowRun::new("wf".to_string());

    run1.started_at = chrono::Utc::now();
    run2.started_at = run1.started_at + chrono::Duration::seconds(10);

    // Add run2 first
    dispatch(&mut app, &mut panels, DaemonEvent::RunUpdated(run2.clone()));
    assert_eq!(app.workflows[0].runs.len(), 1);

    // Add run1
    dispatch(&mut app, &mut panels, DaemonEvent::RunUpdated(run1.clone()));
    assert_eq!(app.workflows[0].runs.len(), 2);

    // Verify sorting: newest first
    assert_eq!(app.workflows[0].runs[0].id, run2.id);
    assert_eq!(app.workflows[0].runs[1].id, run1.id);
}

#[test]
fn deleting_selected_run_snaps_cursor_to_previous_run() {
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    let run1 = WorkflowRun::new("wf".to_string());
    let run2 = WorkflowRun::new("wf".to_string());
    let run1_id = run1.id;
    let run2_id = run2.id;

    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![run2, run1],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Cursor on the last run (index 1)
    panels.runs.toggle("wf");
    app.ui.cursor = CursorTarget::Run(0, 1);
    assert_eq!(app.selected_run_id(), Some(run1_id));

    // Delete the selected run
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::RunDeleted { run_id: run1_id },
    );

    // Cursor should snap to the previous run (index 0), not jump to another workflow
    assert_eq!(app.ui.cursor, CursorTarget::Run(0, 0));
    assert_eq!(app.selected_run_id(), Some(run2_id));
}

#[test]
fn deleting_run_does_not_jump_to_another_workflow() {
    use chrono::Duration;

    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    let run_a = WorkflowRun::new("wf-a".to_string());
    let run_b0 = WorkflowRun::new("wf-b".to_string());
    let mut run_b1 = WorkflowRun::new("wf-b".to_string());
    run_b1.started_at = run_b0.started_at + Duration::seconds(1);
    let run_b1_id = run_b1.id;

    // wf-a has one run; wf-b has two runs (b1 newest first)
    app.workflows.push(WorkflowEntry {
        name: "wf-a".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![run_a],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });
    app.workflows.push(WorkflowEntry {
        name: "wf-b".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![run_b1, run_b0.clone()],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Cursor on the second run of wf-b (the older one)
    panels.runs.toggle("wf-a");
    panels.runs.toggle("wf-b");
    app.ui.cursor = CursorTarget::Run(1, 1);

    // Delete that run
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::RunDeleted { run_id: run_b0.id },
    );

    // Should snap to Run(1, 0) — the first run of wf-b — NOT to wf-a or run_b1_id accidentally
    assert_eq!(app.ui.cursor, CursorTarget::Run(1, 0));
    assert_eq!(app.selected_run_id(), Some(run_b1_id));
}

#[test]
fn deleting_last_run_from_expanded_workflow_snaps_cursor_to_workflow() {
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    let run = WorkflowRun::new("wf".to_string());
    let run_id = run.id;

    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![run],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Cursor on the only run
    app.ui.cursor = CursorTarget::Run(0, 0);
    assert_eq!(app.selected_run_id(), Some(run_id));

    // Delete the run
    dispatch(&mut app, &mut panels, DaemonEvent::RunDeleted { run_id });

    // Cursor should snap to the workflow row
    assert_eq!(app.ui.cursor, CursorTarget::Workflow(0));
    assert_eq!(app.selected_run_id(), None);
}

#[test]
fn handle_daemon_event_run_updated_moves_cursor_to_new_run_when_just_started() {
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    app.ui.cursor = CursorTarget::Workflow(0);
    panels.runs.start_selected(&mut app);

    let run = WorkflowRun::new("wf".to_string());
    dispatch(&mut app, &mut panels, DaemonEvent::RunUpdated(run));

    // Cursor should move to the new run, not stay on the workflow row
    assert_eq!(app.ui.cursor, CursorTarget::Run(0, 0));
}

#[test]
fn handle_daemon_event_run_updated_does_not_auto_expand_without_start() {
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Add a new run without starting the workflow
    let run = WorkflowRun::new("wf".to_string());
    dispatch(&mut app, &mut panels, DaemonEvent::RunUpdated(run));

    // Workflow should NOT be expanded
    assert!(!panels.runs.is_expanded("wf"));
}

#[test]
fn handle_daemon_event_run_updated_expands_workflow_when_just_started() {
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // Start the workflow
    app.ui.cursor = CursorTarget::Workflow(0);
    panels.runs.start_selected(&mut app);

    // Add a new run
    let run = WorkflowRun::new("wf".to_string());
    dispatch(&mut app, &mut panels, DaemonEvent::RunUpdated(run));

    // Workflow should be expanded
    assert!(panels.runs.is_expanded("wf"));
}

#[test]
fn feedback_processing_set_on_feedback_and_cleared_on_checkpoint_repending() {
    use otter_core::types::{CheckpointAction, RunStatus};

    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    let mut run = WorkflowRun::new("wf".to_string());
    run.status = RunStatus::WaitingCheckpoint;
    let run_id = run.id;

    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Running,
        runs: vec![run],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });
    app.ui.cursor = CursorTarget::Run(0, 0);

    // Simulate checkpoint pending
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::CheckpointPending {
            run_id,
            step_index: 0,
            message: "Review?".to_string(),
            feedback_available: true,
        },
    );
    assert!(app.pending_checkpoints.contains_key(&run_id));
    assert!(!app.pending_checkpoints[&run_id].processing);

    // WHEN user submits feedback
    app.respond_checkpoint(CheckpointAction::Feedback("fix this".to_string()));

    // THEN processing is set, checkpoint still pending
    assert!(app.pending_checkpoints[&run_id].processing);

    // WHEN agent finishes and checkpoint re-presents
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::CheckpointPending {
            run_id,
            step_index: 0,
            message: "Review?".to_string(),
            feedback_available: true,
        },
    );

    // THEN processing is cleared
    assert!(!app.pending_checkpoints[&run_id].processing);
}

fn snap(name: &str, toml_content: Option<&str>, enabled: bool) -> WorkflowStatus {
    WorkflowStatus {
        name: name.to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        trigger: None,
        toml_content: toml_content.map(str::to_string),
        enabled,
        update_available: None,
        origin: None,
    }
}

#[test]
fn workflows_snapshot_stores_toml_content() {
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::WorkflowsSnapshot(vec![snap(
            "wf",
            Some("name = \"wf\"\ntype = \"looping\"\n"),
            false,
        )]),
    );

    assert_eq!(app.workflows.len(), 1);
    assert_eq!(
        app.workflows[0].toml_content.as_deref(),
        Some("name = \"wf\"\ntype = \"looping\"\n")
    );
}

#[test]
fn step_progress_accumulates_and_persists_after_log() {
    // GIVEN
    let mut app = make_test_app();
    let mut panels = PanelSet::default();
    let run_id = Uuid::new_v4();

    // WHEN progress arrives
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::StepProgress {
            run_id,
            step_index: 0,
            chunk: ProgressChunk::Status("Thinking...".to_string()),
        },
    );
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::StepProgress {
            run_id,
            step_index: 0,
            chunk: ProgressChunk::Status("Using tool: Read".to_string()),
        },
    );

    // THEN — progress accumulates
    assert_eq!(app.progress.get(&run_id).unwrap().len(), 2);

    // WHEN a LogAppended arrives for the same run
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::LogAppended(LogEntry {
            run_id,
            iteration: 0,
            step_index: 0,
            step_type: "agent".to_string(),
            stdout: "done".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            accepted: None,
            feedback: None,
            timestamp: Utc::now(),
        }),
    );

    // THEN — progress persists (not cleared)
    assert_eq!(app.progress.get(&run_id).unwrap().len(), 2);
    // AND the log entry was added
    assert_eq!(app.logs.get(&run_id).unwrap().len(), 1);
}

#[test]
fn snapshot_preserves_runs_and_expanded_for_existing_workflow() {
    // GIVEN a workflow with a run and expanded=true
    let mut app = make_test_app();
    let mut panels = PanelSet::default();
    let run = WorkflowRun::new("wf".to_string());
    let run_id = run.id;
    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![run],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // WHEN a snapshot arrives that still contains wf (with a state change and toml)
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::WorkflowsSnapshot(vec![WorkflowStatus {
            name: "wf".to_string(),
            kind: WorkflowType::Looping,
            state: WorkflowState::Running,
            trigger: None,
            toml_content: Some("name = \"wf\"\n".to_string()),
            enabled: true,
            update_available: None,
            origin: None,
        }]),
    );

    // THEN runs are preserved; state, toml, autostart updated. (Expand state
    // lives on RunsPanel and is preserved separately by the panel's `retain`.)
    assert_eq!(app.workflows.len(), 1);
    assert_eq!(app.workflows[0].runs.len(), 1);
    assert_eq!(app.workflows[0].runs[0].id, run_id);
    assert_eq!(app.workflows[0].state, WorkflowState::Running);
    assert_eq!(
        app.workflows[0].toml_content.as_deref(),
        Some("name = \"wf\"\n")
    );
    assert!(app.workflows[0].autostart);
}

#[test]
fn snapshot_removes_workflows_not_in_payload() {
    // GIVEN two workflows
    let mut app = make_test_app();
    let mut panels = PanelSet::default();
    app.workflows.push(WorkflowEntry {
        name: "wf-a".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![WorkflowRun::new("wf-a".to_string())],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });
    app.workflows.push(WorkflowEntry {
        name: "wf-b".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });

    // WHEN a snapshot arrives that only contains wf-b
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::WorkflowsSnapshot(vec![snap("wf-b", None, false)]),
    );

    // THEN wf-a is gone, wf-b remains
    assert_eq!(app.workflows.len(), 1);
    assert_eq!(app.workflows[0].name, "wf-b");
}

#[test]
fn toggle_enable_selected_enables_workflow() {
    // GIVEN a disabled workflow
    let cfg = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(32);
    let mut app = App::new(tx, data.path().into(), cfg.path().into());
    let mut panels = PanelSet::default();
    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });
    app.ui.cursor = CursorTarget::Workflow(0);

    // WHEN toggled
    panels.runs.toggle_autostart_for_selected(&mut app);

    // THEN enabled flips to true, EnableWorkflow sent
    assert!(app.workflows[0].autostart);
    let cmd = rx.try_recv().expect("command sent");
    assert!(matches!(cmd, DaemonCommand::EnableWorkflow { name } if name == "wf"));
}

#[test]
fn toggle_enable_selected_disables_workflow() {
    // GIVEN an enabled workflow
    let cfg = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(32);
    let mut app = App::new(tx, data.path().into(), cfg.path().into());
    let mut panels = PanelSet::default();
    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: true,
        update_available: None,
        origin: None,
    });
    app.ui.cursor = CursorTarget::Workflow(0);

    // WHEN toggled
    panels.runs.toggle_autostart_for_selected(&mut app);

    // THEN enabled flips to false, DisableWorkflow sent
    assert!(!app.workflows[0].autostart);
    let cmd = rx.try_recv().expect("command sent");
    assert!(matches!(cmd, DaemonCommand::DisableWorkflow { name } if name == "wf"));
}

#[test]
fn snapshot_stores_enabled_flag() {
    // GIVEN an app with no workflows
    let mut app = make_test_app();
    let mut panels = PanelSet::default();

    // WHEN a snapshot arrives with enabled=true
    dispatch(
        &mut app,
        &mut panels,
        DaemonEvent::WorkflowsSnapshot(vec![snap("wf", None, true)]),
    );

    // THEN the entry has autostart=true
    assert!(app.workflows[0].autostart);
}

fn make_marketplace(name: &str, workflows: Vec<&str>) -> otter_core::types::MarketplaceStatus {
    otter_core::types::MarketplaceStatus {
        name: name.to_string(),
        url: format!("https://example.com/{name}"),
        workflow_count: workflows.len(),
        last_fetched_at: None,
        workflows: workflows
            .into_iter()
            .map(|n| otter_core::types::MarketplaceWorkflowEntry {
                name: n.to_string(),
                version: Some("1.0.0".to_string()),
                description: None,
                path: format!("workflows/{n}"),
            })
            .collect(),
    }
}

#[test]
fn cursor_flows_from_runs_into_marketplaces() {
    // GIVEN one collapsed workflow and one collapsed marketplace
    let mut app = make_test_app();
    let mut panels = PanelSet::default();
    app.workflows.push(WorkflowEntry {
        name: "wf".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });
    app.apply_marketplaces_snapshot(vec![make_marketplace("acme", vec!["a"])], &mut panels);
    app.ui.cursor = CursorTarget::Workflow(0);

    // WHEN moving down
    app.move_cursor_down(&panels);

    // THEN cursor lands on the marketplace
    assert_eq!(app.ui.cursor, CursorTarget::Marketplace(0));
}

#[test]
fn toggle_expanded_expands_marketplace() {
    // GIVEN a marketplace with workflows, cursor on it
    let mut app = make_test_app();
    let mut panels = PanelSet::default();
    app.apply_marketplaces_snapshot(vec![make_marketplace("acme", vec!["a", "b"])], &mut panels);
    app.ui.cursor = CursorTarget::Marketplace(0);

    // WHEN toggling
    panels.marketplaces.toggle_expanded("acme");

    // THEN it expands and the workflow rows show up in the flat list
    assert!(panels.marketplaces.is_expanded("acme"));
    let flat = app.build_flat_list(&panels);
    assert!(flat.contains(&CursorTarget::MarketplaceWorkflow(0, 0)));
    assert!(flat.contains(&CursorTarget::MarketplaceWorkflow(0, 1)));
}

#[test]
fn apply_marketplaces_snapshot_drops_stale_expand_state() {
    // GIVEN an expanded marketplace
    let mut app = make_test_app();
    let mut panels = PanelSet::default();
    app.apply_marketplaces_snapshot(vec![make_marketplace("acme", vec!["a"])], &mut panels);
    panels.marketplaces.toggle_expanded("acme");
    panels.marketplaces.toggle_expanded("gone");

    // WHEN a new snapshot arrives without 'gone'
    app.apply_marketplaces_snapshot(vec![make_marketplace("acme", vec!["a"])], &mut panels);

    // THEN stale expand state is removed
    assert!(panels.marketplaces.is_expanded("acme"));
    assert!(!panels.marketplaces.is_expanded("gone"));
}

#[test]
fn first_launch_shows_help_modal_on_initial_construction() {
    // GIVEN a fresh config dir (no prior tui-state.toml)
    let cfg = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let (tx, _rx) = mpsc::channel(32);

    // WHEN constructing the app
    let app = App::new(tx, data.path().into(), cfg.path().into());

    // THEN the help modal is open and the queue is empty
    assert!(matches!(app.ui.modal, Some(Modal::Help)));
    assert!(app.ui.first_launch_queue.is_empty());
}

#[test]
fn first_launch_does_not_re_show_help_on_second_construction() {
    // GIVEN a config dir where the user has already seen the help modal
    let cfg = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    {
        let (tx, _rx) = mpsc::channel(32);
        let _first = App::new(tx, data.path().into(), cfg.path().into());
    }

    // WHEN constructing the app again with the same config dir
    let (tx, _rx) = mpsc::channel(32);
    let app = App::new(tx, data.path().into(), cfg.path().into());

    // THEN no modal is auto-opened
    assert!(app.ui.modal.is_none());
    assert!(app.ui.first_launch_queue.is_empty());
}

#[test]
fn dismiss_modal_advances_to_next_first_launch_entry() {
    // GIVEN an app with two queued first-launch modals
    let cfg = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let (tx, _rx) = mpsc::channel(32);
    let mut app = App::new(tx, data.path().into(), cfg.path().into());
    // Reset "help" so we can re-queue and add a second entry behind it
    // simulating the future changelog modal.
    app.ui.modal = None;
    app.ui.first_launch_queue.push_back(Modal::Help);
    app.ui.first_launch_queue.push_back(Modal::Help);
    app.ui.modal = app.ui.first_launch_queue.pop_front();

    // WHEN dismissing the first modal
    app.ui.dismiss_modal();

    // THEN the second queued modal becomes active
    assert!(matches!(app.ui.modal, Some(Modal::Help)));
    assert!(app.ui.first_launch_queue.is_empty());

    // AND dismissing again closes everything
    app.ui.dismiss_modal();
    assert!(app.ui.modal.is_none());
}

#[test]
fn toggle_expanded_is_noop_on_workflow_without_runs() {
    use crate::panels::Panel;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    // GIVEN a workflow with no runs
    let mut app = make_test_app();
    let mut panels = PanelSet::default();
    app.workflows.push(WorkflowEntry {
        name: "empty".to_string(),
        kind: WorkflowType::Looping,
        state: WorkflowState::Dormant,
        runs: vec![],
        trigger: None,
        toml_content: None,
        autostart: false,
        update_available: None,
        origin: None,
    });
    app.ui.cursor = CursorTarget::Workflow(0);

    // WHEN Space is pressed on the workflow row
    panels.runs.handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
    );

    // THEN the expand state stays absent
    assert!(!panels.runs.is_expanded("empty"));
}
