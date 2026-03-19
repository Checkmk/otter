use super::*;
use crate::storage::InMemoryStorage;
use crate::test_helpers::write_executable_script;
use crate::types::{RunStatus, StepDef, StepType, TriggerDef, WorkflowDef, WorkflowType, WorkspaceConfig};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn make_engine(storage: Arc<InMemoryStorage>) -> Engine {
    let scratch = std::env::temp_dir().join("orchestr8r-tests");
    Engine::new(storage, scratch, Arc::new(orchestr8r_notify::NoOpNotifier))
}

fn step_def(step_type: StepType) -> StepDef {
    StepDef {
        step_type,
        command: None,
        message: None,
        session: None,
        notify: None,
        agent: Default::default(),
    }
}

fn workflow(name: &str, workflow_type: WorkflowType, steps: Vec<StepDef>) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        workflow_type,
        trigger: None,
        workspace: None,
        resources: None,
        steps,
    }
}

// Helper: Poll until logs are non-empty, with timeout (used 3 times)
async fn wait_for_logs(storage: &Arc<InMemoryStorage>, timeout_secs: u64) {
    let start = tokio::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);
    loop {
        if !storage.logs().is_empty() {
            return;
        }
        assert!(
            start.elapsed() < timeout,
            "logs did not appear within timeout"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn shell_step_runs_and_logs() {
    // GIVEN
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let wf = workflow(
        "test-shell",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "hello".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let storage_clone = storage.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
        storage_clone
    });

    wait_for_logs(&storage, 5).await;

    shutdown.store(true, Ordering::Relaxed);
    let storage = handle.await.unwrap();

    // THEN
    let logs = storage.logs();
    assert!(!logs.is_empty());
    assert_eq!(logs[0].step_type, "shell");
    assert!(logs[0].stdout.contains("hello"));
}

#[tokio::test]
async fn failed_shell_command_marks_run_failed() {
    // GIVEN
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let wf = workflow(
        "test-fail",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["false".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    engine.run(&wf, shutdown, None).await.unwrap();

    // THEN
    let runs = storage.runs();
    assert_eq!(runs.last().unwrap().status, RunStatus::Failed);
}

#[tokio::test]
async fn workspace_step_sets_working_dir_for_shell() {
    // GIVEN
    let workspace = tempfile::tempdir().unwrap();
    let marker = workspace.path().join("marker.txt");

    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-workspace",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["touch".to_string(), "marker.txt".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );
    wf.workspace = Some(WorkspaceConfig::Fixed {
        path: workspace.path().to_string_lossy().into_owned(),
    });

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let marker_clone = marker.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
    });

    // Poll for marker file with timeout
    let start = tokio::time::Instant::now();
    let timeout = std::time::Duration::from_secs(5);
    loop {
        if marker.exists() {
            break;
        }
        assert!(
            start.elapsed() < timeout,
            "marker file not created within timeout"
        );
        tokio::task::yield_now().await;
    }

    shutdown.store(true, Ordering::Relaxed);
    handle.await.unwrap();

    // THEN
    assert!(marker_clone.exists());
}


#[tokio::test]
async fn triggered_workflow_runs_once_per_event() {
    // GIVEN a triggered workflow with a ManualTrigger
    use crate::types::TriggerDef;

    let storage = Arc::new(InMemoryStorage::new());
    let scratch = tempfile::tempdir().unwrap();

    let engine = Engine::new(
        storage.clone(),
        scratch.path().to_path_buf(),
        Arc::new(orchestr8r_notify::NoOpNotifier),
    );

    let wf = WorkflowDef {
        name: "my-workflow".to_string(),
        workflow_type: WorkflowType::Triggered,
        trigger: Some(TriggerDef::Manual),
        workspace: None,
        resources: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "triggered".to_string()]),
            ..step_def(StepType::Shell)
        }],
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let storage_clone = storage.clone();

    let handle = tokio::spawn(async move {
        engine.run_once(&wf, None, shutdown_clone, None).await.unwrap();
        storage_clone
    });

    let storage = handle.await.unwrap();

    // THEN — one completed run was recorded
    let runs = storage.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Completed);

    let logs = storage.logs();
    assert!(!logs.is_empty());
    assert!(logs[0].stdout.contains("triggered"));

    // Cleanup
    shutdown.store(true, Ordering::Relaxed);
}

