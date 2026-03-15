use super::*;
use crate::storage::InMemoryStorage;
use crate::test_helpers::write_executable_script;
use crate::types::{StepDef, StepType, TriggerDef, WorkflowType};
use orchestr8r_notify::NoOpNotifier;

fn make_manager(event_tx: mpsc::Sender<EngineEvent>) -> WorkflowManager {
    let storage = Arc::new(InMemoryStorage::new());
    let data_dir = std::env::temp_dir().join("orchestr8r-wm-tests");
    WorkflowManager::new(
        storage,
        data_dir,
        event_tx,
        Arc::new(NoOpNotifier),
    )
}

fn looping_workflow(name: &str) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        workflow_type: WorkflowType::Looping,
        trigger: None,
        workspace: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["true".to_string()]),
            message: None,
            session: None,
            notify: None,
            agent: Default::default(),
        }],
    }
}

fn triggered_workflow(name: &str) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        workflow_type: WorkflowType::Triggered,
        trigger: None,
        workspace: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["true".to_string()]),
            message: None,
            session: None,
            notify: None,
            agent: Default::default(),
        }],
    }
}

fn polling_workflow(name: &str, command: Vec<String>) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        workflow_type: WorkflowType::Triggered,
        trigger: Some(TriggerDef::Polling {
            command,
            interval_secs: 3600, // Very long interval (1 hour)
        }),
        workspace: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["true".to_string()]),
            message: None,
            session: None,
            notify: None,
            agent: Default::default(),
        }],
    }
}

#[test]
fn register_makes_workflow_dormant() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);

    // WHEN
    manager.register(looping_workflow("hello"));

    // THEN
    let status = manager.status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].name, "hello");
    assert_eq!(status[0].state, WorkflowState::Dormant);
}

#[test]
fn register_emits_registered_and_state_changed_events() {
    // GIVEN
    let (tx, mut rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);

    // WHEN
    manager.register(looping_workflow("hello"));

    // THEN
    let ev1 = rx.try_recv().expect("WorkflowRegistered");
    assert!(matches!(ev1, EngineEvent::WorkflowRegistered { ref name, .. } if name == "hello"));
    let ev2 = rx.try_recv().expect("WorkflowStateChanged");
    assert!(
        matches!(ev2, EngineEvent::WorkflowStateChanged { ref name, state: WorkflowState::Dormant } if name == "hello")
    );
}

#[tokio::test]
async fn start_transitions_to_running_and_stop_returns_to_dormant() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = make_manager(tx);
    manager.register(looping_workflow("hello"));

    // WHEN
    manager.start("hello").await.unwrap();

    // THEN
    assert_eq!(manager.status()[0].state, WorkflowState::Running);

    // WHEN
    manager.stop("hello").await.unwrap();

    // THEN
    assert_eq!(manager.status()[0].state, WorkflowState::Dormant);
}

#[tokio::test]
async fn pause_and_resume_lifecycle() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = make_manager(tx);
    manager.register(looping_workflow("hello"));
    manager.start("hello").await.unwrap();

    // WHEN
    manager.pause("hello").unwrap();

    // THEN
    assert_eq!(manager.status()[0].state, WorkflowState::Paused);

    // WHEN
    manager.resume("hello").unwrap();

    // THEN
    assert_eq!(manager.status()[0].state, WorkflowState::Running);

    // cleanup
    manager.stop("hello").await.unwrap();
}

#[tokio::test]
async fn pause_rejected_for_triggered_workflow() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = make_manager(tx);
    manager.register(triggered_workflow("on-demand"));
    manager.start("on-demand").await.unwrap();

    // WHEN / THEN
    assert!(manager.pause("on-demand").is_err());

    // cleanup — wait for one-shot run to finish
    manager.stop("on-demand").await.unwrap();
}

#[tokio::test]
async fn start_fails_if_already_running() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = make_manager(tx);
    manager.register(looping_workflow("hello"));
    manager.start("hello").await.unwrap();

    // WHEN / THEN
    assert!(manager.start("hello").await.is_err());

    manager.stop("hello").await.unwrap();
}

#[tokio::test]
async fn stop_unknown_workflow_returns_error() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);

    // WHEN / THEN
    assert!(manager.stop("nope").await.is_err());
}

#[tokio::test]
async fn triggered_workflow_completes_and_returns_to_dormant() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = make_manager(tx);
    manager.register(triggered_workflow("job"));

    // WHEN
    manager.start("job").await.unwrap();
    // Give the task time to complete the single run.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // THEN
    assert_eq!(manager.status()[0].state, WorkflowState::Dormant);
}

#[tokio::test]
async fn status_reports_all_workflows_sorted() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = make_manager(tx);
    manager.register(looping_workflow("beta"));
    manager.register(looping_workflow("alpha"));

    // WHEN
    let statuses = manager.status();

    // THEN
    assert_eq!(statuses[0].name, "alpha");
    assert_eq!(statuses[1].name, "beta");

    manager.stop("alpha").await.unwrap();
    manager.stop("beta").await.unwrap();
}

