use super::*;
use crate::storage::InMemoryStorage;
use crate::test_helpers::write_executable_script;
use crate::types::{StepDef, StepType, TriggerDef, WorkflowDef, WorkflowType};
use otter_notify::NoOpNotifier;

fn make_manager(event_tx: mpsc::Sender<EngineEvent>) -> WorkflowManager {
    let storage = Arc::new(InMemoryStorage::new());
    let data_dir = std::env::temp_dir().join("otter-wm-tests");
    WorkflowManager::new(
        storage,
        data_dir,
        event_tx,
        Arc::new(NoOpNotifier),
    )
}

fn shell_step() -> StepDef {
    StepDef {
        step_type: StepType::Shell,
        command: Some(vec!["true".to_string()]),
        message: None,
        session: None,
        notify: None,
        secrets: None,
        sandbox: None,
        agent: Default::default(),
    }
}

fn looping_workflow(name: &str) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        workflow_type: WorkflowType::Looping,
        schema: None,
        version: None,
        trigger: None,
        workspace: None,
        resources: None,
        sandbox: None,
        steps: vec![shell_step()],
        finally: vec![],
    }
}

fn triggered_workflow(name: &str) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        workflow_type: WorkflowType::Triggered,
        schema: None,
        version: None,
        trigger: None,
        workspace: None,
        resources: None,
        sandbox: None,
        steps: vec![shell_step()],
        finally: vec![],
    }
}

fn manual_workflow(name: &str) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        workflow_type: WorkflowType::Triggered,
        schema: None,
        version: None,
        trigger: Some(TriggerDef::Manual),
        workspace: None,
        resources: None,
        sandbox: None,
        steps: vec![shell_step()],
        finally: vec![],
    }
}

fn polling_workflow(name: &str, command: Vec<String>) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        workflow_type: WorkflowType::Triggered,
        schema: None,
        version: None,
        trigger: Some(TriggerDef::Polling {
            poll_command: command,
            context_command: None,
            interval_secs: 3600, // Very long interval (1 hour)
            secrets: None,
        }),
        workspace: None,
        resources: None,
        sandbox: None,
        steps: vec![shell_step()],
        finally: vec![],
    }
}

#[test]
fn register_makes_workflow_dormant() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);

    // WHEN
    manager.register(looping_workflow("hello"), String::new());

    // THEN
    let status = manager.status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].name, "hello");
    assert_eq!(status[0].state, WorkflowState::Dormant);
}

#[test]
fn register_emits_state_changed_event() {
    // GIVEN
    let (tx, mut rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);

    // WHEN
    manager.register(looping_workflow("hello"), String::new());

    // THEN — exactly one event: the dormant state change.
    // Lifecycle (register/remove) is no longer announced via EngineEvent —
    // the daemon broadcasts a fresh WorkflowsSnapshot instead.
    let ev = rx.try_recv().expect("WorkflowStateChanged");
    assert!(
        matches!(ev, EngineEvent::WorkflowStateChanged { ref name, state: WorkflowState::Dormant } if name == "hello")
    );
    assert!(rx.try_recv().is_err(), "no further events expected");
}

#[test]
fn status_includes_toml_content() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);

    // WHEN
    manager.register(looping_workflow("hello"), "name = \"hello\"\n".to_string());

    // THEN
    let status = manager.status();
    assert_eq!(status[0].toml_content.as_deref(), Some("name = \"hello\"\n"));
}

#[tokio::test]
async fn start_transitions_to_running_and_stop_returns_to_dormant() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = make_manager(tx);
    manager.register(looping_workflow("hello"), String::new());

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
async fn start_fails_if_already_running() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = make_manager(tx);
    manager.register(looping_workflow("hello"), String::new());
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
    manager.register(triggered_workflow("job"), String::new());

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
    manager.register(looping_workflow("beta"), String::new());
    manager.register(looping_workflow("alpha"), String::new());

    // WHEN
    let statuses = manager.status();

    // THEN
    assert_eq!(statuses[0].name, "alpha");
    assert_eq!(statuses[1].name, "beta");

    manager.stop("alpha").await.unwrap();
    manager.stop("beta").await.unwrap();
}

