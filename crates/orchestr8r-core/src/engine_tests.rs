use super::*;
use crate::storage::InMemoryStorage;
use crate::types::{RunStatus, StepDef, StepType, WorkflowDef, WorkflowKind};
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

fn workflow(name: &str, kind: WorkflowKind, steps: Vec<StepDef>) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        kind,
        trigger: None,
        workspace: None,
        steps,
    }
}

#[tokio::test]
async fn shell_step_runs_and_logs() {
    // GIVEN
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let wf = workflow(
        "test-shell",
        WorkflowKind::Indefinite,
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
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    shutdown.store(true, Ordering::Relaxed);
    let storage = handle.await.unwrap();

    // THEN
    let logs = storage.logs();
    assert!(!logs.is_empty(), "expected at least one log entry");
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
        WorkflowKind::Indefinite,
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
        WorkflowKind::Indefinite,
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
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    shutdown.store(true, Ordering::Relaxed);
    handle.await.unwrap();

    // THEN
    assert!(marker.exists(), "shell step should have run in the workspace dir");
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
        kind: WorkflowKind::Triggered,
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
    // GIVEN an indefinite workflow with a fast shell step
    let storage = Arc::new(InMemoryStorage::new());
    let engine = Engine::new(
        storage.clone(),
        std::env::temp_dir().join("orchestr8r-tests-pause"),
        Arc::new(orchestr8r_notify::NoOpNotifier),
    );
    let paused_flag = engine.paused_flag();
    let wf = workflow(
        "test-pause",
        WorkflowKind::Indefinite,
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

    // Let it run for a bit, then pause
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    paused_flag.store(true, Ordering::Relaxed);
    // Allow any in-flight iteration to finish writing its logs before snapshotting
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let count_while_paused = storage.logs().len();

    // Wait to confirm no new iterations happen while paused
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let count_after_wait = storage.logs().len();
    assert_eq!(
        count_while_paused, count_after_wait,
        "no new logs should be produced while paused"
    );

    // Resume and confirm iterations restart
    paused_flag.store(false, Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let count_after_resume = storage.logs().len();
    assert!(
        count_after_resume > count_after_wait,
        "iterations should resume after unpausing"
    );

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
        WorkflowKind::Indefinite,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "x".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move { engine.run(&wf, shutdown_clone, None).await });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    paused_flag.store(true, Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // WHEN shutdown while paused
    shutdown.store(true, Ordering::Relaxed);

    // THEN engine exits without hanging
    tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("engine should exit within timeout")
        .unwrap()
        .unwrap();
}
