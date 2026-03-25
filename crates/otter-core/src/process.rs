use std::path::Path;

/// Extension trait that prepends a scripts directory to the PATH environment variable
/// of a subprocess command, giving workflow companion scripts priority over system binaries.
pub trait PrependScriptsDir {
    fn prepend_scripts_dir(&mut self, scripts_dir: Option<&Path>) -> &mut Self;
}

impl PrependScriptsDir for tokio::process::Command {
    fn prepend_scripts_dir(&mut self, scripts_dir: Option<&Path>) -> &mut Self {
        if let Some(dir) = scripts_dir {
            let old_path = std::env::var_os("PATH").unwrap_or_default();
            let mut parts: Vec<std::path::PathBuf> = vec![dir.to_path_buf()];
            parts.extend(std::env::split_paths(&old_path));
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

/// Clear the subprocess environment and inject only the given resolved secrets
/// plus a minimal set of safe system variables.
///
/// Call this before `prepend_scripts_dir` so PATH is re-extended with the scripts dir.
pub fn inject_isolated_env(cmd: &mut tokio::process::Command, resolved: &[(String, String)]) {
    cmd.env_clear();
    for &key in SAFE_ENV_VARS {
        if let Some(val) = std::env::var_os(key) {
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
        cmd.env_clear();
        cmd
    } else {
        let mut cmd = tokio::process::Command::new(&cmd_args[0]);
        cmd.args(&cmd_args[1..]).current_dir(working_dir);
        inject_isolated_env(&mut cmd, secrets);
        cmd.prepend_scripts_dir(scripts_dir);
        cmd
    }
}
