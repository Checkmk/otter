use super::StepExecutor;
use crate::types::{CheckpointResponse, EngineEvent, LogEntry, StepContext, StepDef, StepError, StepOutput};
use async_trait::async_trait;
use chrono::Utc;
use orchestr8r_notify::Notification;

pub struct CheckpointExecutor;

#[async_trait]
impl StepExecutor for CheckpointExecutor {
    fn step_type(&self) -> crate::types::StepType {
        crate::types::StepType::Checkpoint
    }

    async fn execute(
        &self,
        step_def: &StepDef,
        ctx: &StepContext,
    ) -> Result<StepOutput, StepError> {
        let message = step_def.message.as_deref().unwrap_or("Checkpoint reached");

        let tx = ctx.checkpoint_tx.as_ref().ok_or_else(|| {
            StepError::ExecutionFailed("no checkpoint handler configured".to_string())
        })?;
        execute_via_channel(tx, ctx, message).await
    }
}

async fn execute_via_channel(
    tx: &tokio::sync::mpsc::Sender<EngineEvent>,
    ctx: &StepContext,
    message: &str,
) -> Result<StepOutput, StepError> {
    loop {
        let feedback_available = ctx
            .session_manager
            .as_ref()
            .map_or(false, |m| m.has_active_session());

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let _ = tx
            .send(EngineEvent::CheckpointPending {
                run_id: ctx.run_id,
                step_index: ctx.step_index,
                message: message.to_string(),
                feedback_available,
                response_tx,
            })
            .await;

        let _ = ctx
            .notifier
            .send(&Notification {
                summary: "orchestr8r — checkpoint".to_string(),
                body: message.to_string(),
            })
            .await;

        match response_rx.await {
            Ok(CheckpointResponse::Continue) => {
                return Ok(StepOutput {
                    stdout: "Continue".to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    accepted: Some(true),
                });
            }
            Ok(CheckpointResponse::Stop) => return Err(StepError::Rejected),
            Ok(CheckpointResponse::Feedback(text)) => {
                // Log feedback immediately so it appears before the checkpoint is re-presented.
                log_immediate(ctx, "feedback", text.clone(), String::new(), None);

                if let Some(manager) = &ctx.session_manager {
                    match manager.prompt_last(&text).await {
                        Ok(Some(agent_out)) => {
                            log_immediate(ctx, "agent", agent_out.stdout, agent_out.stderr, agent_out.exit_code);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "Agent feedback prompt failed");
                        }
                    }
                }
                // loop: present checkpoint again
            }
            Err(_) => return Err(StepError::Rejected),
        }
    }
}

