use super::*;
use crate::storage::InMemoryStorage;
use crate::types::{StepDef, StepType, WorkflowKind};
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

fn indefinite_workflow(name: &str) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        kind: WorkflowKind::Indefinite,
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
        kind: WorkflowKind::Triggered,
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

#[test]
fn register_makes_workflow_dormant() {
    // GIVEN
    let (tx, _rx) = mpsc::channel(32);
    let mut manager = make_manager(tx);

    // WHEN
    manager.register(indefinite_workflow("hello"));

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
    manager.register(indefinite_workflow("hello"));

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
    manager.register(indefinite_workflow("hello"));

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
    manager.register(indefinite_workflow("hello"));
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
    manager.register(indefinite_workflow("hello"));
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
    manager.register(indefinite_workflow("beta"));
    manager.register(indefinite_workflow("alpha"));

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
    manager.register(indefinite_workflow("counter"));
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
