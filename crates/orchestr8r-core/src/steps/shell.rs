use super::StepExecutor;
use crate::process::{inject_isolated_env, PrependScriptsDir};
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

        let display_cmd = command.join(" ");
        let command = ctx.resource_limiter.apply(command);
        let mut cmd = tokio::process::Command::new(&command[0]);
        cmd.args(&command[1..]).current_dir(working_dir).kill_on_drop(true);

        if let Some(ref names) = step_def.secrets {
            let resolved = ctx
                .secret_store
                .resolve(names)
                .map_err(|e| StepError::ExecutionFailed(e.to_string()))?;
            inject_isolated_env(&mut cmd, &resolved);
        }

        cmd.prepend_scripts_dir(ctx.scripts_dir.as_deref());
        let output = cmd.output().await?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code();

        if !output.status.success() {
            return Err(StepError::ExecutionFailed(format!(
                "'{}' exited with code {}\nstdout: {}\nstderr: {}",
                display_cmd,
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
    use crate::resource_limiter::NoOpLimiter;
    use crate::types::{StepDef, StepType};
    use std::sync::Arc;
    use uuid::Uuid;

    fn ctx(scratch: &std::path::Path) -> StepContext {
        StepContext {
            run_id: Uuid::new_v4(),
            workflow_name: "test".into(),
            iteration: 0,
            step_index: 0,
            scratch_dir: scratch.to_path_buf(),
            workspace_dir: None,
            scripts_dir: None,
            checkpoint_tx: None,
            session_manager: None,
            notifier: std::sync::Arc::new(orchestr8r_notify::NoOpNotifier),
            log_fn: None,
            progress_fn: None,
            resource_limiter: Arc::new(NoOpLimiter),
            secret_store: Arc::new(orchestr8r_secrets::NoOpSecretStore),
        }
    }

    fn step(command: Vec<&str>) -> StepDef {
        StepDef {
            step_type: StepType::Shell,
            command: Some(command.into_iter().map(String::from).collect()),
            message: None,
            session: None,
            notify: None,
            secrets: None,
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

    #[tokio::test]
    async fn secrets_field_isolates_env_and_injects_declared_secret() {
        use orchestr8r_secrets::{FileSecretStore, SecretStore as _};

        // GIVEN a store with one secret and a daemon env var that should not leak
        let dir = tempfile::tempdir().unwrap();
        let store = FileSecretStore::new(dir.path().join("secrets.toml"));
        store.set("MY_SECRET", "hunter2").unwrap();

        let scratch = tempfile::tempdir().unwrap();
        let mut ctx = ctx(scratch.path());
        ctx.secret_store = Arc::new(store);

        // A canary env var set only in the daemon process
        std::env::set_var("ORCHESTR8R_CANARY", "should_not_leak");

        let step_def = StepDef {
            secrets: Some(vec!["MY_SECRET".into()]),
            ..step(vec!["bash", "-c", "echo secret=$MY_SECRET canary=${ORCHESTR8R_CANARY:-absent}"])
        };

        // WHEN
        let out = ShellExecutor.execute(&step_def, &ctx).await.unwrap();

        // THEN: declared secret is present, canary is absent
        assert!(out.stdout.contains("secret=hunter2"), "secret not injected: {}", out.stdout);
        assert!(out.stdout.contains("canary=absent"), "daemon env leaked: {}", out.stdout);
    }

    #[tokio::test]
    async fn secrets_field_fails_step_when_secret_not_in_store() {
        // GIVEN an empty store and a step that declares a missing secret
        let dir = tempfile::tempdir().unwrap();
        let store = orchestr8r_secrets::FileSecretStore::new(dir.path().join("secrets.toml"));
        let scratch = tempfile::tempdir().unwrap();
        let mut ctx = ctx(scratch.path());
        ctx.secret_store = Arc::new(store);

        let step_def = StepDef {
            secrets: Some(vec!["MISSING_KEY".into()]),
            ..step(vec!["echo", "hello"])
        };

        // WHEN
        let err = ShellExecutor.execute(&step_def, &ctx).await.unwrap_err();

        // THEN
        assert!(err.to_string().contains("MISSING_KEY"), "error should name the missing key: {err}");
    }
}
