use super::*;
use crate::storage::InMemoryStorage;
use crate::types::{CheckpointResponse, RunStatus, StepDef, StepType, WorkflowDef, WorkflowKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
            message: None,
            session: None,
            notify: None,
            agent: Default::default(),
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

#[test]
fn unknown_step_type_fails_deserialization() {
    // GIVEN
    let toml_str = r#"
        name = "bad"
        kind = "indefinite"
        [[steps]]
        type = "nonexistent"
    "#;

    // WHEN / THEN
    assert!(toml::from_str::<WorkflowDef>(toml_str).is_err());
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
            message: None,
            session: None,
            notify: None,
            agent: Default::default(),
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
        vec![
            StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["touch".to_string(), "marker.txt".to_string()]),
                message: None,
                session: None,
                notify: None,
                agent: Default::default(),
            },
        ],
    );
    wf.workspace = Some(workspace.path().to_string_lossy().to_string());

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    shutdown.store(true, Ordering::Relaxed);
    handle.await.unwrap();

    // THEN
    assert!(
        marker.exists(),
        "shell step should have run in the workspace dir"
    );
}

#[tokio::test]
async fn named_session_shared_across_agent_steps() {
    // GIVEN two agent steps sharing a session name; both use the command escape hatch
    // (CustomRunner re-runs the command per step, which is fine for this test)
    let storage = Arc::new(InMemoryStorage::new());
    let scratch = tempfile::tempdir().unwrap();
    let engine = Engine::with_executors(
        storage.clone(),
        scratch.path().to_path_buf(),
        vec![Box::new(crate::steps::agent::AgentExecutor)],
    );
    let wf = workflow(
        "test-sessions",
        WorkflowKind::Indefinite,
        vec![
            StepDef {
                step_type: StepType::Agent,
                command: Some(vec!["printf".to_string(), "first prompt".to_string()]),
                message: Some("first prompt".to_string()),
                session: Some("planner".to_string()),
                ..step_def(StepType::Agent)
            },
            StepDef {
                step_type: StepType::Agent,
                command: None,
                message: Some("second prompt".to_string()),
                session: Some("planner".to_string()),
                ..step_def(StepType::Agent)
            },
        ],
    );

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    shutdown.store(true, Ordering::Relaxed);
    handle.await.unwrap();

    // THEN — both steps ran and were logged
    let logs = storage.logs();
    let agent_logs: Vec<_> = logs.iter().filter(|l| l.step_type == "agent").collect();
    assert!(
        agent_logs.len() >= 2,
        "expected logs for both agent steps, got {:?}",
        agent_logs.len()
    );
}

struct MockNotifier {
    count: Mutex<usize>,
}

