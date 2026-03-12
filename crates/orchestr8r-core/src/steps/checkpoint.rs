use super::StepExecutor;
use crate::types::{CheckpointResponse, StepContext, StepDef, StepError, StepOutput, EngineEvent};
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
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    let _ = tx
        .send(EngineEvent::CheckpointPending {
            run_id: ctx.run_id,
            step_index: ctx.step_index,
            message: message.to_string(),
            feedback_available: ctx.feedback_available,
            response_tx,
        })
        .await;

    match response_rx.await {
        Ok(CheckpointResponse::Continue) => Ok(StepOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            accepted: Some(true),
            feedback: None,
        }),
        Ok(CheckpointResponse::Stop) => Err(StepError::Rejected),
        Ok(CheckpointResponse::Feedback(text)) => Ok(StepOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            accepted: None,
            feedback: Some(text),
        }),
        Err(_) => Err(StepError::Rejected),
    }
}

