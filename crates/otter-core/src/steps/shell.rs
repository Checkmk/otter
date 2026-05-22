use super::StepExecutor;
use crate::process::build_subprocess_command;
use crate::requirements::resolve_requires;
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
        let resolved = resolve_requires(
            step_def.requires.as_deref().unwrap_or_default(),
            ctx.requirements.as_deref(),
            ctx.scripts_dir.as_deref(),
            ctx.secret_store.as_ref(),
            &ctx.workflow_name,
        )
        .map_err(|e| StepError::ExecutionFailed(e.to_string()))?;

        let command = if ctx.sandbox_config.is_none() {
            ctx.resource_limiter.apply(command)
        } else {
            command.to_vec()
        };
        let mut cmd = build_subprocess_command(
            &command,
            working_dir,
            ctx.scripts_dir.as_deref(),
            &resolved,
            ctx.sandbox_config.as_ref(),
        );
        cmd.kill_on_drop(true);
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

    struct PlaintextKeyProvider;
    impl otter_secrets::KeyProvider for PlaintextKeyProvider {
        fn encrypt(&self, plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
            Ok(plaintext.to_vec())
        }
        fn decrypt(&self, ciphertext: &[u8]) -> anyhow::Result<Vec<u8>> {
            Ok(ciphertext.to_vec())
        }
    }

    fn test_store(dir: &std::path::Path) -> otter_secrets::EncryptedSecretStore {
        otter_secrets::EncryptedSecretStore::new(
            dir.join("secrets.age"),
            Arc::new(PlaintextKeyProvider),
        )
    }

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
            notifier: std::sync::Arc::new(otter_notify::NoOpNotifier),
            log_fn: None,
            progress_fn: None,
            resource_limiter: Arc::new(NoOpLimiter),
            secret_store: Arc::new(otter_secrets::NoOpSecretStore),
            requirements: None,
            sandbox_config: None,
        }
    }

    fn step(command: Vec<&str>) -> StepDef {
        StepDef {
            step_type: StepType::Shell,
            command: Some(command.into_iter().map(String::from).collect()),
            message: None,
            session: None,
            notify: None,
            requires: None,
            sandbox: None,
            agent: Default::default(),
        }
    }

    #[tokio::test]
    async fn failed_command_includes_command_name_in_error() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let step_def = step(vec!["bash", "-c", "exit 1"]);

        // WHEN
        let err = ShellExecutor
            .execute(&step_def, &ctx(scratch.path()))
            .await
            .unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(
            msg.contains("bash -c exit 1"),
            "error should contain the command: {msg}"
        );
    }

    #[tokio::test]
    async fn failed_command_includes_exit_code_in_error() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let step_def = step(vec!["bash", "-c", "exit 42"]);

        // WHEN
        let err = ShellExecutor
            .execute(&step_def, &ctx(scratch.path()))
            .await
            .unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(
            msg.contains("42"),
            "error should contain the exit code: {msg}"
        );
    }

    #[tokio::test]
    async fn failed_command_includes_stderr_in_error() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let step_def = step(vec![
            "bash",
            "-c",
            "echo 'something went wrong' >&2; exit 1",
        ]);

        // WHEN
        let err = ShellExecutor
            .execute(&step_def, &ctx(scratch.path()))
            .await
            .unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(
            msg.contains("something went wrong"),
            "error should contain stderr: {msg}"
        );
    }

    #[tokio::test]
    async fn failed_command_includes_stdout_in_error() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let step_def = step(vec!["bash", "-c", "echo 'partial output'; exit 1"]);

        // WHEN
        let err = ShellExecutor
            .execute(&step_def, &ctx(scratch.path()))
            .await
            .unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(
            msg.contains("partial output"),
            "error should contain stdout: {msg}"
        );
    }

    #[tokio::test]
    async fn secrets_field_isolates_env_and_injects_declared_secret() {
        use otter_secrets::SecretStore as _;

        // GIVEN a store with one secret and a daemon env var that should not leak
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        store.set("MY_SECRET", "hunter2").unwrap();

        let scratch = tempfile::tempdir().unwrap();
        let mut ctx = ctx(scratch.path());
        ctx.secret_store = Arc::new(store);

        // A canary env var set only in the daemon process
        std::env::set_var("OTTER_CANARY", "should_not_leak");

        let step_def = StepDef {
            requires: Some(vec!["MY_SECRET".into()]),
            ..step(vec![
                "bash",
                "-c",
                "echo secret=$MY_SECRET canary=${OTTER_CANARY:-absent}",
            ])
        };

        // WHEN
        let out = ShellExecutor.execute(&step_def, &ctx).await.unwrap();

        // THEN: declared secret is present, canary is absent
        assert!(
            out.stdout.contains("secret=hunter2"),
            "secret not injected: {}",
            out.stdout
        );
        assert!(
            out.stdout.contains("canary=absent"),
            "daemon env leaked: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn no_secrets_field_still_isolates_env() {
        // GIVEN no secrets field (None) and a daemon env var that should not leak
        let scratch = tempfile::tempdir().unwrap();
        std::env::set_var("OTTER_CANARY_NO_SECRETS", "should_not_leak");

        let step_def = step(vec![
            "bash",
            "-c",
            "echo canary=${OTTER_CANARY_NO_SECRETS:-absent}",
        ]);
        // secrets: None (the default from step())

        // WHEN
        let out = ShellExecutor
            .execute(&step_def, &ctx(scratch.path()))
            .await
            .unwrap();

        // THEN: daemon env var is not visible
        assert!(
            out.stdout.contains("canary=absent"),
            "daemon env leaked without secrets field: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn secrets_field_fails_step_when_secret_not_in_store() {
        // GIVEN an empty store and a step that declares a missing secret.
        // Even without a manifest, undeclared names route to the store (legacy path).
        let dir = tempfile::tempdir().unwrap();
        let store = test_store(dir.path());
        let scratch = tempfile::tempdir().unwrap();
        let mut ctx = ctx(scratch.path());
        ctx.secret_store = Arc::new(store);

        let step_def = StepDef {
            requires: Some(vec!["MISSING_KEY".into()]),
            ..step(vec!["echo", "hello"])
        };

        // WHEN
        let err = ShellExecutor.execute(&step_def, &ctx).await.unwrap_err();

        // THEN
        assert!(
            err.to_string().contains("MISSING_KEY"),
            "error should name the missing key: {err}"
        );
    }

    #[tokio::test]
    async fn requires_injects_non_sensitive_value_from_values_toml() {
        use crate::requirements::{RequireEntry, Requirements};

        // GIVEN: non-sensitive entry whose value lives in
        // <scripts_dir>/.otter-state/values.toml. The shell step reads it from env.
        let scripts_dir = tempfile::tempdir().unwrap();
        let state_dir = scripts_dir.path().join(".otter-state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(state_dir.join("values.toml"), r#"REPO_PATH = "/srv/repo""#).unwrap();

        let mut manifest = Requirements::new();
        manifest.insert(
            "REPO_PATH".into(),
            RequireEntry {
                description: "x".into(),
                sensitive: false,
                default: None,
            },
        );

        let scratch = tempfile::tempdir().unwrap();
        let mut ctx = ctx(scratch.path());
        ctx.scripts_dir = Some(scripts_dir.path().to_path_buf());
        ctx.requirements = Some(Arc::new(manifest));

        let step_def = StepDef {
            requires: Some(vec!["REPO_PATH".into()]),
            ..step(vec!["bash", "-c", "echo repo=$REPO_PATH"])
        };

        // WHEN
        let out = ShellExecutor.execute(&step_def, &ctx).await.unwrap();

        // THEN
        assert!(
            out.stdout.contains("repo=/srv/repo"),
            "non-sensitive value not injected: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn requires_missing_non_sensitive_value_fails_with_configure_hint() {
        use crate::requirements::{RequireEntry, Requirements};

        // GIVEN a declared non-sensitive entry but no values.toml
        let scripts_dir = tempfile::tempdir().unwrap();
        let mut manifest = Requirements::new();
        manifest.insert(
            "REPO_PATH".into(),
            RequireEntry {
                description: "x".into(),
                sensitive: false,
                default: None,
            },
        );

        let scratch = tempfile::tempdir().unwrap();
        let mut ctx = ctx(scratch.path());
        ctx.scripts_dir = Some(scripts_dir.path().to_path_buf());
        ctx.requirements = Some(Arc::new(manifest));

        let step_def = StepDef {
            requires: Some(vec!["REPO_PATH".into()]),
            ..step(vec!["echo", "hi"])
        };

        // WHEN
        let err = ShellExecutor.execute(&step_def, &ctx).await.unwrap_err();

        // THEN
        let msg = err.to_string();
        assert!(
            msg.contains("REPO_PATH"),
            "missing name not in error: {msg}"
        );
        assert!(
            msg.contains("otter workflow configure"),
            "missing-value error must hint at `configure`: {msg}"
        );
    }
}
