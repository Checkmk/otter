pub mod shell;
pub mod checkpoint;
pub mod agent;

use async_trait::async_trait;
use crate::types::{StepContext, StepOutput, StepError};

#[async_trait]
pub trait StepExecutor: Send + Sync {
    async fn execute(&self, step_def: &crate::types::StepDef, ctx: &StepContext) -> Result<StepOutput, StepError>;
    fn step_type(&self) -> &'static str;
}

pub fn registry() -> Vec<Box<dyn StepExecutor>> {
    vec![
        Box::new(shell::ShellExecutor),
        Box::new(checkpoint::CheckpointExecutor),
        Box::new(agent::AgentExecutor),
    ]
}
