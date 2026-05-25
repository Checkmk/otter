use super::*;
use crate::storage::InMemoryStorage;
use crate::test_helpers::{bash_path, executable_name, write_executable_script};
use crate::types::{
    FinallyStepDef, RunOutcome, RunStatus, StepDef, StepType, TriggerDef, WorkflowDef, WorkflowRun,
    WorkflowType, WorkspaceConfig, WorkspaceSource,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn make_engine(storage: Arc<InMemoryStorage>) -> Engine {
    let scratch = std::env::temp_dir().join("otter-tests");
    Engine::new(storage, scratch, Arc::new(otter_notify::NoOpNotifier))
}

fn step_def(step_type: StepType) -> StepDef {
    StepDef {
        step_type,
        command: None,
        message: None,
        session: None,
        notify: None,
        requires: None,
        sandbox: None,
        agent: Default::default(),
    }
}

fn workflow(name: &str, workflow_type: WorkflowType, steps: Vec<StepDef>) -> WorkflowDef {
    WorkflowDef {
        name: name.to_string(),
        workflow_type,
        schema: None,
        version: None,
        description: None,
        trigger: None,
        workspace: None,
        resources: None,
        sandbox: None,
        steps,
        finally: vec![],
        require: None,
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
    let shell_log = logs
        .iter()
        .find(|l| l.step_type == "shell")
        .expect("shell log not found");
    assert!(shell_log.stdout.contains("hello"));
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
    wf.workspace = Some(
        WorkspaceSource::Fixed {
            path: workspace.path().to_string_lossy().into_owned(),
        }
        .into(),
    );

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
        Arc::new(otter_notify::NoOpNotifier),
    );

    let wf = WorkflowDef {
        name: "my-workflow".to_string(),
        workflow_type: WorkflowType::Triggered,
        schema: None,
        version: None,
        description: None,
        trigger: Some(TriggerDef::Manual),
        workspace: None,
        resources: None,
        sandbox: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "triggered".to_string()]),
            ..step_def(StepType::Shell)
        }],
        finally: vec![],
        require: None,
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let storage_clone = storage.clone();

    let handle = tokio::spawn(async move {
        engine
            .run_once(&wf, None, shutdown_clone, None)
            .await
            .unwrap();
        storage_clone
    });

    let storage = handle.await.unwrap();

    // THEN — one completed run was recorded
    let runs = storage.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, RunStatus::Completed);

    let logs = storage.logs();
    assert!(
        logs.iter().any(|l| l.stdout.contains("triggered")),
        "no log containing 'triggered' found"
    );

    // Cleanup
    shutdown.store(true, Ordering::Relaxed);
}

