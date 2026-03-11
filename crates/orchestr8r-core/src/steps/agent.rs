use async_trait::async_trait;
use crate::types::{StepDef, StepContext, StepOutput, StepError};
use super::StepExecutor;

pub struct AgentExecutor;

#[async_trait]
impl StepExecutor for AgentExecutor {
    fn step_type(&self) -> &'static str {
        "agent"
    }

    async fn execute(&self, step_def: &StepDef, ctx: &StepContext) -> Result<StepOutput, StepError> {
        let command = step_def.command.as_ref().ok_or_else(|| {
            StepError::ExecutionFailed("agent step missing command".to_string())
        })?;

        if command.is_empty() {
            return Err(StepError::ExecutionFailed("empty command".to_string()));
        }

        let message = step_def.message.as_ref().ok_or_else(|| {
            StepError::ExecutionFailed("agent step missing message".to_string())
        })?;

        let output = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .arg(message)
            .current_dir(&ctx.scratch_dir)
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

        let output_path = ctx.scratch_dir.join(format!("step-{}-output.md", ctx.step_index));
        tokio::fs::write(&output_path, &stdout).await?;

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
    use crate::types::StepDef;
    use uuid::Uuid;

    fn ctx(scratch_dir: std::path::PathBuf, step_index: usize) -> StepContext {
        StepContext {
            run_id: Uuid::new_v4(),
            workflow_name: "test".to_string(),
            iteration: 0,
            step_index,
            scratch_dir,
        }
    }

    fn step(command: Option<Vec<&str>>, message: Option<&str>) -> StepDef {
        StepDef {
            step_type: "agent".to_string(),
            command: command.map(|c| c.into_iter().map(String::from).collect()),
            message: message.map(String::from),
        }
    }

    #[tokio::test]
    async fn passes_message_as_final_arg() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let s = step(Some(vec!["echo"]), Some("hello agent"));

        // WHEN
        let result = AgentExecutor.execute(&s, &ctx(dir.path().to_path_buf(), 0)).await;

        // THEN
        assert!(result.is_ok());
        assert!(result.unwrap().stdout.contains("hello agent"));
    }

    #[tokio::test]
    async fn writes_output_to_scratch_dir() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let s = step(Some(vec!["echo"]), Some("written output"));

        // WHEN
        AgentExecutor.execute(&s, &ctx(dir.path().to_path_buf(), 2)).await.unwrap();

        // THEN
        let contents = std::fs::read_to_string(dir.path().join("step-2-output.md")).unwrap();
        assert!(contents.contains("written output"));
    }

    #[tokio::test]
    async fn fails_without_message() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let s = step(Some(vec!["echo"]), None);

        // WHEN
        let result = AgentExecutor.execute(&s, &ctx(dir.path().to_path_buf(), 0)).await;

        // THEN
        assert!(matches!(result, Err(StepError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn fails_without_command() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let s = step(None, Some("hello"));

        // WHEN
        let result = AgentExecutor.execute(&s, &ctx(dir.path().to_path_buf(), 0)).await;

        // THEN
        assert!(matches!(result, Err(StepError::ExecutionFailed(_))));
    }
}
