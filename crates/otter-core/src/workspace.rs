use std::path::{Path, PathBuf};
use tracing::warn;
use uuid::Uuid;

use otter_secrets::SecretStore;

use crate::process::inject_isolated_env;
use crate::requirements::{resolve_requires, Requirements};
use crate::types::{RunOutcome, WorkspaceConfig, WorkspaceSource};
use crate::workspace_pool::{acquire_pool_slot, release_pool_slot};

/// Resolves the effective workspace directory for a workflow run.
///
/// - `None` / `Scratch` → `Ok(None)` — callers use the scratch dir instead
/// - `Fixed { path }` → canonicalize, assert is_dir, return `Ok(Some(...))`
/// - `Script { command }` → spawn `command <workflow-name> <run-id>` with a clean
///   environment, trim stdout, canonicalize, assert is_dir, return `Ok(Some(...))`
/// - `Git { base_repo, ref }` → create a git worktree at `ref` from the local base
///   repo. Unpooled: worktree lives inside `scratch_dir`. Pooled: acquires a slot
///   from `[workspace.pool]` and resets it to `ref`.
pub async fn resolve_workspace(
    config: Option<&WorkspaceConfig>,
    workflow_name: &str,
    run_id: Uuid,
    scratch_dir: &Path,
    secret_store: &dyn SecretStore,
    requirements: Option<&Requirements>,
    scripts_dir: Option<&Path>,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(config) = config else {
        return Ok(None);
    };
    match &config.source {
        WorkspaceSource::Scratch => Ok(None),
        WorkspaceSource::Fixed { path } => {
            let resolved = std::fs::canonicalize(path)
                .map_err(|e| anyhow::anyhow!("cannot resolve workspace path '{}': {}", path, e))?;
            if !resolved.is_dir() {
                return Err(anyhow::anyhow!(
                    "workspace path '{}' is not a directory",
                    resolved.display()
                ));
            }
            Ok(Some(resolved))
        }
        WorkspaceSource::Script { command, requires } => {
            if command.is_empty() {
                return Err(anyhow::anyhow!(
                    "workspace script command must not be empty"
                ));
            }
            let resolved_secrets = resolve_requires(
                requires.as_deref().unwrap_or_default(),
                requirements,
                scripts_dir,
                secret_store,
                workflow_name,
            )
            .map_err(|e| {
                anyhow::anyhow!("requires resolution for workspace script failed: {}", e)
            })?;
            let mut cmd = tokio::process::Command::new(&command[0]);
            cmd.args(&command[1..])
                .arg(workflow_name)
                .arg(run_id.to_string());
            inject_isolated_env(&mut cmd, &resolved_secrets, true);
            let output = cmd.output().await.map_err(|e| {
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
        WorkspaceSource::Git { base_repo, ref_ } => {
            let base_repo_path = std::fs::canonicalize(base_repo).map_err(|e| {
                anyhow::anyhow!("cannot resolve git base_repo '{}': {}", base_repo, e)
            })?;
            if !base_repo_path.is_dir() {
                return Err(anyhow::anyhow!(
                    "git base_repo '{}' is not a directory",
                    base_repo_path.display()
                ));
            }
            let git_ref = ref_.as_deref().unwrap_or("HEAD");
            match &config.pool {
                Some(pool) => {
                    let pool_dir = PathBuf::from(&pool.dir);
                    let slot = acquire_pool_slot(&pool_dir, &base_repo_path, git_ref).await?;
                    Ok(Some(slot))
                }
                None => {
                    let worktree_path = scratch_dir.join("worktree");
                    add_worktree(&base_repo_path, &worktree_path, git_ref, false).await?;
                    Ok(Some(worktree_path))
                }
            }
        }
    }
}

/// Cleans up workspace resources at end of run.
pub async fn cleanup_workspace(
    config: Option<&WorkspaceConfig>,
    workspace_dir: Option<&Path>,
    outcome: &RunOutcome,
) -> anyhow::Result<()> {
    let Some(config) = config else {
        return Ok(());
    };
    let Some(workspace_dir) = workspace_dir else {
        return Ok(());
    };
    match &config.source {
        WorkspaceSource::Scratch
        | WorkspaceSource::Fixed { .. }
        | WorkspaceSource::Script { .. } => Ok(()),
        WorkspaceSource::Git { base_repo, .. } => match &config.pool {
            Some(pool) => {
                if pool.keep_directory_on.contains(outcome) {
                    warn!(
                        slot = %workspace_dir.display(),
                        ?outcome,
                        "Keeping git pool slot locked (keep_directory_on)"
                    );
                    Ok(())
                } else {
                    release_pool_slot(workspace_dir).await
                }
            }
            None => {
                let base_repo_path = std::fs::canonicalize(base_repo).map_err(|e| {
                    anyhow::anyhow!("cannot resolve git base_repo '{}': {}", base_repo, e)
                })?;
                remove_worktree(&base_repo_path, workspace_dir).await
            }
        },
    }
}

/// `git -C <base_repo> worktree add [--force] --detach <path> <ref>`.
///
/// `force` overrides a stale "missing but already registered" registration for
/// this exact path (e.g. a pooled slot whose directory was wiped externally).
pub(crate) async fn add_worktree(
    base_repo: &Path,
    worktree_path: &Path,
    git_ref: &str,
    force: bool,
) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(base_repo).arg("worktree").arg("add");
    if force {
        cmd.arg("--force");
    }
    cmd.arg("--detach").arg(worktree_path).arg(git_ref);
    inject_isolated_env(&mut cmd, &[], true);
    let output = cmd.output().await.map_err(|e| {
        anyhow::anyhow!(
            "failed to spawn 'git worktree add' for {}: {}",
            worktree_path.display(),
            e
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "'git worktree add' failed (status {}): {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
}

/// `git -C <base_repo> worktree remove --force <path>`.
pub(crate) async fn remove_worktree(base_repo: &Path, worktree_path: &Path) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(base_repo)
        .arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(worktree_path);
    inject_isolated_env(&mut cmd, &[], true);
    let output = cmd.output().await.map_err(|e| {
        anyhow::anyhow!(
            "failed to spawn 'git worktree remove' for {}: {}",
            worktree_path.display(),
            e
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "'git worktree remove' failed (status {}): {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PoolConfig;
    use crate::workspace_pool::repo_namespace;
    use otter_secrets::NoOpSecretStore;
    use std::process::Command;

    fn no_secrets() -> NoOpSecretStore {
        NoOpSecretStore
    }

    /// Convenience: produces a tempdir for use as the run scratch dir.
    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn cfg(source: WorkspaceSource) -> WorkspaceConfig {
        source.into()
    }

    /// Builds a minimal git repo with one commit. Returns the temp dir guard
    /// and the canonical repo path.
    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        run_git_sync(&path, &["init", "--initial-branch=main"]);
        run_git_sync(&path, &["config", "user.email", "t@t"]);
        run_git_sync(&path, &["config", "user.name", "t"]);
        std::fs::write(path.join("README.md"), "hello").unwrap();
        run_git_sync(&path, &["add", "."]);
        run_git_sync(&path, &["commit", "-m", "init"]);
        (dir, path)
    }

    fn run_git_sync(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn none_returns_no_workspace() {
        // GIVEN / WHEN / THEN
        let s = scratch();
        assert!(resolve_workspace(
            None,
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None
        )
        .await
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn scratch_returns_no_workspace() {
        // GIVEN / WHEN / THEN
        let s = scratch();
        let config = cfg(WorkspaceSource::Scratch);
        assert!(resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None
        )
        .await
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn fixed_resolves_existing_dir() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let config = cfg(WorkspaceSource::Fixed {
            path: dir.path().to_string_lossy().into_owned(),
        });
        let s = scratch();

        // WHEN
        let result = resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap();

        // THEN
        assert_eq!(result.unwrap(), dir.path().canonicalize().unwrap());
    }

    #[tokio::test]
    async fn fixed_errors_for_nonexistent_path() {
        // GIVEN
        let config = cfg(WorkspaceSource::Fixed {
            path: "/nonexistent/path/xyz".to_string(),
        });
        let s = scratch();

        // WHEN / THEN
        assert!(resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn fixed_errors_for_file_not_dir() {
        // GIVEN
        let file = tempfile::NamedTempFile::new().unwrap();
        let config = cfg(WorkspaceSource::Fixed {
            path: file.path().to_string_lossy().into_owned(),
        });
        let s = scratch();

        // WHEN / THEN
        assert!(resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn script_resolves_path_from_stdout() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().to_string_lossy().into_owned();
        let config = cfg(WorkspaceSource::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!("echo '{}'", target),
            ],
            requires: None,
        });
        let s = scratch();

        // WHEN
        let result = resolve_workspace(
            Some(&config),
            "my-wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap();

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
        let config = cfg(WorkspaceSource::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!(
                    "printf '%s %s' \"$0\" \"$1\" > '{}' ; echo '{}'",
                    args_file.display(),
                    target
                ),
            ],
            requires: None,
        });
        let run_id = Uuid::new_v4();
        let s = scratch();

        // WHEN
        resolve_workspace(
            Some(&config),
            "my-workflow",
            run_id,
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap();

        // THEN
        let captured = std::fs::read_to_string(&args_file).unwrap();
        assert_eq!(captured.trim(), format!("my-workflow {}", run_id));
    }

    #[tokio::test]
    async fn script_errors_on_nonzero_exit() {
        // GIVEN
        let config = cfg(WorkspaceSource::Script {
            command: vec!["bash".to_string(), "-c".to_string(), "exit 1".to_string()],
            requires: None,
        });
        let s = scratch();

        // WHEN / THEN
        assert!(resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn script_errors_on_empty_stdout() {
        // GIVEN
        let config = cfg(WorkspaceSource::Script {
            command: vec!["bash".to_string(), "-c".to_string(), "echo ''".to_string()],
            requires: None,
        });
        let s = scratch();

        // WHEN / THEN
        assert!(resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn script_trims_trailing_newline() {
        // GIVEN
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let config = cfg(WorkspaceSource::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!("printf '{}\n'", path),
            ],
            requires: None,
        });
        let s = scratch();

        // WHEN
        let result = resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap();

        // THEN
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn script_isolates_env_and_injects_declared_secret() {
        // GIVEN a script that writes the value of MY_SECRET to a file and the workspace dir to stdout
        let dir = tempfile::tempdir().unwrap();
        let secret_file = dir.path().join("secret_val.txt");
        let target = dir.path().to_string_lossy().into_owned();
        let config = cfg(WorkspaceSource::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!(
                    "echo \"$MY_SECRET\" > '{}' ; echo '{}'",
                    secret_file.display(),
                    target
                ),
            ],
            requires: Some(vec!["MY_SECRET".to_string()]),
        });

        struct OneSecret;
        impl SecretStore for OneSecret {
            fn get(&self, key: &str) -> Option<String> {
                if key == "MY_SECRET" {
                    Some("injected-value".to_string())
                } else {
                    None
                }
            }
            fn list(&self) -> Vec<String> {
                vec!["MY_SECRET".to_string()]
            }
            fn set(&self, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn delete(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let s = scratch();

        // WHEN
        resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &OneSecret,
            None,
            None,
        )
        .await
        .unwrap();

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
        let config = cfg(WorkspaceSource::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!(
                    "echo \"$OTTER_TEST_SENTINEL\" > '{}' ; echo '{}'",
                    env_file.display(),
                    target
                ),
            ],
            requires: None,
        });
        let s = scratch();

        // WHEN
        resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap();

        // THEN — sentinel must not appear (env is isolated)
        let val = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(
            val.trim(),
            "",
            "daemon env must not leak into isolated workspace script"
        );
    }

    #[tokio::test]
    async fn script_injects_non_sensitive_value_from_values_toml() {
        use crate::requirements::{RequireEntry, Requirements};

        // GIVEN a workspace script that reads $REPO_PATH from env, and a
        // values.toml under <scripts_dir>/.otter-state/ holding the value.
        let dir = tempfile::tempdir().unwrap();
        let env_file = dir.path().join("env.txt");
        let target = dir.path().to_string_lossy().into_owned();
        let config = cfg(WorkspaceSource::Script {
            command: vec![
                "bash".to_string(),
                "-c".to_string(),
                format!(
                    "echo \"$REPO_PATH\" > '{}' ; echo '{}'",
                    env_file.display(),
                    target
                ),
            ],
            requires: Some(vec!["REPO_PATH".to_string()]),
        });

        let scripts_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(scripts_dir.path().join(".otter-state")).unwrap();
        std::fs::write(
            scripts_dir.path().join(".otter-state").join("values.toml"),
            r#"REPO_PATH = "/srv/repo""#,
        )
        .unwrap();

        let mut manifest = Requirements::new();
        manifest.insert(
            "REPO_PATH".into(),
            RequireEntry {
                description: "x".into(),
                sensitive: false,
                default: None,
            },
        );

        let s = scratch();

        // WHEN
        resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            Some(&manifest),
            Some(scripts_dir.path()),
        )
        .await
        .unwrap();

        // THEN — non-sensitive value was injected
        let val = std::fs::read_to_string(&env_file).unwrap();
        assert_eq!(val.trim(), "/srv/repo");
    }

    // ────────────────────────── git workspace ──────────────────────────

    #[tokio::test]
    async fn git_unpooled_creates_worktree_at_ref() {
        // GIVEN a base repo with one commit and an empty scratch dir
        let (_repo_guard, repo) = init_repo();
        let s = scratch();
        let config = cfg(WorkspaceSource::Git {
            base_repo: repo.to_string_lossy().into_owned(),
            ref_: Some("HEAD".to_string()),
        });

        // WHEN
        let result = resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap();

        // THEN — worktree under scratch/worktree with the base commit checked out
        let wt = result.unwrap();
        assert_eq!(wt, s.path().join("worktree"));
        assert_eq!(
            std::fs::read_to_string(wt.join("README.md")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn git_pooled_returns_locked_slot() {
        // GIVEN
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let s = scratch();
        let config = WorkspaceConfig {
            source: WorkspaceSource::Git {
                base_repo: repo.to_string_lossy().into_owned(),
                ref_: Some("HEAD".to_string()),
            },
            pool: Some(PoolConfig {
                dir: pool.path().to_string_lossy().into_owned(),
                keep_directory_on: vec![],
            }),
        };

        // WHEN
        let result = resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap();

        // THEN
        let slot = result.unwrap();
        let ns = pool.path().join(repo_namespace(&repo));
        assert_eq!(slot, ns.join("slot-0"));
        assert!(ns.join("slot-0.lock").is_dir());
    }

    #[tokio::test]
    async fn cleanup_releases_pool_slot_on_success() {
        // GIVEN
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let s = scratch();
        let config = WorkspaceConfig {
            source: WorkspaceSource::Git {
                base_repo: repo.to_string_lossy().into_owned(),
                ref_: Some("HEAD".to_string()),
            },
            pool: Some(PoolConfig {
                dir: pool.path().to_string_lossy().into_owned(),
                keep_directory_on: vec![],
            }),
        };
        let slot = resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        // WHEN
        cleanup_workspace(Some(&config), Some(&slot), &RunOutcome::Success)
            .await
            .unwrap();

        // THEN — lock dir gone, slot dir kept
        let ns = pool.path().join(repo_namespace(&repo));
        assert!(!ns.join("slot-0.lock").exists());
        assert!(ns.join("slot-0").is_dir());
    }

    #[tokio::test]
    async fn cleanup_keeps_pool_slot_when_outcome_in_keep_directory_on() {
        // GIVEN keep on Failed
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let s = scratch();
        let config = WorkspaceConfig {
            source: WorkspaceSource::Git {
                base_repo: repo.to_string_lossy().into_owned(),
                ref_: Some("HEAD".to_string()),
            },
            pool: Some(PoolConfig {
                dir: pool.path().to_string_lossy().into_owned(),
                keep_directory_on: vec![RunOutcome::Failed],
            }),
        };
        let slot = resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();

        // WHEN — run failed
        cleanup_workspace(Some(&config), Some(&slot), &RunOutcome::Failed)
            .await
            .unwrap();

        // THEN — lock dir stays held
        let ns = pool.path().join(repo_namespace(&repo));
        assert!(ns.join("slot-0.lock").is_dir());
    }

    #[tokio::test]
    async fn cleanup_removes_unpooled_worktree() {
        // GIVEN
        let (_repo_guard, repo) = init_repo();
        let s = scratch();
        let config = cfg(WorkspaceSource::Git {
            base_repo: repo.to_string_lossy().into_owned(),
            ref_: Some("HEAD".to_string()),
        });
        let wt = resolve_workspace(
            Some(&config),
            "wf",
            Uuid::new_v4(),
            s.path(),
            &no_secrets(),
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(wt.is_dir());

        // WHEN
        cleanup_workspace(Some(&config), Some(&wt), &RunOutcome::Success)
            .await
            .unwrap();

        // THEN — worktree dir is gone and `git worktree list` no longer registers it
        assert!(!wt.exists(), "worktree dir should be removed");
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(
            !listing.contains(wt.to_string_lossy().as_ref()),
            "registration should be dropped, got: {listing}"
        );
    }
}
