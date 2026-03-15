use super::*;
use crate::storage::InMemoryStorage;
use crate::types::{RunStatus, StepDef, StepType, WorkflowDef, WorkflowType};
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
    wf.workspace = Some(workspace.path().to_string_lossy().to_string());

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
