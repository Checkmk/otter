use super::StepExecutor;
use crate::types::{CheckpointResponse, EngineEvent, StepContext, StepDef, StepError, StepOutput, SubStepLog};
use async_trait::async_trait;

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