#[tokio::test]
async fn stop_prevents_next_iteration() {
    // GIVEN a looping workflow with a fast shell step
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let wf = workflow(
        "test-stop",
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

    // Wait for at least one iteration to complete, then stop
    wait_for_logs(&storage, 5).await;
    shutdown.store(true, Ordering::Relaxed);

    let storage = handle.await.unwrap();

    // THEN the engine exits cleanly; a completed run exists
    let runs = storage.runs();
    assert!(!runs.is_empty());
    assert!(runs.iter().any(|r| r.status == RunStatus::Completed));
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
            "#!/bin/bash\nRUN_DIR=\"{}/run-$2\"\nmkdir -p \"$RUN_DIR\"\necho \"$RUN_DIR\"",
            bash_path(&workspaces_dir)
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
    let poll_script =
        write_executable_script(temp.path(), "poll.sh", "#!/bin/bash\necho '[\"hash-001\"]'")
            .unwrap();

    let storage = Arc::new(InMemoryStorage::new());
    let scratch = temp.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();

    let engine = Engine::new(
        storage.clone(),
        scratch.clone(),
        Arc::new(otter_notify::NoOpNotifier),
    );

    // Shell step: asserts trigger-context/context.txt exists in the current working dir
    // (which must be the script-created workspace), then drops a marker file to prove it.
    let wf = WorkflowDef {
        name: "test-script-ws-trigger".to_string(),
        workflow_type: WorkflowType::Triggered,
        schema: None,
        version: None,
        description: None,
        trigger: Some(TriggerDef::Polling {
            poll_command: vec![poll_script.to_string_lossy().into_owned()],
            context_command: Some(vec![ctx_script.to_string_lossy().into_owned()]),
            interval_secs: 3600, // won't re-poll during the test
            requires: None,
        }),
        workspace: Some(
            WorkspaceSource::Script {
                command: vec![ws_script.to_string_lossy().into_owned()],
                requires: None,
            }
            .into(),
        ),
        resources: None,
        sandbox: None,
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
        finally: vec![],
        require: None,
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
        if storage
            .runs()
            .iter()
            .any(|r| r.status == RunStatus::Completed)
        {
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
        storage
            .runs()
            .iter()
            .any(|r| r.status == RunStatus::Completed),
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
        .any(|e| {
            e.path()
                .join("trigger-context")
                .join("context.txt")
                .exists()
        });
    assert!(
        context_in_workspace,
        "trigger-context/context.txt should be inside the workspace dir"
    );

    let context_in_scratch = scratch.exists()
        && std::fs::read_dir(&scratch)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.path().join("trigger-context").exists());
    assert!(
        !context_in_scratch,
        "context should not have been placed in the scratch dir"
    );
}

#[tokio::test]
async fn triggered_workflow_with_git_pool_acquires_and_releases_slot() {
    // GIVEN a base git repo with one commit, an empty pool dir, a triggered workflow
    // pinned to that repo with `[workspace.pool]`, and a shell step that succeeds.
    use crate::types::PoolConfig;
    use std::process::Command;

    let temp = tempfile::tempdir().unwrap();
    let base_repo = temp.path().join("base-repo");
    std::fs::create_dir_all(&base_repo).unwrap();
    for args in [
        &["init", "--initial-branch=main"][..],
        &["config", "user.email", "t@t"],
        &["config", "user.name", "t"],
    ] {
        let out = Command::new("git")
            .arg("-C")
            .arg(&base_repo)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    std::fs::write(base_repo.join("README.md"), "hello").unwrap();
    for args in [&["add", "."][..], &["commit", "-m", "init"]] {
        let out = Command::new("git")
            .arg("-C")
            .arg(&base_repo)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    let pool_dir = temp.path().join("pool");

    let storage = Arc::new(InMemoryStorage::new());
    let scratch = temp.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();
    let engine = Engine::new(
        storage.clone(),
        scratch.clone(),
        Arc::new(otter_notify::NoOpNotifier),
    );

    // A poll script that emits a single hash, then nothing (interval guards repeat).
    let poll_script = write_executable_script(
        temp.path(),
        &format!("poll.{}", executable_name("sh")),
        "#!/bin/bash\necho '[\"hash-001\"]'",
    )
    .unwrap();

    // Step asserts that README.md (from the base repo) is present at the workspace root.
    let wf = WorkflowDef {
        name: "test-git-pool".to_string(),
        workflow_type: WorkflowType::Triggered,
        schema: None,
        version: None,
        description: None,
        trigger: Some(TriggerDef::Polling {
            poll_command: vec![poll_script.to_string_lossy().into_owned()],
            context_command: None,
            interval_secs: 3600,
            requires: None,
        }),
        workspace: Some(WorkspaceConfig {
            source: WorkspaceSource::Git {
                base_repo: base_repo.to_string_lossy().into_owned(),
                ref_: Some("HEAD".to_string()),
            },
            pool: Some(PoolConfig {
                dir: pool_dir.to_string_lossy().into_owned(),
                keep_directory_on: vec![],
            }),
        }),
        resources: None,
        sandbox: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec![
                "bash".to_string(),
                "-c".to_string(),
                "test -f README.md".to_string(),
            ]),
            ..step_def(StepType::Shell)
        }],
        finally: vec![],
        require: None,
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
    });

    // WHEN: wait for the run to complete.
    let start = tokio::time::Instant::now();
    loop {
        if storage
            .runs()
            .iter()
            .any(|r| r.status == RunStatus::Completed)
        {
            break;
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "run did not complete in time; runs: {:?}",
            storage
                .runs()
                .iter()
                .map(|r| (r.id, r.status.clone()))
                .collect::<Vec<_>>()
        );
        tokio::task::yield_now().await;
    }
    shutdown.store(true, Ordering::Relaxed);
    handle.await.unwrap();

    // THEN: the slot dir was created, the worktree persists for reuse, and the lock
    // was released (cleanup ran at end of finally).
    assert!(
        pool_dir.join("slot-0").is_dir(),
        "slot-0 worktree should exist"
    );
    assert!(
        !pool_dir.join("slot-0.lock").exists(),
        "slot-0 lock should be released after a successful run, got: {:?}",
        std::fs::read_dir(&pool_dir).ok().map(|r| r
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect::<Vec<_>>())
    );
}

