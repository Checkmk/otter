use async_trait::async_trait;
use crate::types::{StepDef, StepContext, StepOutput, StepError};
use super::StepExecutor;

pub struct ShellExecutor;

#[async_trait]
impl StepExecutor for ShellExecutor {
    fn step_type(&self) -> &'static str {
        "shell"
    }

    async fn execute(&self, step_def: &StepDef, _ctx: &StepContext) -> Result<StepOutput, StepError> {
        let command = step_def.command.as_ref().ok_or_else(|| {
            StepError::ExecutionFailed("shell step missing command".to_string())
        })?;

        if command.is_empty() {
            return Err(StepError::ExecutionFailed("empty command".to_string()));
        }

        let output = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code();

        if !stdout.is_empty() {
            print!("{}", stdout);
        }
        if !stderr.is_empty() {
            eprint!("{}", stderr);
        }

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