#[tokio::test]
async fn pause_halts_iterations_and_resume_continues() {
    // GIVEN an looping workflow with a fast shell step
    let storage = Arc::new(InMemoryStorage::new());
    let engine = Engine::new(
        storage.clone(),
        std::env::temp_dir().join("orchestr8r-tests-pause"),
        Arc::new(orchestr8r_notify::NoOpNotifier),
    );
    let paused_flag = engine.paused_flag();
    let wf = workflow(
        "test-pause",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "iter".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let storage_clone = storage.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
        storage_clone
    });

    // Wait for at least one iteration to complete before pausing
    wait_for_logs(&storage, 5).await;

    paused_flag.store(true, Ordering::Relaxed);

    // Wait for log count to stabilize while paused
    // Poll until log count stabilizes (same count for 3 consecutive checks ~75ms)
    let settle_start = tokio::time::Instant::now();
    let mut prev_count = storage.logs().len();
    let mut stable_checks = 0;
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let current_count = storage.logs().len();
        if current_count == prev_count {
            stable_checks += 1;
            if stable_checks >= 3 {
                break;
            }
        } else {
            stable_checks = 0;
        }
        prev_count = current_count;
        assert!(
            settle_start.elapsed() < std::time::Duration::from_secs(2),
            "log count did not stabilize within timeout"
        );
    }
    let count_while_paused = storage.logs().len();

    // Verify no new logs appear while paused (poll for 500ms to be sure)
    let check_start = tokio::time::Instant::now();
    loop {
        let current = storage.logs().len();
        assert_eq!(
            current, count_while_paused,
            "no new logs should be produced while paused"
        );
        if check_start.elapsed() > std::time::Duration::from_millis(500) {
            break;
        }
        tokio::task::yield_now().await;
    }

    // Resume and confirm iterations restart
    paused_flag.store(false, Ordering::Relaxed);
    let resume_start = tokio::time::Instant::now();
    let resume_timeout = std::time::Duration::from_secs(5);
    loop {
        if storage.logs().len() > count_while_paused {
            break;
        }
        assert!(
            resume_start.elapsed() < resume_timeout,
            "log count did not increase within timeout"
        );
        tokio::task::yield_now().await;
    }

    shutdown.store(true, Ordering::Relaxed);
    handle.await.unwrap();
}

#[tokio::test]
async fn looping_workflow_creates_new_run_per_iteration() {
    // GIVEN a looping workflow with a fast shell step
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let wf = workflow(
        "test-new-runs",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "iter".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );

    // WHEN the engine runs until at least 2 runs appear
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let storage_clone = storage.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
        storage_clone
    });

    let start = tokio::time::Instant::now();
    loop {
        if storage.runs().len() >= 2 {
            break;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "two separate runs did not appear within timeout"
        );
        tokio::task::yield_now().await;
    }

    shutdown.store(true, Ordering::Relaxed);
    let storage = handle.await.unwrap();

    // THEN each iteration created a distinct run record
    let runs = storage.runs();
    assert!(runs.len() >= 2, "expected >= 2 runs, got {}", runs.len());
    let ids: std::collections::HashSet<_> = runs.iter().map(|r| r.id).collect();
    assert_eq!(ids.len(), runs.len(), "all run IDs should be distinct");
}