#[tokio::test]
async fn context_command_resolves_via_scripts_dir_path() {
    // GIVEN
    let temp = tempfile::tempdir().unwrap();
    let scripts_dir = temp.path().join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();

    // Poll script: absolute path (polls are resolved before scripts_dir is known to engine)
    let poll_script =
        write_executable_script(temp.path(), "poll.sh", "#!/bin/bash\necho '[\"hash-abc\"]'")
            .unwrap();

    // Context script: lives in scripts_dir, referenced by bare name only
    write_executable_script(
        &scripts_dir,
        "ctx.sh",
        "#!/bin/bash\nmkdir -p \"$2\"\necho 'ctx' > \"$2/ctx.txt\"",
    )
    .unwrap();

    let storage = Arc::new(InMemoryStorage::new());
    let scratch = temp.path().join("scratch");
    std::fs::create_dir_all(&scratch).unwrap();

    let engine = Engine::new_with_scripts_dir(
        storage.clone(),
        scratch.clone(),
        Arc::new(otter_notify::NoOpNotifier),
        Some(scripts_dir),
    );

    let wf = WorkflowDef {
        name: "test-ctx-scripts-dir".to_string(),
        workflow_type: WorkflowType::Triggered,
        schema: None,
        version: None,
        description: None,
        trigger: Some(TriggerDef::Polling {
            poll_command: vec![poll_script.to_string_lossy().into_owned()],
            context_command: Some(vec![executable_name("ctx.sh")]), // bare name — requires scripts_dir in PATH
            interval_secs: 3600,
            requires: None,
        }),
        workspace: None,
        resources: None,
        sandbox: None,
        steps: vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec![
                "bash".to_string(),
                "-c".to_string(),
                "test -f trigger-context/ctx.txt".to_string(),
            ]),
            ..step_def(StepType::Shell)
        }],
        finally: vec![],
        require: None,
    };

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let storage_clone = storage.clone();
    let handle = tokio::spawn(async move {
        engine.run(&wf, shutdown_clone, None).await.unwrap();
        storage_clone
    });

    // WHEN: wait for the run to complete
    let start = tokio::time::Instant::now();
    loop {
        if storage
            .runs()
            .iter()
            .any(|r| r.status == RunStatus::Completed)
        {
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

    // THEN: the run completed (context command was found via PATH and ctx.txt was written)
    assert!(
        storage
            .runs()
            .iter()
            .any(|r| r.status == RunStatus::Completed),
        "run should have completed — context command must be resolved via scripts_dir"
    );
}

// ── Finally step integration tests ───────────────────────────────────────────

#[tokio::test]
async fn finally_step_runs_on_success() {
    // GIVEN a workflow with one successful shell step and a finally shell step
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-finally-success",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "main".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );
    wf.finally = vec![FinallyStepDef {
        step: StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "cleanup".to_string()]),
            ..step_def(StepType::Shell)
        },
        on: None,
    }];

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
    assert!(
        logs.iter()
            .any(|l| l.step_type == "finally:shell" && l.stdout.contains("cleanup")),
        "finally:shell log not found"
    );
}

#[tokio::test]
async fn finally_step_runs_on_failure() {
    // GIVEN a workflow with a failing shell step and a finally shell step (no `on`)
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-finally-on-fail",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["false".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );
    wf.finally = vec![FinallyStepDef {
        step: StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "cleanup-on-fail".to_string()]),
            ..step_def(StepType::Shell)
        },
        on: None,
    }];

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    engine.run(&wf, shutdown, None).await.unwrap();

    // THEN — run is Failed AND finally step logged
    let runs = storage.runs();
    assert_eq!(runs.last().unwrap().status, RunStatus::Failed);
    let logs = storage.logs();
    assert!(
        logs.iter()
            .any(|l| l.step_type == "finally:shell" && l.stdout.contains("cleanup-on-fail")),
        "finally:shell log not found on failure"
    );
}