fn log_immediate(ctx: &StepContext, step_type: &str, stdout: String, stderr: String, exit_code: Option<i32>) {
    if let Some(log) = &ctx.log_fn {
        log(LogEntry {
            run_id: ctx.run_id,
            iteration: ctx.iteration,
            step_index: ctx.step_index,
            step_type: step_type.to_string(),
            stdout,
            stderr,
            exit_code,
            accepted: None,
            feedback: None,
            timestamp: Utc::now(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runner::{AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec};
    use crate::session::AgentSessionManager;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    struct MockRunner {
        calls: Mutex<Vec<String>>,
    }

    impl MockRunner {
        fn new() -> Arc<Self> {
            Arc::new(Self { calls: Mutex::new(Vec::new()) })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentRunner for MockRunner {
        async fn start(&self, spec: AgentSpec) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
            self.calls.lock().unwrap().push("start".into());
            Ok((
                AgentSessionHandle { id: "s1".into(), working_dir: spec.working_dir },
                AgentOutput { stdout: "initial".into(), stderr: String::new(), exit_code: Some(0) },
            ))
        }

        async fn prompt(&self, _session: &AgentSessionHandle, message: &str) -> Result<AgentOutput, AgentError> {
            self.calls.lock().unwrap().push(format!("prompt:{}", message));
            Ok(AgentOutput { stdout: format!("response:{}", message), stderr: String::new(), exit_code: Some(0) })
        }

        async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
            Ok(())
        }
    }

    fn test_context(run_id: Uuid, tx: tokio::sync::mpsc::Sender<EngineEvent>) -> StepContext {
        StepContext {
            run_id,
            workflow_name: "test".into(),
            iteration: 1,
            step_index: 0,
            scratch_dir: PathBuf::from("/tmp"),
            workspace_dir: None,
            checkpoint_tx: Some(tx),
            session_manager: None,
            notifier: Arc::new(orchestr8r_notify::NoOpNotifier),
            log_fn: None,
        }
    }

    fn test_context_with_manager(
        run_id: Uuid,
        tx: tokio::sync::mpsc::Sender<EngineEvent>,
        manager: Arc<AgentSessionManager>,
    ) -> StepContext {
        StepContext {
            run_id,
            workflow_name: "test".into(),
            iteration: 1,
            step_index: 0,
            scratch_dir: PathBuf::from("/tmp"),
            workspace_dir: None,
            checkpoint_tx: Some(tx),
            session_manager: Some(manager),
            notifier: Arc::new(orchestr8r_notify::NoOpNotifier),
            log_fn: None,
        }
    }

    #[tokio::test]
    async fn checkpoint_continue_returns_output() {
        // GIVEN a checkpoint with Continue response
        let run_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        tokio::spawn(async move {
            if let Some(EngineEvent::CheckpointPending { response_tx, .. }) = rx.recv().await {
                let _ = response_tx.send(CheckpointResponse::Continue);
            }
        });

        let ctx = test_context(run_id, tx.clone());

        // WHEN
        let result = execute_via_channel(&tx, &ctx, "Continue?").await;

        // THEN — Continue returns success
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.accepted, Some(true));
    }

    #[tokio::test]
    async fn checkpoint_stop_returns_rejected() {
        // GIVEN a checkpoint with Stop response
        let run_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        tokio::spawn(async move {
            if let Some(EngineEvent::CheckpointPending { response_tx, .. }) = rx.recv().await {
                let _ = response_tx.send(CheckpointResponse::Stop);
            }
        });

        let ctx = test_context(run_id, tx.clone());

        // WHEN
        let result = execute_via_channel(&tx, &ctx, "Stop?").await;

        // THEN — Stop returns Rejected error
        assert!(matches!(result, Err(StepError::Rejected)));
    }

    #[tokio::test]
    async fn checkpoint_feedback_reprompts_agent_and_loops() {
        // GIVEN a checkpoint with feedback, followed by Continue
        let run_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        let feedback_sent = Arc::new(Mutex::new(false));
        let feedback_sent_clone = feedback_sent.clone();

        tokio::spawn(async move {
            let mut count = 0;
            while let Some(EngineEvent::CheckpointPending { response_tx, .. }) = rx.recv().await {
                if count == 0 && !*feedback_sent_clone.lock().unwrap() {
                    // First checkpoint: send feedback
                    *feedback_sent_clone.lock().unwrap() = true;
                    let _ = response_tx.send(CheckpointResponse::Feedback("fix this".into()));
                    count += 1;
                } else {
                    // Second checkpoint (after feedback): continue
                    let _ = response_tx.send(CheckpointResponse::Continue);
                    break;
                }
            }
        });

        // Set up a session manager with an active agent session
        let runner = MockRunner::new();
        let manager = AgentSessionManager::new_with_runner_override(runner.clone());
        manager
            .run_step(
                Some("test_session"),
                &crate::types::AgentConfig::default(),
                Some(&["echo".to_string()]),
                "initial",
                std::path::Path::new("/tmp"),
            )
            .await
            .unwrap();

        let ctx = test_context_with_manager(run_id, tx.clone(), Arc::new(manager));

        // WHEN
        let result = execute_via_channel(&tx, &ctx, "Review?").await;

        // THEN — loops, prompts agent with feedback, returns success
        // (feedback and agent entries are emitted immediately via log_fn)
        assert!(result.is_ok());

        // Runner should have been called twice: start + prompt with feedback
        let calls = runner.calls();
        assert!(calls.contains(&"start".into()));
        assert!(calls.contains(&"prompt:fix this".into()));
    }

    #[tokio::test]
    async fn checkpoint_feedback_available_with_active_session() {
        // GIVEN a checkpoint with an active agent session
        let run_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        let feedback_available_seen = Arc::new(Mutex::new(None));
        let seen_clone = feedback_available_seen.clone();

        tokio::spawn(async move {
            if let Some(EngineEvent::CheckpointPending { feedback_available, response_tx, .. }) = rx.recv().await {
                *seen_clone.lock().unwrap() = Some(feedback_available);
                let _ = response_tx.send(CheckpointResponse::Continue);
            }
        });

        // Set up a session manager with an active agent session
        let runner = MockRunner::new();
        let manager = AgentSessionManager::new_with_runner_override(runner);
        manager
            .run_step(
                Some("test_session"),
                &crate::types::AgentConfig::default(),
                Some(&["echo".to_string()]),
                "initial",
                std::path::Path::new("/tmp"),
            )
            .await
            .unwrap();

        let ctx = test_context_with_manager(run_id, tx.clone(), Arc::new(manager));

        // WHEN
        let _ = execute_via_channel(&tx, &ctx, "Review?").await;

        // THEN — feedback_available was true
        assert_eq!(*feedback_available_seen.lock().unwrap(), Some(true));
    }

    #[tokio::test]
    async fn checkpoint_feedback_available_without_session() {
        // GIVEN a checkpoint with no active agent session
        let run_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        let feedback_available_seen = Arc::new(Mutex::new(None));
        let seen_clone = feedback_available_seen.clone();

        tokio::spawn(async move {
            if let Some(EngineEvent::CheckpointPending { feedback_available, response_tx, .. }) = rx.recv().await {
                *seen_clone.lock().unwrap() = Some(feedback_available);
                let _ = response_tx.send(CheckpointResponse::Continue);
            }
        });

        let ctx = test_context(run_id, tx.clone());

        // WHEN
        let _ = execute_via_channel(&tx, &ctx, "Review?").await;

        // THEN — feedback_available was false
        assert_eq!(*feedback_available_seen.lock().unwrap(), Some(false));
    }

    // ── Regression tests: feedback+agent must be logged before checkpoint re-presents ──

    #[tokio::test]
    async fn feedback_logged_immediately_before_second_checkpoint_pending() {
        // GIVEN a checkpoint that receives feedback then continue.
        // The log_fn must be called with "feedback" BEFORE the second CheckpointPending event.
        let run_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);

        // Interleave: record every EngineEvent and log_fn call in order.
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let events_rx = events.clone();

        tokio::spawn(async move {
            let mut count = 0;
            while let Some(ev) = rx.recv().await {
                if let EngineEvent::CheckpointPending { response_tx, .. } = ev {
                    events_rx.lock().unwrap().push(format!("checkpoint_pending:{}", count));
                    if count == 0 {
                        let _ = response_tx.send(CheckpointResponse::Feedback("my feedback".into()));
                    } else {
                        let _ = response_tx.send(CheckpointResponse::Continue);
                        break;
                    }
                    count += 1;
                }
            }
        });

        let runner = MockRunner::new();
        let manager = AgentSessionManager::new_with_runner_override(runner);
        manager
            .run_step(
                Some("s"),
                &crate::types::AgentConfig::default(),
                Some(&["echo".to_string()]),
                "hi",
                std::path::Path::new("/tmp"),
            )
            .await
            .unwrap();

        let logged: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
        let events_log = events.clone();
        let logged_clone = logged.clone();
        // Wrap log_fn to also record into `events` so we can compare ordering.
        let log_fn: Arc<dyn Fn(LogEntry) + Send + Sync> = Arc::new(move |entry: LogEntry| {
            events_log.lock().unwrap().push(format!("log:{}", entry.step_type));
            logged_clone.lock().unwrap().push(entry);
        });

        let ctx = StepContext {
            run_id,
            workflow_name: "test".into(),
            iteration: 1,
            step_index: 0,
            scratch_dir: PathBuf::from("/tmp"),
            workspace_dir: None,
            checkpoint_tx: Some(tx.clone()),
            session_manager: Some(Arc::new(manager)),
            notifier: Arc::new(orchestr8r_notify::NoOpNotifier),
            log_fn: Some(log_fn),
        };

        // WHEN
        let result = execute_via_channel(&tx, &ctx, "Review?").await;

        // THEN — succeeded
        assert!(result.is_ok());

        // Event order must be: checkpoint_pending:0, log:feedback, log:agent, checkpoint_pending:1
        let order = events.lock().unwrap().clone();
        let cp0 = order.iter().position(|s| s == "checkpoint_pending:0").unwrap();
        let log_fb = order.iter().position(|s| s == "log:feedback").unwrap();
        let log_ag = order.iter().position(|s| s == "log:agent").unwrap();
        let cp1 = order.iter().position(|s| s == "checkpoint_pending:1").unwrap();

        assert!(cp0 < log_fb, "feedback must be logged after first checkpoint");
        assert!(log_fb < log_ag, "agent must be logged after feedback");
        assert!(log_ag < cp1, "second checkpoint_pending must come after agent log");

        // log_fn received correct entries
        let entries = logged.lock().unwrap().clone();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].step_type, "feedback");
        assert_eq!(entries[0].stdout, "my feedback");
        assert_eq!(entries[1].step_type, "agent");
        assert!(entries[1].stdout.contains("response:my feedback"));
    }

    #[tokio::test]
    async fn continue_produces_no_log_fn_calls() {
        // GIVEN a checkpoint that receives Continue immediately.
        let run_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        tokio::spawn(async move {
            if let Some(EngineEvent::CheckpointPending { response_tx, .. }) = rx.recv().await {
                let _ = response_tx.send(CheckpointResponse::Continue);
            }
        });

        let logged: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
        let logged_clone = logged.clone();
        let ctx = StepContext {
            run_id,
            workflow_name: "test".into(),
            iteration: 1,
            step_index: 0,
            scratch_dir: PathBuf::from("/tmp"),
            workspace_dir: None,
            checkpoint_tx: Some(tx.clone()),
            session_manager: None,
            notifier: Arc::new(orchestr8r_notify::NoOpNotifier),
            log_fn: Some(Arc::new(move |e| { logged_clone.lock().unwrap().push(e); })),
        };

        // WHEN
        let result = execute_via_channel(&tx, &ctx, "ok?").await;

        // THEN — no log_fn calls; the "> Continue" entry is handled by the engine, not checkpoint
        assert!(result.is_ok());
        assert!(logged.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stop_produces_no_log_fn_calls() {
        // GIVEN a checkpoint that receives Stop.
        let run_id = Uuid::new_v4();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        tokio::spawn(async move {
            if let Some(EngineEvent::CheckpointPending { response_tx, .. }) = rx.recv().await {
                let _ = response_tx.send(CheckpointResponse::Stop);
            }
        });

        let logged: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
        let logged_clone = logged.clone();
        let ctx = StepContext {
            run_id,
            workflow_name: "test".into(),
            iteration: 1,
            step_index: 0,
            scratch_dir: PathBuf::from("/tmp"),
            workspace_dir: None,
            checkpoint_tx: Some(tx.clone()),
            session_manager: None,
            notifier: Arc::new(orchestr8r_notify::NoOpNotifier),
            log_fn: Some(Arc::new(move |e| { logged_clone.lock().unwrap().push(e); })),
        };

        // WHEN
        let result = execute_via_channel(&tx, &ctx, "stop?").await;

        // THEN — rejected, no log_fn calls
        assert!(matches!(result, Err(StepError::Rejected)));
        assert!(logged.lock().unwrap().is_empty());
    }
}