#[tokio::test]
async fn shutdown_while_paused_exits_cleanly() {
    // GIVEN a paused engine
    let storage = Arc::new(InMemoryStorage::new());
    let engine = Engine::new(
        storage.clone(),
        std::env::temp_dir().join("orchestr8r-tests-pause-shutdown"),
        Arc::new(orchestr8r_notify::NoOpNotifier),
    );
    let paused_flag = engine.paused_flag();
    let wf = workflow(
        "test-pause-shutdown",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "x".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move { engine.run(&wf, shutdown_clone, None).await });

    // Wait for at least one iteration to start
    wait_for_logs(&storage, 5).await;
    paused_flag.store(true, Ordering::Relaxed);

    // WHEN shutdown while paused
    shutdown.store(true, Ordering::Relaxed);

    // THEN engine exits without hanging
    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("engine should exit within timeout")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn script_workspace_polling_trigger_context_written_to_workspace() {
    // GIVEN
    // The desired flow: poll trigger finds hash → workspace script runs → context command
    // writes into <workspace>/trigger-context/ → steps execute in that workspace.
    let temp = tempfile::tempdir().unwrap();
    let workspaces_dir = temp.path().join("workspaces");
    std::fs::create_dir_all(&workspaces_dir).unwrap();

    // Workspace script: creates a unique dir per run_id ($2) and returns its path.
    let ws_script = write_executable_script(
        temp.path(),
        "workspace.sh",
        &format!(
            "#!/bin/bash\nRUN_DIR='{}/run-$2'\nmkdir -p \"$RUN_DIR\"\necho \"$RUN_DIR\"",
            workspaces_dir.display()
        ),
    )
    .unwrap();

    // Context command: writes context.txt into the trigger-context dir it receives.
    let ctx_script = write_executable_script(
        temp.path(),
        "context.sh",
        "#!/bin/bash\nmkdir -p \"$2\"\necho 'ctx-data' > \"$2/context.txt\"",
    )
    .unwrap();

    // Poll script: returns a single hash once, then nothing.
    let poll_script = write_executable_script(
        temp.path(),
        "poll.sh",
        "#!/bin/bash\necho '[\"hash-001\"]'",
    )
    .unwrap();

    let storage = Arc::new(InMemoryStorage::new());
    let scratch = temp.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();

    let engine = Engine::new(
        storage.clone(),
        scratch.clone(),
        Arc::new(orchestr8r_notify::NoOpNotifier),
    );

    // Shell step: asserts trigger-context/context.txt exists in the current working dir
    // (which must be the script-created workspace), then drops a marker file to prove it.
    let wf = WorkflowDef {
        name: "test-script-ws-trigger".to_string(),
        workflow_type: WorkflowType::Triggered,
        trigger: Some(TriggerDef::Polling {
            poll_command: vec![poll_script.to_string_lossy().into_owned()],
            context_command: Some(vec![ctx_script.to_string_lossy().into_owned()]),
            interval_secs: 3600, // won't re-poll during the test
        }),
        workspace: Some(WorkspaceConfig::Script {
            command: vec![ws_script.to_string_lossy().into_owned()],
        }),
        resources: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            // Fail explicitly if context.txt is not in the workspace; write marker if it is.
            command: Some(vec![
                "bash".to_string(),
                "-c".to_string(),
                "test -f trigger-context/context.txt && touch workspace-marker.txt".to_string(),
            ]),
            ..step_def(StepType::Shell)
        }],
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let storage_clone = storage.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
        storage_clone
    });

    // WHEN: wait for the single run to complete, then shut down.
    let start = tokio::time::Instant::now();
    loop {
        if storage.runs().iter().any(|r| r.status == RunStatus::Completed) {
            break;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "run did not complete within timeout"
        );
        tokio::task::yield_now().await;
    }
    shutdown.store(true, Ordering::Relaxed);
    handle.await.unwrap();

    // THEN: the run completed successfully (shell step found context.txt in its CWD).
    assert!(
        storage.runs().iter().any(|r| r.status == RunStatus::Completed),
        "run should have completed"
    );

    // AND: workspace-marker.txt exists in a script-created workspace dir, not in scratch.
    let marker_in_workspace = std::fs::read_dir(&workspaces_dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().join("workspace-marker.txt").exists());
    assert!(
        marker_in_workspace,
        "workspace-marker.txt should exist in the script-created workspace dir"
    );

    // AND: context.txt lives inside the workspace's trigger-context/, not in scratch.
    let context_in_workspace = std::fs::read_dir(&workspaces_dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().join("trigger-context").join("context.txt").exists());
    assert!(
        context_in_workspace,
        "trigger-context/context.txt should be inside the workspace dir"
    );

    let context_in_scratch = scratch.exists() && std::fs::read_dir(&scratch)
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().join("trigger-context").exists());
    assert!(
        !context_in_scratch,
        "context should not have been placed in the scratch dir"
    );
}