#[tokio::test]
async fn finally_step_filtered_on_success_only_skipped_on_failure() {
    // GIVEN a finally step with `on = [success]` and a failing main step
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-finally-filter-fail",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["false".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );
    wf.finally = vec![FinallyStepDef {
        step: StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "success-only".to_string()]),
            ..step_def(StepType::Shell)
        },
        on: Some(vec![RunOutcome::Success]),
    }];

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    engine.run(&wf, shutdown, None).await.unwrap();

    // THEN — finally step NOT executed (run failed, on = [success])
    let logs = storage.logs();
    assert!(
        !logs.iter().any(|l| l.step_type == "finally:shell"),
        "finally:shell should NOT have run when on=[success] and run failed"
    );
}

#[tokio::test]
async fn finally_step_filtered_on_failure_only_skipped_on_success() {
    // GIVEN a finally step with `on = [failed]` and a successful main step
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-finally-filter-success",
        WorkflowType::Looping,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "main".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );
    wf.finally = vec![FinallyStepDef {
        step: StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "fail-only".to_string()]),
            ..step_def(StepType::Shell)
        },
        on: Some(vec![RunOutcome::Failed]),
    }];

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

    // THEN — finally step NOT executed (run succeeded, on = [failed])
    let logs = storage.logs();
    assert!(
        !logs
            .iter()
            .any(|l| l.step_type == "finally:shell" && l.stdout.contains("fail-only")),
        "finally:shell should NOT have run when on=[failed] and run succeeded"
    );
}

#[tokio::test]
async fn finally_step_failure_does_not_change_run_status() {
    // GIVEN a successful main step and a failing finally step
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-finally-fail-no-status-change",
        WorkflowType::Triggered,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "main".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );
    wf.finally = vec![FinallyStepDef {
        step: StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["false".to_string()]),
            ..step_def(StepType::Shell)
        },
        on: None,
    }];

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    let result = engine.run_once(&wf, None, shutdown, None).await.unwrap();

    // THEN — run status is still Completed despite finally step failing
    assert_eq!(result, RunStatus::Completed);
    // AND — the finally step log entry is present (with exit_code 1)
    let logs = storage.logs();
    let finally_log = logs
        .iter()
        .find(|l| l.step_type == "finally:shell")
        .expect("finally:shell log not found");
    assert_eq!(finally_log.exit_code, Some(1));
}

#[tokio::test]
async fn finally_steps_continue_after_one_fails() {
    // GIVEN two finally steps where the first fails and the second succeeds
    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-finally-continue-after-fail",
        WorkflowType::Triggered,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "main".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );
    wf.finally = vec![
        FinallyStepDef {
            step: StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["false".to_string()]),
                ..step_def(StepType::Shell)
            },
            on: None,
        },
        FinallyStepDef {
            step: StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["echo".to_string(), "second-cleanup".to_string()]),
                ..step_def(StepType::Shell)
            },
            on: None,
        },
    ];

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    engine.run_once(&wf, None, shutdown, None).await.unwrap();

    // THEN — both finally step log entries are present
    let logs = storage.logs();
    let finally_logs: Vec<_> = logs
        .iter()
        .filter(|l| l.step_type == "finally:shell")
        .collect();
    assert_eq!(
        finally_logs.len(),
        2,
        "both finally steps should have logged"
    );
    assert!(finally_logs
        .iter()
        .any(|l| l.stdout.contains("second-cleanup")));
}

#[tokio::test]
async fn finally_steps_run_in_order() {
    // GIVEN two finally steps that write to files in order
    let dir = tempfile::tempdir().unwrap();
    let marker1 = dir.path().join("marker1.txt");
    let marker2 = dir.path().join("marker2.txt");

    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-finally-order",
        WorkflowType::Triggered,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "main".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );
    wf.workspace = Some(
        WorkspaceSource::Fixed {
            path: dir.path().to_string_lossy().into_owned(),
        }
        .into(),
    );
    wf.finally = vec![
        FinallyStepDef {
            step: StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["touch".to_string(), "marker1.txt".to_string()]),
                ..step_def(StepType::Shell)
            },
            on: None,
        },
        FinallyStepDef {
            step: StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["touch".to_string(), "marker2.txt".to_string()]),
                ..step_def(StepType::Shell)
            },
            on: None,
        },
    ];

    // WHEN
    let shutdown = Arc::new(AtomicBool::new(false));
    engine.run_once(&wf, None, shutdown, None).await.unwrap();

    // THEN — both markers exist, and finally log step_indices are main_steps.len() and main_steps.len()+1
    assert!(marker1.exists(), "marker1.txt should exist");
    assert!(marker2.exists(), "marker2.txt should exist");
    let logs = storage.logs();
    let finally_logs: Vec<_> = logs
        .iter()
        .filter(|l| l.step_type == "finally:shell")
        .collect();
    assert_eq!(finally_logs.len(), 2);
    assert_eq!(finally_logs[0].step_index, 1); // 1 main step + 0
    assert_eq!(finally_logs[1].step_index, 2); // 1 main step + 1
}

