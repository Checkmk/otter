pub mod agent;
pub mod checkpoint;
pub mod shell;
pub mod workspace;

use crate::types::{StepContext, StepError, StepOutput, StepType};
use async_trait::async_trait;

#[async_trait]
pub trait StepExecutor: Send + Sync {
    async fn execute(
        &self,
        step_def: &crate::types::StepDef,
        ctx: &StepContext,
    ) -> Result<StepOutput, StepError>;
    fn step_type(&self) -> StepType;
}

pub fn registry() -> Vec<Box<dyn StepExecutor>> {
    vec![
        Box::new(shell::ShellExecutor),
        Box::new(checkpoint::CheckpointExecutor),
        Box::new(agent::AgentExecutor),
        Box::new(workspace::WorkspaceExecutor),
    ]
}
