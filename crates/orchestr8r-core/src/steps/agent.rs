use super::StepExecutor;
use crate::types::{StepContext, StepDef, StepError, StepOutput};
use async_trait::async_trait;

pub struct AgentExecutor;

#[async_trait]
impl StepExecutor for AgentExecutor {
    fn step_type(&self) -> crate::types::StepType {
        crate::types::StepType::Agent
    }

    async fn execute(
        &self,
        step_def: &StepDef,
        ctx: &StepContext,
    ) -> Result<StepOutput, StepError> {
        let command = step_def
            .command
            .as_ref()
            .ok_or_else(|| StepError::ExecutionFailed("agent step missing command".to_string()))?;

        if command.is_empty() {
            return Err(StepError::ExecutionFailed("empty command".to_string()));
        }

        let message = step_def
            .message
            .as_ref()
            .ok_or_else(|| StepError::ExecutionFailed("agent step missing message".to_string()))?;

        let working_dir = ctx.workspace_dir.as_ref().unwrap_or(&ctx.scratch_dir);

        let mut child = tokio::process::Command::new(&command[0])
            .args(&command[1..])
            .current_dir(working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(message.as_bytes()).await?;
        }

        let output = child.wait_with_output().await?;

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

        let output_path = ctx
            .scratch_dir
            .join(format!("step-{}-output.md", ctx.step_index));
        tokio::fs::write(&output_path, &stdout).await?;

        if let Some(output_file) = &step_def.output_file {
            let path = working_dir.join(output_file);
            if !path.exists() {
                return Err(StepError::ExecutionFailed(format!(
                    "expected output file '{}' not found in {}",
                    output_file,
                    working_dir.display()
                )));
            }
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

    fn ctx(scratch_dir: std::path::PathBuf, step_index: usize) -> StepContext {
        StepContext {
            run_id: Uuid::new_v4(),
            workflow_name: "test".to_string(),
            iteration: 0,
            step_index,
            scratch_dir,
            workspace_dir: None,
        }
    }

    fn step(command: Option<Vec<&str>>, message: Option<&str>) -> StepDef {
        StepDef {
            step_type: StepType::Agent,
            command: command.map(|c| c.into_iter().map(String::from).collect()),
            message: message.map(String::from),
            path: None,
            output_file: None,
        }
    }

    #[tokio::test]
    async fn passes_message_via_stdin() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let s = step(Some(vec!["cat"]), Some("hello agent"));

        // WHEN
        let result = AgentExecutor
            .execute(&s, &ctx(dir.path().to_path_buf(), 0))
            .await;

        // THEN
        assert!(result.is_ok());
        assert!(result.unwrap().stdout.contains("hello agent"));
    }

    #[tokio::test]
    async fn writes_output_to_scratch_dir() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let s = step(Some(vec!["cat"]), Some("written output"));

        // WHEN
        AgentExecutor
            .execute(&s, &ctx(dir.path().to_path_buf(), 2))
            .await
            .unwrap();

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
        let result = AgentExecutor
            .execute(&s, &ctx(dir.path().to_path_buf(), 0))
            .await;

        // THEN
        assert!(matches!(result, Err(StepError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn succeeds_when_output_file_exists_in_scratch() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        std::fs::write(scratch.path().join("plan.md"), "the plan").unwrap();
        let mut s = step(Some(vec!["echo"]), Some("hello"));
        s.output_file = Some("plan.md".to_string());

        // WHEN
        let result = AgentExecutor
            .execute(&s, &ctx(scratch.path().to_path_buf(), 0))
            .await;

        // THEN
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn fails_when_output_file_missing_from_scratch() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let mut s = step(Some(vec!["echo"]), Some("hello"));
        s.output_file = Some("plan.md".to_string());

        // WHEN
        let result = AgentExecutor
            .execute(&s, &ctx(scratch.path().to_path_buf(), 0))
            .await;

        // THEN
        assert!(matches!(result, Err(StepError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn fails_without_command() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let s = step(None, Some("hello"));

        // WHEN
        let result = AgentExecutor
            .execute(&s, &ctx(dir.path().to_path_buf(), 0))
            .await;

        // THEN
        assert!(matches!(result, Err(StepError::ExecutionFailed(_))));
    }
}