#[tokio::test]
async fn shutdown_between_steps_sets_stopped_status_and_runs_finally() {
    // GIVEN a workflow with two shell steps and a finally step with on=[stopped]
    // Shutdown fires before the second step — run should be Stopped and finally should run.
    let dir = tempfile::tempdir().unwrap();
    let finally_marker = dir.path().join("finally.txt");

    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-shutdown-between-steps",
        WorkflowType::Triggered,
        vec![
            StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["echo".to_string(), "step0".to_string()]),
                ..step_def(StepType::Shell)
            },
            StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["echo".to_string(), "step1".to_string()]),
                ..step_def(StepType::Shell)
            },
        ],
    );
    wf.workspace = Some(
        WorkspaceSource::Fixed {
            path: dir.path().to_string_lossy().into_owned(),
        }
        .into(),
    );
    wf.finally = vec![FinallyStepDef {
        step: StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["touch".to_string(), "finally.txt".to_string()]),
            ..step_def(StepType::Shell)
        },
        on: Some(vec![RunOutcome::Stopped]),
    }];

    // WHEN — shutdown already set, so it fires at the start of step 0 (before any step runs)
    let shutdown = Arc::new(AtomicBool::new(true));
    engine.run_once(&wf, None, shutdown, None).await.unwrap();

    // THEN — run status is Stopped and the on=[stopped] finally step ran
    assert_eq!(storage.runs().last().unwrap().status, RunStatus::Stopped);
    assert!(
        finally_marker.exists(),
        "on=[stopped] finally step should have run"
    );
}

#[tokio::test]
async fn run_finally_executes_stopped_finally_steps() {
    // GIVEN a workflow def with on=[stopped] finally step and a run in Stopped status
    let dir = tempfile::tempdir().unwrap();
    let finally_marker = dir.path().join("finally.txt");

    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-run-finally",
        WorkflowType::Triggered,
        vec![StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".to_string(), "main".to_string()]),
            ..step_def(StepType::Shell)
        }],
    );
    wf.workspace = Some(
        WorkspaceSource::Fixed {
            path: dir.path().to_string_lossy().into_owned(),
        }
        .into(),
    );
    wf.finally = vec![
        FinallyStepDef {
            step: StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["touch".to_string(), "finally.txt".to_string()]),
                ..step_def(StepType::Shell)
            },
            on: Some(vec![RunOutcome::Stopped]),
        },
        FinallyStepDef {
            step: StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["echo".to_string(), "success-only".to_string()]),
                ..step_def(StepType::Shell)
            },
            on: Some(vec![RunOutcome::Success]),
        },
    ];

    // Build a run as if it was killed (status Stopped, already saved)
    let mut run = WorkflowRun::new(wf.name.clone());
    run.status = RunStatus::Stopped;
    run.workspace_dir = Some(dir.path().canonicalize().unwrap());
    storage.save_workflow_run(&run).unwrap();

    // WHEN — engine.run_finally called directly (simulates run_finally_after_kill)
    engine
        .run_finally(&wf, &run, RunOutcome::Stopped, None)
        .await;

    // THEN — only the on=[stopped] step ran
    let logs = storage.logs();
    let finally_logs: Vec<_> = logs
        .iter()
        .filter(|l| l.step_type == "finally:shell")
        .collect();
    assert_eq!(finally_logs.len(), 1, "only one finally step should run");
    assert!(
        finally_marker.exists(),
        "on=[stopped] finally step should have created the marker"
    );
    assert!(
        !logs
            .iter()
            .any(|l| l.step_type == "finally:shell" && l.stdout.contains("success-only")),
        "on=[success] step should NOT have run"
    );
}

