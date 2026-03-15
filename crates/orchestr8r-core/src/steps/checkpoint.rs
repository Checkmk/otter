use super::StepExecutor;
use crate::types::{CheckpointResponse, EngineEvent, StepContext, StepDef, StepError, StepOutput, SubStepLog};
use async_trait::async_trait;
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
    let mut extra_logs: Vec<SubStepLog> = Vec::new();

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
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    accepted: Some(true),
                    extra_logs,
                });
            }
            Ok(CheckpointResponse::Stop) => return Err(StepError::Rejected(extra_logs)),
            Ok(CheckpointResponse::Feedback(text)) => {
                if let Some(manager) = &ctx.session_manager {
                    match manager.prompt_last(&text).await {
                        Ok(Some(agent_out)) => {
                            extra_logs.push(SubStepLog {
                                step_type: "agent".to_string(),
                                stdout: agent_out.stdout,
                                stderr: agent_out.stderr,
                                exit_code: agent_out.exit_code,
                            });
                        }
                        Ok(None) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "Agent feedback prompt failed");
                        }
                    }
                }
                // loop: present checkpoint again with updated agent response
            }
            Err(_) => return Err(StepError::Rejected(extra_logs)),
        }
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
        assert_eq!(output.extra_logs.len(), 0);
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
        assert!(matches!(result, Err(StepError::Rejected(_))));
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

        // THEN — loops, prompts agent with feedback, and returns success
        assert!(result.is_ok());
        let output = result.unwrap();
        assert_eq!(output.extra_logs.len(), 1);
        assert_eq!(output.extra_logs[0].step_type, "agent");
        assert!(output.extra_logs[0].stdout.contains("response:fix this"));

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
}
