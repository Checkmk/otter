use std::path::PathBuf;
use uuid::Uuid;

use crate::types::WorkspaceConfig;

/// Resolves the effective workspace directory for a workflow run.
///
/// - `None` / `Scratch` → `Ok(None)` — callers use the scratch dir instead
/// - `Fixed { path }` → canonicalize, assert is_dir, return `Ok(Some(...))`
/// - `Script { command }` → spawn `command <workflow-name> <run-id>`,
///   trim stdout, canonicalize, assert is_dir, return `Ok(Some(...))`
pub fn resolve_workspace(
    config: Option<&WorkspaceConfig>,
    workflow_name: &str,
    run_id: Uuid,
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
        Some(WorkspaceConfig::Script { command }) => {
            if command.is_empty() {
                return Err(anyhow::anyhow!("workspace script command must not be empty"));
            }
            let output = std::process::Command::new(&command[0])
                .args(&command[1..])
                .arg(workflow_name)
                .arg(run_id.to_string())
                .output()
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

    #[test]
    fn none_returns_no_workspace() {
        // GIVEN / WHEN / THEN
        assert!(resolve_workspace(None, "wf", Uuid::new_v4()).unwrap().is_none());
    }

    #[test]
    fn scratch_returns_no_workspace() {
        // GIVEN / WHEN / THEN
        assert!(
            resolve_workspace(Some(&WorkspaceConfig::Scratch), "wf", Uuid::new_v4())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn fixed_resolves_existing_dir() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let config = WorkspaceConfig::Fixed {
            path: dir.path().to_string_lossy().into_owned(),
        };

        // WHEN
        let result = resolve_workspace(Some(&config), "wf", Uuid::new_v4()).unwrap();

        // THEN
        assert_eq!(result.unwrap(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn fixed_errors_for_nonexistent_path() {
        // GIVEN
        let config = WorkspaceConfig::Fixed {
            path: "/nonexistent/path/xyz".to_string(),
        };

        // WHEN / THEN
        assert!(resolve_workspace(Some(&config), "wf", Uuid::new_v4()).is_err());
    }

    #[test]
    fn fixed_errors_for_file_not_dir() {
        // GIVEN
        let file = tempfile::NamedTempFile::new().unwrap();
        let config = WorkspaceConfig::Fixed {
            path: file.path().to_string_lossy().into_owned(),
        };

        // WHEN / THEN
        assert!(resolve_workspace(Some(&config), "wf", Uuid::new_v4()).is_err());
    }

    #[test]
    fn script_resolves_path_from_stdout() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().to_string_lossy().into_owned();
        let config = WorkspaceConfig::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!("echo '{}'", target),
            ],
        };

        // WHEN
        let result = resolve_workspace(Some(&config), "my-wf", Uuid::new_v4()).unwrap();

        // THEN
        assert_eq!(result.unwrap(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn script_passes_workflow_name_and_run_id_as_args() {
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
        };
        let run_id = Uuid::new_v4();

        // WHEN
        resolve_workspace(Some(&config), "my-workflow", run_id).unwrap();

        // THEN
        let captured = std::fs::read_to_string(&args_file).unwrap();
        assert_eq!(captured.trim(), format!("my-workflow {}", run_id));
    }

    #[test]
    fn script_errors_on_nonzero_exit() {
        // GIVEN
        let config = WorkspaceConfig::Script {
            command: vec!["bash".to_string(), "-c".to_string(), "exit 1".to_string()],
        };

        // WHEN / THEN
        assert!(resolve_workspace(Some(&config), "wf", Uuid::new_v4()).is_err());
    }

    #[test]
    fn script_errors_on_empty_stdout() {
        // GIVEN
        let config = WorkspaceConfig::Script {
            command: vec!["bash".to_string(), "-c".to_string(), "echo ''".to_string()],
        };

        // WHEN / THEN
        assert!(resolve_workspace(Some(&config), "wf", Uuid::new_v4()).is_err());
    }

    #[test]
    fn script_trims_trailing_newline() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let config = WorkspaceConfig::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!("printf '{}\n'", path),
            ],
        };

        // WHEN
        let result = resolve_workspace(Some(&config), "wf", Uuid::new_v4()).unwrap();

        // THEN
        assert!(result.is_some());
    }
}