#[tokio::test]
async fn polling_trigger_fires_immediately_when_manually_started() {
    // GIVEN — a polling workflow with a very long interval (1 hour),
    // with a mock polling script that returns one hash
    let temp_dir = std::env::temp_dir().join(format!(
        "otter-polling-immediate-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let cmd_path = write_executable_script(
        &temp_dir,
        "mock-poller.sh",
        "#!/bin/bash\necho '[\"test-hash\"]'\n",
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
    manager.register(workflow, String::new());

    // WHEN — manually start the workflow
    let start_time = std::time::Instant::now();
    manager.start("poller").await.unwrap();

    // Give the trigger time to fire and execute (generous for slow CI/parallel test runs)
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

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
        "otter-polling-multi-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let cmd_path = write_executable_script(
        &temp_dir,
        "mock-poller.sh",
        "#!/bin/bash\necho '[\"hash1\", \"hash2\", \"hash3\"]'\n",
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
    manager.register(workflow, String::new());

    // WHEN — manually start the workflow
    manager.start("multi-poller").await.unwrap();

    // Give the trigger time to fire and execute all events (generous for slow CI/parallel test runs)
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

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

#[tokio::test]
async fn manual_trigger_fires_immediately_on_start() {
    // GIVEN — a manual trigger workflow
    let (tx, _rx) = mpsc::channel(64);
    let storage = Arc::new(InMemoryStorage::new());
    let data_dir = std::env::temp_dir().join("otter-manual-trigger-test");
    let mut manager = WorkflowManager::new(
        storage.clone(),
        data_dir,
        tx,
        Arc::new(NoOpNotifier),
    );

    manager.register(manual_workflow("manual-job"), String::new());

    // WHEN — manually start the workflow
    manager.start("manual-job").await.unwrap();

    // THEN — a run should have been created (trigger fired immediately)
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let runs = storage.runs();
    assert!(
        !runs.is_empty(),
        "manual trigger should fire immediately on start, creating a run"
    );

    // THEN — it should return to dormant
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        manager.status()[0].state,
        WorkflowState::Dormant,
        "manual trigger workflow should return to dormant after being started"
    );
}

#[test]
fn unregister_removes_dormant_workflow() {
    // GIVEN
    let (tx, mut rx) = mpsc::channel(32);
    let storage = Arc::new(InMemoryStorage::new());
    let mut manager = WorkflowManager::new(
        storage.clone(),
        std::env::temp_dir(),
        tx,
        Arc::new(NoOpNotifier),
    );
    manager.register(looping_workflow("wf"), String::new());
    // drain registration events
    while rx.try_recv().is_ok() {}

    // WHEN
    manager.unregister("wf").unwrap();

    // THEN — workflow is no longer listed and no event was emitted
    // (the daemon broadcasts a fresh WorkflowsSnapshot instead).
    assert!(manager.status().is_empty());
    assert!(rx.try_recv().is_err(), "no event expected on unregister");
}

#[tokio::test]
async fn unregister_fails_if_workflow_is_running() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);
    manager.register(looping_workflow("wf"), String::new());
    manager.start("wf").await.unwrap();

    // WHEN / THEN
    assert!(manager.unregister("wf").is_err());

    manager.stop("wf").await.unwrap();
}

#[test]
fn unregister_marks_runs_orphaned() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let storage = Arc::new(InMemoryStorage::new());
    let mut manager = WorkflowManager::new(
        storage.clone(),
        std::env::temp_dir(),
        tx,
        Arc::new(NoOpNotifier),
    );
    let run = crate::types::WorkflowRun::new("wf".to_string());
    storage.save_workflow_run(&run).unwrap();
    manager.register(looping_workflow("wf"), String::new());

    // WHEN
    manager.unregister("wf").unwrap();

    // THEN
    let runs = storage.runs();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].orphaned);
}

#[test]
fn reload_adds_new_workflow() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);

    // WHEN
    manager.reload(vec![(looping_workflow("new-wf"), String::new(), None)]);

    // THEN
    let status = manager.status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].name, "new-wf");
}

#[test]
fn reload_removes_dormant_workflow_not_in_new_list() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);
    manager.register(looping_workflow("old-wf"), String::new());
    manager.register(looping_workflow("keep-wf"), String::new());

    // WHEN — reload with only keep-wf
    manager.reload(vec![(looping_workflow("keep-wf"), String::new(), None)]);

    // THEN
    let names: Vec<_> = manager.status().into_iter().map(|s| s.name).collect();
    assert_eq!(names, vec!["keep-wf"]);
}

#[tokio::test]
async fn reload_leaves_running_workflow_unchanged() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(64);
    let mut manager = make_manager(tx);
    manager.register(looping_workflow("running-wf"), String::new());
    manager.start("running-wf").await.unwrap();

    // WHEN — reload without running-wf
    manager.reload(Vec::new());

    // THEN — running workflow is still registered (not removed)
    let status = manager.status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].state, WorkflowState::Running);

    manager.stop("running-wf").await.unwrap();
}

#[test]
fn register_with_scripts_dir_stores_scripts_dir() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);
    let scripts_dir = std::path::PathBuf::from("/tmp/scripts");

    // WHEN
    manager.register_with_scripts_dir(looping_workflow("wf"), String::new(), Some(scripts_dir.clone()));

    // THEN — workflow is registered (we can't inspect scripts_dir directly, but start should work)
    let status = manager.status();
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].name, "wf");
}

#[tokio::test]
async fn abort_and_stop_returns_workflow_to_dormant_immediately() {
    // GIVEN — a looping workflow whose step runs for 60 seconds
    let (tx, _rx) = mpsc::channel(64);
    let storage = Arc::new(InMemoryStorage::new());
    let data_dir = std::env::temp_dir().join("otter-abort-test");
    let mut manager = WorkflowManager::new(
        storage,
        data_dir,
        tx,
        Arc::new(NoOpNotifier),
    );
    let wf = WorkflowDef {
        name: "long-job".to_string(),
        workflow_type: WorkflowType::Looping,
        schema: None,
        version: None,
        trigger: None,
        workspace: None,
        resources: None,
        sandbox: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["sleep".to_string(), "60".to_string()]),
            message: None,
            session: None,
            notify: None,
            secrets: None,
            sandbox: None,
            agent: Default::default(),
        }],
        finally: vec![],
    };
    manager.register(wf, String::new());
    manager.start("long-job").await.unwrap();

    // Wait for the engine to reach Running state
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if manager.status()[0].state == WorkflowState::Running { break; }
        assert!(std::time::Instant::now() < deadline, "timed out waiting for Running");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // WHEN — abort_and_stop is called while the 60-second step is in progress
    manager.abort_and_stop("long-job");

    // THEN — immediately Dormant, did not wait 60 seconds for the step to complete
    assert_eq!(manager.status()[0].state, WorkflowState::Dormant);

    // AND — the workflow can be started again (task handle was cleared)
    manager.start("long-job").await.unwrap();
    manager.stop("long-job").await.unwrap();
}
