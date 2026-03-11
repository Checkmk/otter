use async_trait::async_trait;
use crate::types::{StepDef, StepContext, StepOutput, StepError};
use super::StepExecutor;

pub struct CheckpointExecutor;

#[async_trait]
impl StepExecutor for CheckpointExecutor {
    fn step_type(&self) -> &'static str {
        "checkpoint"
    }

    async fn execute(&self, step_def: &StepDef, _ctx: &StepContext) -> Result<StepOutput, StepError> {
        let message = step_def.message.as_deref().unwrap_or("Checkpoint reached");

        println!("\n[CHECKPOINT] {}", message);
        println!("Type 'accept' to continue or 'reject' to stop: ");

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        let input = input.trim().to_lowercase();

        let accepted = input == "accept";

        if !accepted {
            return Err(StepError::Rejected);
        }

        Ok(StepOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            accepted: Some(true),
        })
    }
}
