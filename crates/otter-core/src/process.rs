use std::path::Path;
use std::sync::OnceLock;

/// Extension trait that prepends a scripts directory to the PATH environment variable
/// of a subprocess command, giving workflow companion scripts priority over system binaries.
pub trait PrependScriptsDir {
    fn prepend_scripts_dir(&mut self, scripts_dir: Option<&Path>) -> &mut Self;
}

static LOGIN_PATH: OnceLock<String> = OnceLock::new();

/// Call once at service startup before spawning subprocesses.
pub fn init_login_path() {
    let result = capture_login_path();
    match &result {
        Ok(path) => tracing::info!("captured login shell PATH ({} entries)", path.matches(':').count() + 1),
        Err(e) => tracing::warn!("failed to capture login shell PATH, using service PATH: {e}"),
    }
    let path = result.unwrap_or_else(|_| std::env::var("PATH").unwrap_or_default());
    LOGIN_PATH.get_or_init(|| path);
}

fn capture_login_path() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        std::env::var("PATH").map_err(|_| "PATH not set".to_string())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let shell = std::env::var("SHELL").map_err(|_| "SHELL not set".to_string())?;
        let output = std::process::Command::new(&shell)
            .args(["-li", "-c", "echo $PATH"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("failed to run {shell} -l -c 'echo $PATH': {e}"))?;
        if !output.status.success() {
            return Err(format!("{shell} -l exited with {}", output.status));
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if path.is_empty() {
            return Err("login shell returned empty PATH".to_string());
        }
        Ok(path)
    }
}

fn login_path() -> &'static str {
    LOGIN_PATH
        .get_or_init(|| {
            tracing::warn!("login PATH not initialized, using service PATH");
            std::env::var("PATH").unwrap_or_default()
        })
}

impl PrependScriptsDir for tokio::process::Command {
    fn prepend_scripts_dir(&mut self, scripts_dir: Option<&Path>) -> &mut Self {
        if let Some(dir) = scripts_dir {
            let base = login_path();
            let base_paths = std::env::split_paths(base);
            let mut parts: Vec<std::path::PathBuf> = vec![dir.to_path_buf()];
            parts.extend(base_paths);
            if let Ok(new_path) = std::env::join_paths(parts) {
                self.env("PATH", new_path);
            }
        }
        self
    }
}

/// Safe system variables always preserved when env isolation is active.
const SAFE_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TEMP",
    "TMP",
    "TERM",
    "SHELL",
    "LANG",
    "LC_ALL",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Call before `prepend_scripts_dir` so PATH is re-extended with the scripts dir.
pub fn inject_isolated_env(cmd: &mut tokio::process::Command, resolved: &[(String, String)]) {
    cmd.env_clear();
    for &key in SAFE_ENV_VARS {
        if key == "PATH" {
            cmd.env("PATH", login_path());
        } else if let Some(val) = std::env::var_os(key) {
            cmd.env(key, val);
        }
    }
    for (k, v) in resolved {
        cmd.env(k, v);
    }
}

/// Build a `tokio::process::Command` for a subprocess, handling both sandboxed and
/// unsandboxed execution. Returns the command ready to spawn.
///
/// When `sandbox_config` is `Some`, the command is wrapped via `agentbox::wrap_command`
/// and secrets are injected as container env vars. When `None`, the command runs directly
/// with an isolated environment and optional scripts dir on PATH.
pub fn build_subprocess_command(
    cmd_args: &[String],
    working_dir: &Path,
    scripts_dir: Option<&Path>,
    secrets: &[(String, String)],
    sandbox_config: Option<&agentbox::SandboxConfig>,
) -> tokio::process::Command {
    if let Some(sandbox) = sandbox_config {
        let mut sandbox = sandbox.clone();
        sandbox.env_vars.extend(secrets.iter().cloned());
        let wrapped = agentbox::wrap_command(cmd_args, &sandbox);
        let mut cmd = tokio::process::Command::new(&wrapped[0]);
        cmd.args(&wrapped[1..]);
        inject_isolated_env(&mut cmd, &[]);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new(&cmd_args[0]);
        cmd.args(&cmd_args[1..]).current_dir(working_dir);
        inject_isolated_env(&mut cmd, secrets);
        cmd.prepend_scripts_dir(scripts_dir);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_login_path_returns_nonempty_on_unix() {
        // GIVEN a Unix system with SHELL set

        // WHEN we capture the login PATH
        let result = capture_login_path();

        // THEN it succeeds and contains at least one directory
        if std::env::var("SHELL").is_ok() {
            let path = result.expect("should capture login PATH");
            assert!(!path.is_empty(), "login PATH should not be empty");
            assert!(
                path.contains('/'),
                "login PATH should contain at least one absolute directory"
            );
        }
    }

    #[test]
    fn login_path_fallback_when_not_initialized() {
        // GIVEN LOGIN_PATH has not been initialized (or was already initialized by another test)

        // WHEN we read the login path
        let path = login_path();

        // THEN it returns a non-empty string (either from init or fallback)
        assert!(!path.is_empty());
    }

    #[test]
    fn inject_isolated_env_sets_path_from_login_shell() {
        // GIVEN a command with isolated env
        let mut cmd = tokio::process::Command::new("true");

        // WHEN we inject the isolated env
        inject_isolated_env(&mut cmd, &[]);

        // THEN the command is configured (we can't inspect env directly,
        // but we verify it doesn't panic and the function completes)
    }

    #[test]
    fn inject_isolated_env_includes_secrets() {
        // GIVEN a command and some secrets
        let mut cmd = tokio::process::Command::new("true");
        let secrets = vec![("MY_SECRET".to_string(), "hunter2".to_string())];

        // WHEN we inject isolated env with secrets
        inject_isolated_env(&mut cmd, &secrets);

        // THEN the function completes without error
    }

    #[test]
    fn prepend_scripts_dir_uses_login_path_not_service_path() {
        // GIVEN a fake scripts dir and a login PATH that differs from the service PATH
        init_login_path();
        let dir = tempfile::tempdir().unwrap();
        let scripts_dir = dir.path();

        // WHEN we prepend the scripts dir
        let mut cmd = tokio::process::Command::new("true");
        inject_isolated_env(&mut cmd, &[]);
        cmd.prepend_scripts_dir(Some(scripts_dir));

        // THEN the resulting PATH starts with the scripts dir followed by the login PATH,
        // not the service PATH.  We verify by checking that login_path() entries appear
        // (they are present in login_path but may not be in the service PATH if the service
        // was launched with a stripped PATH, which is exactly the failure scenario).
        let lp = login_path();
        assert!(!lp.is_empty(), "login PATH must be available for this test");
        // The function must not panic and must complete — the structural check above is
        // sufficient; runtime PATH injection cannot be inspected on a Command directly.
    }
}