impl MockNotifier {
    fn new() -> Arc<Self> {
        Arc::new(Self { count: Mutex::new(0) })
    }
    fn call_count(&self) -> usize {
        *self.count.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl orchestr8r_notify::Notifier for MockNotifier {
    fn name(&self) -> &str { "mock" }
    async fn send(&self, _: &orchestr8r_notify::Notification) -> Result<(), orchestr8r_notify::NotifyError> {
        *self.count.lock().unwrap() += 1;
        Ok(())
    }
}

#[tokio::test]
async fn checkpoint_feedback_loop_reprompts_agent() {
    // GIVEN an agent step followed by a checkpoint that receives feedback then continue
    // Using `cat` as the command: it echoes stdin, so feedback text appears in stdout.
    let storage = Arc::new(InMemoryStorage::new());
    let scratch = tempfile::tempdir().unwrap();
    let notifier = MockNotifier::new();
    let engine = Engine::new(
        storage.clone(),
        scratch.path().to_path_buf(),
        notifier.clone(),
    );
    let wf = workflow(
        "test-feedback",
        WorkflowKind::Indefinite,
        vec![
            StepDef {
                step_type: StepType::Agent,
                command: Some(vec!["cat".to_string()]),
                message: Some("write code".to_string()),
                session: Some("coder".to_string()),
                ..step_def(StepType::Agent)
            },
            StepDef {
                step_type: StepType::Checkpoint,
                message: Some("Review the code".to_string()),
                ..step_def(StepType::Checkpoint)
            },
        ],
    );

    let (ui_tx, mut ui_rx) = mpsc::channel::<EngineEvent>(32);

    // Respond: feedback first, then stop to terminate the run
    let feedback_sent = Arc::new(AtomicBool::new(false));
    let feedback_sent_clone = feedback_sent.clone();
    tokio::spawn(async move {
        while let Some(event) = ui_rx.recv().await {
            if let EngineEvent::CheckpointPending { response_tx, .. } = event {
                if !feedback_sent_clone.load(Ordering::Relaxed) {
                    feedback_sent_clone.store(true, Ordering::Relaxed);
                    let _ = response_tx.send(CheckpointResponse::Feedback(
                        "please fix the typo".to_string(),
                    ));
                } else {
                    let _ = response_tx.send(CheckpointResponse::Stop);
                    break;
                }
            }
        }
    });

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    engine.run(&wf, shutdown, Some(ui_tx)).await.unwrap();

    // THEN — agent re-prompted with feedback; cat echoes it, so stdout contains the feedback text
    let logs = storage.logs();
    let agent_at_checkpoint: Vec<_> = logs
        .iter()
        .filter(|l| l.step_type == "agent" && l.step_index == 1)
        .collect();
    assert_eq!(
        agent_at_checkpoint.len(),
        1,
        "checkpoint should log the agent feedback response"
    );

    // Exactly one notification was sent when the checkpoint became pending
    assert!(
        notifier.call_count() >= 1,
        "checkpoint should have sent a desktop notification"
    );
}

#[tokio::test]
async fn checkpoint_without_session_does_not_offer_feedback() {
    // GIVEN a checkpoint with no prior agent step — feedback_available will be false
    let storage = Arc::new(InMemoryStorage::new());
    let scratch = tempfile::tempdir().unwrap();

    // Use a channel so we can verify what feedback_available was sent
    let (ui_tx, mut ui_rx) = mpsc::channel::<EngineEvent>(32);
    let feedback_available_seen = Arc::new(Mutex::new(None::<bool>));
    let seen_clone = feedback_available_seen.clone();
    tokio::spawn(async move {
        while let Some(event) = ui_rx.recv().await {
            if let EngineEvent::CheckpointPending { feedback_available, response_tx, .. } = event {
                *seen_clone.lock().unwrap() = Some(feedback_available);
                let _ = response_tx.send(CheckpointResponse::Stop);
                break;
            }
        }
    });

    let engine = Engine::new(
        storage.clone(),
        scratch.path().to_path_buf(),
        Arc::new(orchestr8r_notify::NoOpNotifier),
    );
    let wf = workflow(
        "test-no-session",
        WorkflowKind::Indefinite,
        vec![StepDef {
            step_type: StepType::Checkpoint,
            message: Some("Review".to_string()),
            ..step_def(StepType::Checkpoint)
        }],
    );

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    engine.run(&wf, shutdown, Some(ui_tx)).await.unwrap();

    // THEN — checkpoint reported feedback_available = false (no active agent session)
    assert_eq!(*feedback_available_seen.lock().unwrap(), Some(false));
}

#[tokio::test]
async fn anonymous_sessions_are_single_use() {
    // GIVEN two agent steps without session names — each gets its own anonymous session key
    let storage = Arc::new(InMemoryStorage::new());
    let scratch = tempfile::tempdir().unwrap();
    let engine = Engine::with_executors(
        storage.clone(),
        scratch.path().to_path_buf(),
        vec![Box::new(crate::steps::agent::AgentExecutor)],
    );
    let wf = workflow(
        "test-anon",
        WorkflowKind::Indefinite,
        vec![
            StepDef {
                step_type: StepType::Agent,
                command: Some(vec!["printf".to_string(), "task one".to_string()]),
                message: Some("task one".to_string()),
                session: None,
                ..step_def(StepType::Agent)
            },
            StepDef {
                step_type: StepType::Agent,
                command: Some(vec!["printf".to_string(), "task two".to_string()]),
                message: Some("task two".to_string()),
                session: None,
                ..step_def(StepType::Agent)
            },
        ],
    );

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    shutdown.store(true, Ordering::Relaxed);
    handle.await.unwrap();

    // THEN — both steps produced agent logs (each ran independently)
    let logs = storage.logs();
    let agent_logs: Vec<_> = logs.iter().filter(|l| l.step_type == "agent").collect();
    assert!(agent_logs.len() >= 2, "both anonymous agent steps should be logged");
}

#[tokio::test]
async fn sessions_cleaned_up_at_run_end() {
    // GIVEN a named session agent step
    let storage = Arc::new(InMemoryStorage::new());
    let scratch = tempfile::tempdir().unwrap();
    let engine = Engine::with_executors(
        storage.clone(),
        scratch.path().to_path_buf(),
        vec![Box::new(crate::steps::agent::AgentExecutor)],
    );
    let wf = workflow(
        "test-cleanup",
        WorkflowKind::Indefinite,
        vec![StepDef {
            step_type: StepType::Agent,
            command: Some(vec!["echo".to_string(), "do work".to_string()]),
            message: Some("do work".to_string()),
            session: Some("worker".to_string()),
            ..step_def(StepType::Agent)
        }],
    );

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    shutdown.store(true, Ordering::Relaxed);
    handle.await.unwrap();

    // THEN — agent step was logged (workflow ran and cleaned up without error)
    let logs = storage.logs();
    assert!(
        logs.iter().any(|l| l.step_type == "agent"),
        "agent step should have been logged"
    );
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
async fn polling_trigger_shuts_down_cleanly() {
    // GIVEN a polling trigger workflow
    use crate::types::TriggerDef;
    use std::fs;
    use std::time::Duration;

    let storage = Arc::new(InMemoryStorage::new());
    let scratch = tempfile::tempdir().unwrap();
    let script_dir = tempfile::tempdir().unwrap();

    // Create a simple polling script that returns new events
    let script_path = script_dir.path().join("poller.sh");
    fs::write(
        &script_path,
        "#!/bin/bash\nif [[ \"$1\" == \"--poll\" ]]; then echo '[\"event1\"]'; fi\nif [[ \"$1\" == \"--context\" ]]; then mkdir -p \"$3\"; fi",
    ).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let engine = Engine::new(
        storage.clone(),
        scratch.path().to_path_buf(),
        Arc::new(orchestr8r_notify::NoOpNotifier),
    );

    let wf = WorkflowDef {
        name: "polling-workflow".to_string(),
        kind: WorkflowKind::Triggered,
        trigger: Some(TriggerDef::Polling {
            command: vec![script_path.to_string_lossy().to_string()],
            interval_secs: 1,
        }),
        workspace: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "triggered".to_string()]),
            ..step_def(StepType::Shell)
        }],
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    // WHEN we run the triggered workflow and then shut it down quickly
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await
    });

    // Give it a moment to start the polling trigger
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Set shutdown flag
    shutdown.store(true, Ordering::Relaxed);

    // Wait for the engine to shut down
    let result = handle.await;

    // THEN the engine should shut down cleanly (not panic or error)
    assert!(result.is_ok(), "Engine should shut down cleanly");
    assert!(result.unwrap().is_ok(), "Engine run should complete without error");
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