#[tokio::test]
async fn paused_engine_loop_actually_pauses() {
    // GIVEN — workflow with a shell step that increments a counter
    let (tx, _rx) = mpsc::channel(64);
    let storage = Arc::new(InMemoryStorage::new());
    let data_dir = std::env::temp_dir().join("orchestr8r-pause-test");
    let mut manager = WorkflowManager::new(
        storage.clone(),
        data_dir,
        tx,
        Arc::new(NoOpNotifier),
    );
    manager.register(looping_workflow("counter"));
    manager.start("counter").await.unwrap();

    // Let it run for at least one iteration.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let runs_before_pause = storage.runs().len();

    // WHEN — pause
    manager.pause("counter").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let runs_after_pause = storage.runs().len();

    // THEN — no additional runs created while paused (at most 1 in-flight completes)
    assert!(
        runs_after_pause <= runs_before_pause + 1,
        "engine should not advance while paused"
    );

    // cleanup
    manager.stop("counter").await.unwrap();
}

#[tokio::test]
async fn polling_trigger_fires_immediately_when_manually_started() {
    // GIVEN — a polling workflow with a very long interval (1 hour),
    // with a mock polling script that returns one hash
    let temp_dir = std::env::temp_dir().join(format!(
        "orchestr8r-polling-immediate-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let cmd_path = write_executable_script(
        &temp_dir,
        "mock-poller.sh",
        "#!/bin/bash\nif [[ \"$1\" == \"--poll\" ]]; then echo '[\"test-hash\"]'; fi\nif [[ \"$1\" == \"--context\" ]]; then mkdir -p \"$3\"; fi\n",
    ).unwrap();

    let (tx, _rx) = mpsc::channel(64);
    let storage = Arc::new(InMemoryStorage::new());
    let data_dir = temp_dir.clone();
    let mut manager = WorkflowManager::new(
        storage.clone(),
        data_dir,
        tx,
        Arc::new(NoOpNotifier),
    );

    let workflow = polling_workflow(
        "poller",
        vec![cmd_path.to_string_lossy().to_string()],
    );
    manager.register(workflow);

    // WHEN — manually start the workflow
    let start_time = std::time::Instant::now();
    manager.start("poller").await.unwrap();

    // Give the trigger time to fire and execute
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let elapsed = start_time.elapsed();

    // THEN — workflow should fire quickly (not wait for 3600 second interval)
    assert!(
        elapsed.as_millis() < 1000,
        "polling trigger should fire immediately, not wait for 3600s interval. elapsed: {elapsed:?}"
    );

    // AND — the workflow should be running (listening for more events)
    assert_eq!(manager.status()[0].state, WorkflowState::Running);

    // AND — a run should have been created
    assert!(
        !storage.runs().is_empty(),
        "a workflow run should have been created"
    );

    // WHEN — stop the workflow
    manager.stop("poller").await.unwrap();

    // Give it time to actually stop
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // THEN — it should be dormant
    assert_eq!(manager.status()[0].state, WorkflowState::Dormant);

    // cleanup
    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn polling_trigger_executes_all_events_from_single_poll() {
    // GIVEN — a polling workflow that returns 3 hashes from a single poll
    let temp_dir = std::env::temp_dir().join(format!(
        "orchestr8r-polling-multi-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let cmd_path = write_executable_script(
        &temp_dir,
        "mock-poller.sh",
        "#!/bin/bash\nif [[ \"$1\" == \"--poll\" ]]; then echo '[\"hash1\", \"hash2\", \"hash3\"]'; fi\nif [[ \"$1\" == \"--context\" ]]; then mkdir -p \"$3\"; fi\n",
    ).unwrap();

    let (tx, _rx) = mpsc::channel(64);
    let storage = Arc::new(InMemoryStorage::new());
    let data_dir = temp_dir.clone();
    let mut manager = WorkflowManager::new(
        storage.clone(),
        data_dir,
        tx,
        Arc::new(NoOpNotifier),
    );

    let workflow = polling_workflow(
        "multi-poller",
        vec![cmd_path.to_string_lossy().to_string()],
    );
    manager.register(workflow);

    // WHEN — manually start the workflow
    manager.start("multi-poller").await.unwrap();

    // Give the trigger time to fire and execute all events
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // THEN — all 3 trigger events should result in separate runs
    let runs = storage.runs();
    assert_eq!(runs.len(), 3);

    // AND — workflow should be running (listening for more events)
    assert_eq!(manager.status()[0].state, WorkflowState::Running);

    // WHEN — stop the workflow
    manager.stop("multi-poller").await.unwrap();

    // Give it time to stop
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // THEN — it should be dormant
    assert_eq!(manager.status()[0].state, WorkflowState::Dormant);

    // cleanup
    let _ = std::fs::remove_dir_all(&temp_dir);
}
