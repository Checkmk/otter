use super::StepExecutor;
use crate::types::{StepContext, StepDef, StepError, StepOutput};
use async_trait::async_trait;

pub struct ShellExecutor;

#[async_trait]
impl StepExecutor for ShellExecutor {
    fn step_type(&self) -> crate::types::StepType {
        crate::types::StepType::Shell
    }

    async fn execute(
        &self,
        step_def: &StepDef,
        ctx: &StepContext,
    ) -> Result<StepOutput, StepError> {
        let command = step_def
            .command
            .as_ref()
            .ok_or_else(|| StepError::ExecutionFailed("shell step missing command".to_string()))?;

        if command.is_empty() {
            return Err(StepError::ExecutionFailed("empty command".to_string()));
        }

        let working_dir = ctx.workspace_dir.as_ref().unwrap_or(&ctx.scratch_dir);

        let output = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .current_dir(working_dir)
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code();

        if !output.status.success() {
            return Err(StepError::ExecutionFailed(format!(
                "command exited with code {:?}",
                exit_code
            )));
        }

        Ok(StepOutput {
            stdout,
            stderr,
            exit_code,
            accepted: None,
        })
    }
}
