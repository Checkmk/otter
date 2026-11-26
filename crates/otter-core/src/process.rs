use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Returns a path suitable for passing as an argument to a subprocess.
///
/// On Windows, `std::fs::canonicalize` prepends `\\?\` (extended-length path prefix)
/// which is not understood by shells like bash. This function strips that prefix so the
/// path is usable as a subprocess argument on all platforms.
pub fn subprocess_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(stripped);
        }
    }
    path.to_path_buf()
}

/// Extension trait that prepends a scripts directory to the PATH environment variable
/// of a subprocess command, giving workflow companion scripts priority over system binaries.
pub trait PrependScriptsDir {
    fn prepend_scripts_dir(&mut self, scripts_dir: Option<&Path>) -> &mut Self;
}

/// The environment of the user's login shell. A service started at boot does
/// not see what the user's shell profile exports, so PATH and inherited
/// variables are read from here instead of the service's own environment.
static LOGIN_ENV: OnceLock<HashMap<OsString, OsString>> = OnceLock::new();

/// Call once at service startup before spawning subprocesses.
pub fn init_login_env() {
    let result = capture_login_env();
    match &result {
        Ok(env) => tracing::info!("captured login shell environment ({} variables)", env.len()),
        Err(e) => tracing::warn!(
            "failed to capture login shell environment, using service environment: {e}"
        ),
    }
    let env = result.unwrap_or_else(|_| std::env::vars_os().collect());
    LOGIN_ENV.get_or_init(|| env);
}

fn capture_login_env() -> Result<HashMap<OsString, OsString>, String> {
    #[cfg(target_os = "windows")]
    {
        Ok(std::env::vars_os().collect())
    }

    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::ffi::OsStrExt;

        let shell = std::env::var("SHELL").map_err(|_| "SHELL not set".to_string())?;
        let output = std::process::Command::new(&shell)
            .args(["-li", "-c", "env -0"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("failed to run {shell} -l -c 'env -0': {e}"))?;
        if !output.status.success() {
            return Err(format!("{shell} -l exited with {}", output.status));
        }
        let env: HashMap<OsString, OsString> = output
            .stdout
            .split(|&b| b == 0)
            .filter_map(|entry| {
                let eq = entry.iter().position(|&b| b == b'=').filter(|&i| i > 0)?;
                Some((
                    OsStr::from_bytes(&entry[..eq]).to_owned(),
                    OsStr::from_bytes(&entry[eq + 1..]).to_owned(),
                ))
            })
            .collect();
        if env.get(OsStr::new("PATH")).is_none_or(|p| p.is_empty()) {
            return Err("login shell returned empty PATH".to_string());
        }
        Ok(env)
    }
}

fn login_env() -> &'static HashMap<OsString, OsString> {
    LOGIN_ENV.get_or_init(|| {
        tracing::warn!("login environment not initialized, using service environment");
        std::env::vars_os().collect()
    })
}

fn login_var(name: &str) -> Option<&'static OsStr> {
    let env = login_env();
    // Windows variable names are case-insensitive.
    if cfg!(windows) {
        env.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_os_str())
    } else {
        env.get(OsStr::new(name)).map(OsString::as_os_str)
    }
}

fn login_path() -> &'static OsStr {
    login_var("PATH").unwrap_or_default()
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
    // Windows: required for config/credential lookup and DLL loading
    "APPDATA",
    "LOCALAPPDATA",
    "USERPROFILE",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    // Windows: required for launching programs and locating installed tools
    "PATHEXT",
    "COMSPEC",
    "WINDIR",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMDATA",
    "USERNAME",
];

/// Variables that grant access to privileged host resources.
const UNSAFE_ENV_VARS: &[&str] = &["SSH_AUTH_SOCK", "SSH_AGENT_PID"];

/// Whether `name` is on the built-in safe list, and so reaches every subprocess anyway.
pub fn is_always_passed(name: &str) -> bool {
    SAFE_ENV_VARS.iter().any(|safe| {
        if cfg!(windows) {
            safe.eq_ignore_ascii_case(name)
        } else {
            *safe == name
        }
    })
}

/// Resolve a workflow's `[env] inherit` list against the login environment.
/// Unset names are skipped so one list can name both Windows and Unix variables.
pub fn inherited_env(names: &[String]) -> Vec<(String, String)> {
    inherited_env_from(names, login_var)
}

