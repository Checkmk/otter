use super::StepExecutor;
use crate::types::{StepContext, StepDef, StepError, StepOutput};
use async_trait::async_trait;

pub struct WorkspaceExecutor;

#[async_trait]
impl StepExecutor for WorkspaceExecutor {
    fn step_type(&self) -> crate::types::StepType {
        crate::types::StepType::Workspace
    }

    async fn execute(
        &self,
        step_def: &StepDef,
        _ctx: &StepContext,
    ) -> Result<StepOutput, StepError> {
        let raw_path = step_def
            .path
            .as_ref()
            .ok_or_else(|| StepError::ExecutionFailed("workspace step missing path".to_string()))?;

        let resolved = std::fs::canonicalize(raw_path).map_err(|e| {
            StepError::ExecutionFailed(format!(
                "cannot resolve workspace path '{}': {}",
                raw_path, e
            ))
        })?;

        if !resolved.is_dir() {
            return Err(StepError::ExecutionFailed(format!(
                "workspace path '{}' is not a directory",
                resolved.display()
            )));
        }

        Ok(StepOutput {
            stdout: resolved.to_string_lossy().to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            accepted: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StepDef;
    use uuid::Uuid;

    fn ctx() -> StepContext {
        StepContext {
            run_id: Uuid::new_v4(),
            workflow_name: "test".to_string(),
            iteration: 0,
            step_index: 0,
            scratch_dir: std::env::temp_dir(),
            workspace_dir: None,
        }
    }

    fn step(path: Option<&str>) -> StepDef {
        StepDef {
            step_type: crate::types::StepType::Workspace,
            command: None,
            message: None,
            path: path.map(String::from),
        }
    }

    #[tokio::test]
    async fn valid_directory_succeeds() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let s = step(Some(dir.path().to_str().unwrap()));

        // WHEN
        let result = WorkspaceExecutor.execute(&s, &ctx()).await;

        // THEN
        let output = result.unwrap();
        assert!(!output.stdout.is_empty());
    }

    #[tokio::test]
    async fn missing_path_errors() {
        // GIVEN
        let s = step(None);

        // WHEN
        let result = WorkspaceExecutor.execute(&s, &ctx()).await;

        // THEN
        assert!(matches!(result, Err(StepError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn nonexistent_path_errors() {
        // GIVEN
        let s = step(Some("/no/such/dir/ever"));

        // WHEN
        let result = WorkspaceExecutor.execute(&s, &ctx()).await;

        // THEN
        assert!(matches!(result, Err(StepError::ExecutionFailed(_))));
    }

    #[tokio::test]
    async fn file_path_errors() {
        // GIVEN
        let file = tempfile::NamedTempFile::new().unwrap();
        let s = step(Some(file.path().to_str().unwrap()));

        // WHEN
        let result = WorkspaceExecutor.execute(&s, &ctx()).await;

        // THEN
        assert!(matches!(result, Err(StepError::ExecutionFailed(_))));
    }
}