#[tokio::test]
async fn run_finally_releases_pool_slot_when_finally_is_empty() {
    use crate::types::PoolConfig;

    // GIVEN — a workflow with a pooled git workspace and NO [[finally]] block,
    // and a run whose workspace_dir points to an already-locked pool slot.
    // (We don't need a real worktree: cleanup only removes the lock dir.)
    let pool_dir = tempfile::tempdir().unwrap();
    let slot_path = pool_dir.path().join("slot-0");
    let lock_path = pool_dir.path().join("slot-0.lock");
    std::fs::create_dir(&slot_path).unwrap();
    std::fs::create_dir(&lock_path).unwrap();
    let base_repo = tempfile::tempdir().unwrap();

    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-run-finally-cleanup-no-finally",
        WorkflowType::Triggered,
        vec![step_def(StepType::Shell)],
    );
    wf.workspace = Some(WorkspaceConfig {
        source: WorkspaceSource::Git {
            base_repo: base_repo.path().to_string_lossy().into_owned(),
            ref_: None,
        },
        pool: Some(PoolConfig {
            dir: pool_dir.path().to_string_lossy().into_owned(),
            keep_directory_on: vec![],
        }),
    });
    // finally is intentionally empty — this is the regression case.
    assert!(wf.finally.is_empty());

    let mut run = WorkflowRun::new(wf.name.clone());
    run.status = RunStatus::Stopped;
    run.workspace_dir = Some(slot_path.clone());
    storage.save_workflow_run(&run).unwrap();

    // WHEN — engine.run_finally is called (simulates run_finally_after_kill)
    engine
        .run_finally(&wf, &run, RunOutcome::Stopped, None)
        .await;

    // THEN — the slot lock was released even though no user finally steps ran
    assert!(
        !lock_path.exists(),
        "pool slot lock must be released on stop even when [[finally]] is empty"
    );
}

#[tokio::test]
async fn run_finally_uses_stored_workspace_not_script() {
    // GIVEN a workflow with a script workspace that returns a DIFFERENT path each call,
    // and a run whose workspace_dir was already resolved to a specific path.
    // run_finally must use the stored path, not re-run the script.
    let dir = tempfile::tempdir().unwrap();
    let original_ws = dir.path().join("original-slot");
    let wrong_ws = dir.path().join("wrong-slot");
    std::fs::create_dir_all(&original_ws).unwrap();
    std::fs::create_dir_all(&wrong_ws).unwrap();

    // Workspace script always returns wrong-slot (simulates pool allocating a new slot)
    let ws_script = write_executable_script(
        dir.path(),
        "workspace.sh",
        &format!("#!/bin/bash\necho '{}'", wrong_ws.display()),
    )
    .unwrap();

    let storage = Arc::new(InMemoryStorage::new());
    let engine = make_engine(storage.clone());
    let mut wf = workflow(
        "test-run-finally-stored-ws",
        WorkflowType::Triggered,
        vec![step_def(StepType::Shell)],
    );
    wf.workspace = Some(
        WorkspaceSource::Script {
            command: vec![ws_script.to_string_lossy().into_owned()],
            requires: None,
        }
        .into(),
    );
    wf.finally = vec![FinallyStepDef {
        step: StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["touch".to_string(), "finally-ran.txt".to_string()]),
            ..step_def(StepType::Shell)
        },
        on: None,
    }];

    // Build a run as if it was killed — workspace_dir points to original-slot
    let mut run = WorkflowRun::new(wf.name.clone());
    run.status = RunStatus::Stopped;
    run.workspace_dir = Some(original_ws.clone());
    storage.save_workflow_run(&run).unwrap();

    // WHEN
    engine
        .run_finally(&wf, &run, RunOutcome::Stopped, None)
        .await;

    // THEN — finally step ran in original-slot, NOT in wrong-slot
    assert!(
        original_ws.join("finally-ran.txt").exists(),
        "finally step should have run in the original workspace (original-slot)"
    );
    assert!(
        !wrong_ws.join("finally-ran.txt").exists(),
        "finally step must NOT run in the re-resolved workspace (wrong-slot)"
    );
}
