use std::path::PathBuf;
use uuid::Uuid;

use otter_secrets::SecretStore;

use crate::process::inject_isolated_env;
use crate::types::WorkspaceConfig;

/// Resolves the effective workspace directory for a workflow run.
///
/// - `None` / `Scratch` → `Ok(None)` — callers use the scratch dir instead
/// - `Fixed { path }` → canonicalize, assert is_dir, return `Ok(Some(...))`
/// - `Script { command }` → spawn `command <workflow-name> <run-id>` with a clean
///   environment, trim stdout, canonicalize, assert is_dir, return `Ok(Some(...))`
pub async fn resolve_workspace(
    config: Option<&WorkspaceConfig>,
    workflow_name: &str,
    run_id: Uuid,
    secret_store: &dyn SecretStore,
) -> anyhow::Result<Option<PathBuf>> {
    match config {
        None | Some(WorkspaceConfig::Scratch) => Ok(None),
        Some(WorkspaceConfig::Fixed { path }) => {
            let resolved = std::fs::canonicalize(path).map_err(|e| {
                anyhow::anyhow!("cannot resolve workspace path '{}': {}", path, e)
            })?;
            if !resolved.is_dir() {
                return Err(anyhow::anyhow!(
                    "workspace path '{}' is not a directory",
                    resolved.display()
                ));
            }
            Ok(Some(resolved))
        }
        Some(WorkspaceConfig::Script { command, secrets }) => {
            if command.is_empty() {
                return Err(anyhow::anyhow!("workspace script command must not be empty"));
            }
            let resolved_secrets = secret_store
                .resolve(secrets.as_deref().unwrap_or_default())
                .map_err(|e| anyhow::anyhow!("secret resolution for workspace script failed: {}", e))?;
            let mut cmd = tokio::process::Command::new(&command[0]);
            cmd.args(&command[1..])
                .arg(workflow_name)
                .arg(run_id.to_string());
            inject_isolated_env(&mut cmd, &resolved_secrets);
            let output = cmd
                .output()
                .await
                .map_err(|e| {
                    anyhow::anyhow!("failed to run workspace script '{}': {}", command[0], e)
                })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow::anyhow!(
                    "workspace script '{}' exited with {}: {}",
                    command[0],
                    output.status,
                    stderr.trim()
                ));
            }
            let raw = String::from_utf8_lossy(&output.stdout);
            let path = raw.trim();
            if path.is_empty() {
                return Err(anyhow::anyhow!(
                    "workspace script '{}' produced no output",
                    command[0]
                ));
            }
            let resolved = std::fs::canonicalize(path).map_err(|e| {
                anyhow::anyhow!("workspace script returned invalid path '{}': {}", path, e)
            })?;
            if !resolved.is_dir() {
                return Err(anyhow::anyhow!(
                    "workspace script returned '{}' which is not a directory",
                    resolved.display()
                ));
            }
            Ok(Some(resolved))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_secrets::NoOpSecretStore;

    fn no_secrets() -> NoOpSecretStore {
        NoOpSecretStore
    }

    #[tokio::test]
    async fn none_returns_no_workspace() {
        // GIVEN / WHEN / THEN
        assert!(resolve_workspace(None, "wf", Uuid::new_v4(), &no_secrets()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn scratch_returns_no_workspace() {
        // GIVEN / WHEN / THEN
        assert!(
            resolve_workspace(Some(&WorkspaceConfig::Scratch), "wf", Uuid::new_v4(), &no_secrets())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn fixed_resolves_existing_dir() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let config = WorkspaceConfig::Fixed {
            path: dir.path().to_string_lossy().into_owned(),
        };

        // WHEN
        let result = resolve_workspace(Some(&config), "wf", Uuid::new_v4(), &no_secrets()).await.unwrap();

        // THEN
        assert_eq!(result.unwrap(), dir.path().canonicalize().unwrap());
    }

    #[tokio::test]
    async fn fixed_errors_for_nonexistent_path() {
        // GIVEN
        let config = WorkspaceConfig::Fixed {
            path: "/nonexistent/path/xyz".to_string(),
        };

        // WHEN / THEN
        assert!(resolve_workspace(Some(&config), "wf", Uuid::new_v4(), &no_secrets()).await.is_err());
    }

    #[tokio::test]
    async fn fixed_errors_for_file_not_dir() {
        // GIVEN
        let file = tempfile::NamedTempFile::new().unwrap();
        let config = WorkspaceConfig::Fixed {
            path: file.path().to_string_lossy().into_owned(),
        };

        // WHEN / THEN
        assert!(resolve_workspace(Some(&config), "wf", Uuid::new_v4(), &no_secrets()).await.is_err());
    }

    #[tokio::test]
    async fn script_resolves_path_from_stdout() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().to_string_lossy().into_owned();
        let config = WorkspaceConfig::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!("echo '{}'", target),
            ],
            secrets: None,
        };

        // WHEN
        let result = resolve_workspace(Some(&config), "my-wf", Uuid::new_v4(), &no_secrets()).await.unwrap();

        // THEN
        assert_eq!(result.unwrap(), dir.path().canonicalize().unwrap());
    }

    #[tokio::test]
    async fn script_passes_workflow_name_and_run_id_as_args() {
        // GIVEN: a bash -c script that writes its positional args ($0, $1) to a file
        // resolve_workspace appends workflow_name and run_id as positional args;
        // with `bash -c 'script' arg0 arg1`, $0=arg0 and $1=arg1.
        let dir = tempfile::tempdir().unwrap();
        let args_file = dir.path().join("args.txt");
        let target = dir.path().to_string_lossy().into_owned();
        let config = WorkspaceConfig::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!(
                    "printf '%s %s' \"$0\" \"$1\" > '{}' ; echo '{}'",
                    args_file.display(),
                    target
                ),
            ],
            secrets: None,
        };
        let run_id = Uuid::new_v4();

        // WHEN
        resolve_workspace(Some(&config), "my-workflow", run_id, &no_secrets()).await.unwrap();

        // THEN
        let captured = std::fs::read_to_string(&args_file).unwrap();
        assert_eq!(captured.trim(), format!("my-workflow {}", run_id));
    }

    #[tokio::test]
    async fn script_errors_on_nonzero_exit() {
        // GIVEN
        let config = WorkspaceConfig::Script {
            command: vec!["bash".to_string(), "-c".to_string(), "exit 1".to_string()],
            secrets: None,
        };

        // WHEN / THEN
        assert!(resolve_workspace(Some(&config), "wf", Uuid::new_v4(), &no_secrets()).await.is_err());
    }

    #[tokio::test]
    async fn script_errors_on_empty_stdout() {
        // GIVEN
        let config = WorkspaceConfig::Script {
            command: vec!["bash".to_string(), "-c".to_string(), "echo ''".to_string()],
            secrets: None,
        };

        // WHEN / THEN
        assert!(resolve_workspace(Some(&config), "wf", Uuid::new_v4(), &no_secrets()).await.is_err());
    }

    #[tokio::test]
    async fn script_trims_trailing_newline() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let config = WorkspaceConfig::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!("printf '{}\n'", path),
            ],
            secrets: None,
        };

        // WHEN
        let result = resolve_workspace(Some(&config), "wf", Uuid::new_v4(), &no_secrets()).await.unwrap();

        // THEN
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn script_isolates_env_and_injects_declared_secret() {
        // GIVEN a script that writes the value of MY_SECRET to a file and the workspace dir to stdout
        let dir = tempfile::tempdir().unwrap();
        let secret_file = dir.path().join("secret_val.txt");
        let target = dir.path().to_string_lossy().into_owned();
        let config = WorkspaceConfig::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!(
                    "echo \"$MY_SECRET\" > '{}' ; echo '{}'",
                    secret_file.display(),
                    target
                ),
            ],
            secrets: Some(vec!["MY_SECRET".to_string()]),
        };

        struct OneSecret;
        impl SecretStore for OneSecret {
            fn get(&self, key: &str) -> Option<String> {
                if key == "MY_SECRET" { Some("injected-value".to_string()) } else { None }
            }
            fn list(&self) -> Vec<String> { vec!["MY_SECRET".to_string()] }
            fn set(&self, _: &str, _: &str) -> anyhow::Result<()> { Ok(()) }
            fn delete(&self, _: &str) -> anyhow::Result<()> { Ok(()) }
        }

        // WHEN
        resolve_workspace(Some(&config), "wf", Uuid::new_v4(), &OneSecret).await.unwrap();

        // THEN — secret was injected
        let val = std::fs::read_to_string(&secret_file).unwrap();
        assert_eq!(val.trim(), "injected-value");
    }

    #[tokio::test]
    async fn script_env_is_isolated_without_secrets() {
        // GIVEN a script that writes a daemon-env var (if set) to a file; under isolation it must be empty
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("env_val.txt");
        let target = dir.path().to_string_lossy().into_owned();
        // OTTER_TEST_SENTINEL is set in the parent process for this test
        std::env::set_var("OTTER_TEST_SENTINEL", "should-not-leak");
        let config = WorkspaceConfig::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!(
                    "echo \"$OTTER_TEST_SENTINEL\" > '{}' ; echo '{}'",
                    env_file.display(),
                    target
                ),
            ],
            secrets: None,
        };

        // WHEN
        resolve_workspace(Some(&config), "wf", Uuid::new_v4(), &no_secrets()).await.unwrap();

        // THEN — sentinel must not appear (env is isolated)
        let val = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(val.trim(), "", "daemon env must not leak into isolated workspace script");
    }
}
