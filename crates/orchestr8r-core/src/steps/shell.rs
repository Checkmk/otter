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
                "'{}' exited with code {}\nstdout: {}\nstderr: {}",
                command.join(" "),
                exit_code.unwrap_or(-1),
                stdout.trim(),
                stderr.trim(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StepDef, StepType};
    use uuid::Uuid;

    fn ctx(scratch: &std::path::Path) -> StepContext {
        StepContext {
            run_id: Uuid::new_v4(),
            workflow_name: "test".into(),
            iteration: 0,
            step_index: 0,
            scratch_dir: scratch.to_path_buf(),
            workspace_dir: None,
            checkpoint_tx: None,
            session_manager: None,
            notifier: std::sync::Arc::new(orchestr8r_notify::NoOpNotifier),
            log_fn: None,
            progress_fn: None,
        }
    }

    fn step(command: Vec<&str>) -> StepDef {
        StepDef {
            step_type: StepType::Shell,
            command: Some(command.into_iter().map(String::from).collect()),
            message: None,
            session: None,
            notify: None,
            agent: Default::default(),
        }
    }

    #[tokio::test]
    async fn failed_command_includes_command_name_in_error() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let step_def = step(vec!["bash", "-c", "exit 1"]);

        // WHEN
        let err = ShellExecutor.execute(&step_def, &ctx(scratch.path())).await.unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(msg.contains("bash -c exit 1"), "error should contain the command: {msg}");
    }

    #[tokio::test]
    async fn failed_command_includes_exit_code_in_error() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let step_def = step(vec!["bash", "-c", "exit 42"]);

        // WHEN
        let err = ShellExecutor.execute(&step_def, &ctx(scratch.path())).await.unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(msg.contains("42"), "error should contain the exit code: {msg}");
    }

    #[tokio::test]
    async fn failed_command_includes_stderr_in_error() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let step_def = step(vec!["bash", "-c", "echo 'something went wrong' >&2; exit 1"]);

        // WHEN
        let err = ShellExecutor.execute(&step_def, &ctx(scratch.path())).await.unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(msg.contains("something went wrong"), "error should contain stderr: {msg}");
    }

    #[tokio::test]
    async fn failed_command_includes_stdout_in_error() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let step_def = step(vec!["bash", "-c", "echo 'partial output'; exit 1"]);

        // WHEN
        let err = ShellExecutor.execute(&step_def, &ctx(scratch.path())).await.unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(msg.contains("partial output"), "error should contain stdout: {msg}");
    }
}