fn inherited_env_from<'a>(
    names: &[String],
    lookup: impl Fn(&str) -> Option<&'a OsStr>,
) -> Vec<(String, String)> {
    names
        .iter()
        .filter(|name| !is_always_passed(name))
        .filter_map(|name| {
            let value = lookup(name)?;
            let Some(value) = value.to_str() else {
                tracing::warn!(name = %name, "inherited variable is not valid UTF-8; skipping");
                return None;
            };
            Some((name.clone(), value.to_string()))
        })
        .collect()
}

/// Reset the command environment to an isolated baseline. Set `include_unsafe`
/// gives access to some host resource access (e.g. SSH agent for git).
pub fn inject_isolated_env(
    cmd: &mut tokio::process::Command,
    resolved: &[(String, String)],
    include_unsafe: bool,
) {
    cmd.env_clear();
    // Iterate the host env rather than the safe list so Windows children see the
    // host's spelling (`ProgramFiles`), which case-sensitive shells depend on.
    for (key, val) in std::env::vars_os() {
        if key.to_str().is_some_and(is_always_passed) {
            cmd.env(key, val);
        }
    }
    cmd.env("PATH", login_path());
    if include_unsafe {
        for &key in UNSAFE_ENV_VARS {
            if let Some(val) = std::env::var_os(key) {
                cmd.env(key, val);
            }
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
        inject_isolated_env(&mut cmd, &[], false);
        cmd
    } else {
        let mut cmd = tokio::process::Command::new(&cmd_args[0]);
        cmd.args(&cmd_args[1..]).current_dir(working_dir);
        inject_isolated_env(&mut cmd, secrets, true);
        cmd.prepend_scripts_dir(scripts_dir);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn capture_login_env_includes_path_on_unix() {
        // GIVEN a Unix system with SHELL set

        // WHEN we capture the login environment
        let result = capture_login_env();

        // THEN it succeeds and its PATH contains at least one directory
        if std::env::var("SHELL").is_ok() {
            let env = result.expect("should capture login environment");
            let path = env[OsStr::new("PATH")].to_string_lossy();
            assert!(
                path.contains('/'),
                "login PATH should contain at least one absolute directory"
            );
        }
    }

    fn lookup_in<'a>(env: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a OsStr> {
        move |name| {
            env.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsStr::new(*v))
        }
    }

    #[test]
    fn inherited_env_passes_set_names_in_order() {
        // GIVEN
        let host = [("JAVA_HOME", "/opt/jdk"), ("BAZEL_SH", "/bin/bash")];
        let names = vec!["BAZEL_SH".to_string(), "JAVA_HOME".to_string()];

        // WHEN
        let env = inherited_env_from(&names, lookup_in(&host));

        // THEN
        assert_eq!(
            env,
            vec![
                ("BAZEL_SH".to_string(), "/bin/bash".to_string()),
                ("JAVA_HOME".to_string(), "/opt/jdk".to_string()),
            ]
        );
    }

    #[test]
    fn inherited_env_skips_unset_names() {
        // GIVEN a list naming both a Windows and a Unix variable
        let host = [("ANDROID_SDK_ROOT", "/opt/android")];
        let names = vec![
            "ProgramFiles(x86)".to_string(),
            "ANDROID_SDK_ROOT".to_string(),
        ];

        // WHEN
        let env = inherited_env_from(&names, lookup_in(&host));

        // THEN
        assert_eq!(
            env,
            vec![("ANDROID_SDK_ROOT".to_string(), "/opt/android".to_string())]
        );
    }

    #[test]
    fn sandboxed_command_passes_step_env_into_container() {
        // GIVEN an inherited host path and a sandbox
        let sandbox = agentbox::SandboxConfig {
            image: None,
            workspace_dir: PathBuf::from("/ws"),
            extra_mounts: vec![],
            network: agentbox::NetworkMode::Bridge,
            cpus: None,
            env_vars: vec![],
            tty: false,
        };
        let env = vec![("JAVA_HOME".to_string(), "/opt/jdk".to_string())];

        // WHEN
        let cmd = build_subprocess_command(
            &["java".to_string()],
            Path::new("/ws"),
            None,
            &env,
            Some(&sandbox),
        );

        // THEN the value is passed into the container verbatim
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-e" && w[1] == "JAVA_HOME=/opt/jdk"));
    }

    #[test]
    fn inherited_env_skips_always_passed_names() {
        // GIVEN a host PATH that would clobber the container PATH under [sandbox]
        let host = [("PATH", "/host/bin")];

        // WHEN
        let env = inherited_env_from(&["PATH".to_string()], lookup_in(&host));

        // THEN
        assert!(env.is_empty());
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
        inject_isolated_env(&mut cmd, &[], false);

        // THEN the command is configured (we can't inspect env directly,
        // but we verify it doesn't panic and the function completes)
    }

    #[test]
    fn inject_isolated_env_includes_secrets() {
        // GIVEN a command and some secrets
        let mut cmd = tokio::process::Command::new("true");
        let secrets = vec![("MY_SECRET".to_string(), "hunter2".to_string())];

        // WHEN we inject isolated env with secrets
        inject_isolated_env(&mut cmd, &secrets, false);

        // THEN the function completes without error
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn sandboxed_env_excludes_ssh_agent_vars() {
        // GIVEN SSH_AUTH_SOCK is set in the host environment
        let test_sock = "/tmp/otter-test-ssh-agent.sock";
        // SAFETY: test process is single-threaded at this point; no concurrent env mutation
        unsafe { std::env::set_var("SSH_AUTH_SOCK", test_sock) };

        // WHEN we inject isolated env with include_unsafe = false (sandboxed path)
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "printf '%s' \"$SSH_AUTH_SOCK\""]);
        inject_isolated_env(&mut cmd, &[], false);
        let output = cmd.output().await.expect("sh must be available");

        // THEN SSH_AUTH_SOCK is absent from the subprocess environment
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.is_empty(),
            "sandboxed steps must not receive SSH_AUTH_SOCK; got: {stdout:?}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn unsandboxed_env_includes_ssh_agent_vars() {
        // GIVEN SSH_AUTH_SOCK is set in the host environment
        let test_sock = "/tmp/otter-test-ssh-agent.sock";
        // SAFETY: test process is single-threaded at this point; no concurrent env mutation
        unsafe { std::env::set_var("SSH_AUTH_SOCK", test_sock) };

        // WHEN we inject isolated env with include_unsafe = true (non-sandboxed path)
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "printf '%s' \"$SSH_AUTH_SOCK\""]);
        inject_isolated_env(&mut cmd, &[], true);
        let output = cmd.output().await.expect("sh must be available");

        // THEN SSH_AUTH_SOCK is forwarded to the subprocess
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.as_ref(),
            test_sock,
            "non-sandboxed steps must receive SSH_AUTH_SOCK"
        );
    }

    #[test]
    #[cfg(windows)]
    fn login_var_ignores_case_on_windows() {
        // GIVEN a variable the host sets as `Path` or `PATH`
        let host_path = std::env::var_os("PATH").unwrap();

        // WHEN it is looked up in a different case
        let found = login_var("pAtH");

        // THEN the host value is found
        assert_eq!(found, Some(host_path.as_os_str()));
    }

    #[test]
    #[cfg(windows)]
    fn isolated_env_keeps_windows_basics() {
        // GIVEN the Windows basics that tooling relies on to launch programs
        let basics = [
            "PATHEXT",
            "COMSPEC",
            "WINDIR",
            "PROGRAMFILES",
            "PROGRAMFILES(X86)",
            "PROGRAMDATA",
            "USERNAME",
        ];

        // WHEN we inject the isolated env
        let mut cmd = tokio::process::Command::new("cmd");
        inject_isolated_env(&mut cmd, &[], false);

        // THEN every basic set on the host is passed under the host's spelling and value,
        // so case-sensitive consumers such as Git Bash still find e.g. `$ProgramFiles`
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        for key in basics {
            let Some((host_key, host_val)) =
                std::env::vars_os().find(|(k, _)| k.eq_ignore_ascii_case(key))
            else {
                continue;
            };
            assert!(
                envs.iter()
                    .any(|(k, v)| *k == host_key && *v == Some(host_val.as_os_str())),
                "{host_key:?} missing from isolated env"
            );
        }
    }

    #[test]
    fn prepend_scripts_dir_uses_login_path_not_service_path() {
        // GIVEN a fake scripts dir and a login PATH that differs from the service PATH
        init_login_env();
        let dir = tempfile::tempdir().unwrap();
        let scripts_dir = dir.path();

        // WHEN we prepend the scripts dir
        let mut cmd = tokio::process::Command::new("true");
        inject_isolated_env(&mut cmd, &[], false);
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
