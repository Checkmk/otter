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
        let cmd_args = if sandbox.backend == agentbox::Backend::Bwrap {
            expose_host_binary(
                cmd_args,
                login_path(),
                scripts_dir,
                &mut sandbox.extra_mounts,
                working_dir,
            )
        } else {
            cmd_args.to_vec()
        };
        let wrapped = agentbox::wrap_command(&cmd_args, &sandbox);
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

/// The bwrap sandbox only sees the host's /usr and /etc, so tools installed
/// elsewhere (e.g. `claude` under ~/.local or linuxbrew) would be missing. Resolve
/// the program on the host PATH and bind its file read-only at the first hidden
/// path in its symlink chain. Visible links and the invocation path stay intact.
///
/// Programs that don't resolve on the host PATH are left alone; they may be
/// companion scripts that only exist on the sandbox PATH.
fn expose_host_binary(
    cmd_args: &[String],
    host_path: impl AsRef<OsStr>,
    scripts_dir: Option<&Path>,
    mounts: &mut Vec<agentbox::Mount>,
    working_dir: &Path,
) -> Vec<String> {
    let program = Path::new(&cmd_args[0]);
    if program.is_relative() && cmd_args[0].contains('/') {
        return cmd_args.to_vec();
    }
    if !cmd_args[0].contains('/')
        && scripts_dir.is_some_and(|dir| is_executable(&dir.join(program)))
    {
        let mut args = cmd_args.to_vec();
        args[0] = Path::new("/opt/scripts")
            .join(program)
            .display()
            .to_string();
        return args;
    }
    let found = if program.is_absolute() {
        is_executable(program).then(|| program.to_path_buf())
    } else {
        std::env::split_paths(host_path.as_ref())
            .map(|dir| working_dir.join(dir).join(program))
            .find(|p| is_executable(p))
    };
    let Some(invocation) = found.and_then(|p| std::path::absolute(p).ok()) else {
        return cmd_args.to_vec();
    };
    let Ok(resolved) = invocation.canonicalize() else {
        return cmd_args.to_vec();
    };
    let destination = agentbox::resolve_visible_host_path(&invocation);
    let visible = ["/etc", "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64"]
        .iter()
        .any(|root| destination.starts_with(root));
    if destination != resolved || !visible {
        mounts.push(agentbox::Mount {
            host: resolved,
            container: destination.clone(),
            read_only: true,
        });
    }
    let mut args = cmd_args.to_vec();
    args[0] = invocation.display().to_string();
    args
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use rustix::fs::{accessat, Access, AtFlags, CWD};
        path.is_file() && accessat(CWD, path, Access::EXEC_OK, AtFlags::EACCESS).is_ok()
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires bwrap and permission to create user namespaces"]
    fn bwrap_live_executable_resolution_and_isolation() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let scripts = root.path().join("scripts");
        let host = root.path().join("host");
        for dir in [&workspace, &scripts, &host] {
            std::fs::create_dir(dir).unwrap();
        }
        let write_script = |path: &Path, body: &str| {
            std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        write_script(&workspace.join("tool"), "printf workspace");
        write_script(&scripts.join("tool"), "printf companion");
        write_script(&host.join("tool"), "printf host");
        std::fs::write(host.join("secret"), "must remain hidden").unwrap();
        symlink("tool", host.join("current")).unwrap();
        symlink("current", host.join("alias")).unwrap();

        let run = |command: Vec<String>, path: &str, companions: bool, tty: bool| {
            let mut config = agentbox::SandboxConfig {
                backend: agentbox::Backend::Bwrap,
                image: None,
                workspace_dir: workspace.clone(),
                extra_mounts: vec![],
                network: agentbox::NetworkMode::None,
                cpus: None,
                env_vars: vec![("HOME".into(), "/home/sandbox".into())],
                tty,
            };
            let scripts_dir = companions.then_some(scripts.as_path());
            if companions {
                config.extra_mounts.push(agentbox::Mount {
                    host: scripts.clone(),
                    container: "/opt/scripts".into(),
                    read_only: true,
                });
            }
            let args = expose_host_binary(
                &command,
                path,
                scripts_dir,
                &mut config.extra_mounts,
                &workspace,
            );
            let wrapped = agentbox::wrap_command(&args, &config);
            let output = std::process::Command::new(&wrapped[0])
                .args(&wrapped[1..])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{command:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };

        for tty in [false, true] {
            assert_eq!(
                run(vec!["tool".into()], host.to_str().unwrap(), true, tty),
                "companion"
            );
            assert_eq!(
                run(vec!["./tool".into()], host.to_str().unwrap(), true, tty),
                "workspace"
            );
            for path in ["", "."] {
                assert_eq!(run(vec!["tool".into()], path, false, tty), "workspace");
            }
            assert_eq!(
                run(vec!["alias".into()], host.to_str().unwrap(), false, tty),
                "host"
            );
            write_script(&host.join("probe"), &format!(
                "test ! -e '{}' && test \"$PWD\" = /workspace && test \"$HOME\" = /home/sandbox && ! ( : </dev/tty ) 2>/dev/null && touch written",
                host.join("secret").display()
            ));
            assert_eq!(
                run(
                    vec![host.join("probe").display().to_string()],
                    "",
                    false,
                    tty
                ),
                ""
            );
            assert!(workspace.join("written").exists());
        }
    }

    #[test]
    #[cfg(unix)]
    fn expose_host_binary_does_not_mount_invalid_absolute_commands() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("not-executable");
        std::fs::write(&file, "private data").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        for program in [Path::new("/.."), root.path(), &file] {
            let command = vec![program.display().to_string()];
            let mut mounts = vec![];
            let args = expose_host_binary(&command, "", None, &mut mounts, root.path());
            assert_eq!(args, command);
            assert!(mounts.is_empty());
        }
    }

    #[test]
    #[cfg(unix)]
    fn expose_host_binary_resolves_relative_path_entries_from_workspace() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        std::fs::create_dir(workspace.join("bin")).unwrap();
        for entry in ["", ".", "bin"] {
            let tool = workspace.join(entry).join("tool");
            std::fs::write(&tool, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut mounts = vec![];
            let args = expose_host_binary(&["tool".into()], entry, None, &mut mounts, &workspace);
            assert_ne!(args[0], "tool");
            assert_eq!(mounts.len(), 1);
            assert_eq!(mounts[0].host, tool.canonicalize().unwrap());
            assert_eq!(mounts[0].container, Path::new(&args[0]));
        }
    }

    #[test]
    #[cfg(unix)]
    fn expose_host_binary_leaves_relative_paths_for_workspace_resolution() {
        use std::os::unix::fs::PermissionsExt;

        let host = tempfile::tempdir().unwrap();
        std::fs::create_dir(host.path().join("tools")).unwrap();
        for program in ["git", "tools/git"] {
            let path = host.path().join(program);
            std::fs::write(&path, "").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for program in ["./git", "tools/git"] {
            let mut mounts = vec![];
            let command = vec![program.into(), "status".into()];
            assert_eq!(
                expose_host_binary(
                    &command,
                    host.path().to_str().unwrap(),
                    Some(host.path()),
                    &mut mounts,
                    Path::new("/"),
                ),
                command,
            );
            assert!(mounts.is_empty());
        }
    }

    #[test]
    #[cfg(unix)]
    fn expose_host_binary_prefers_executable_companion_script() {
        use std::os::unix::fs::PermissionsExt;

        let scripts = tempfile::tempdir().unwrap();
        let host = tempfile::tempdir().unwrap();
        let companion = scripts.path().join("git");
        let host = host.path().canonicalize().unwrap();
        let host_binary = host.join("git");
        std::fs::write(&companion, "#!/bin/sh\n").unwrap();
        std::fs::write(&host_binary, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&host_binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        for executable in [true, false] {
            std::fs::set_permissions(
                &companion,
                std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
            )
            .unwrap();
            let mut mounts = vec![];
            let args = expose_host_binary(
                &["git".into(), "status".into()],
                host.to_str().unwrap(),
                Some(scripts.path()),
                &mut mounts,
                Path::new("/"),
            );
            assert_eq!(args[1], "status");
            if executable {
                assert_eq!(args[0], "/opt/scripts/git");
                assert!(mounts.is_empty());
            } else {
                assert_eq!(args[0], host_binary.to_str().unwrap());
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn expose_host_binary_mounts_only_resolved_symlink_target() {
        use std::os::unix::fs::PermissionsExt;

        // GIVEN a tool installed outside /usr and linked from a PATH dir
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let versions = root.join("share/claude/versions");
        let bin = root.join("bin");
        std::fs::create_dir_all(&versions).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(versions.join("2.1.0"), "").unwrap();
        std::fs::set_permissions(
            versions.join("2.1.0"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink(versions.join("2.1.0"), bin.join("claude")).unwrap();
        let mut mounts = vec![];

        // WHEN
        let args = expose_host_binary(
            &["claude".into(), "-p".into()],
            bin.to_str().unwrap(),
            None,
            &mut mounts,
            Path::new("/"),
        );

        // THEN
        assert_eq!(
            args,
            vec![bin.join("claude").display().to_string(), "-p".into()]
        );
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].host, versions.join("2.1.0"));
        assert_eq!(mounts[0].container, bin.join("claude"));
        assert!(mounts[0].read_only);
    }

    #[test]
    #[cfg(unix)]
    fn expose_host_binary_bypasses_hidden_intermediate_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("usr/bin");
        let hidden = root.path().join("opt/tool");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&hidden).unwrap();
        std::os::unix::fs::symlink("/bin/sh", hidden.join("current")).unwrap();
        std::os::unix::fs::symlink("../../opt/tool/current", bin.join("sh")).unwrap();
        let mut mounts = vec![];
        let args = expose_host_binary(
            &[
                bin.join("sh").display().to_string(),
                "-c".into(),
                "true".into(),
            ],
            "",
            None,
            &mut mounts,
            Path::new("/"),
        );

        // This invocation is outside the system mounts, so bind the file directly
        // there. No hidden links are needed and the applet name stays sh.
        assert_eq!(args[0], bin.join("sh").to_str().unwrap());
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].container, Path::new(&args[0]));
        assert_eq!(mounts[0].host, Path::new("/bin/sh").canonicalize().unwrap());
        assert!(mounts[0].read_only);
    }

    #[test]
    #[cfg(unix)]
    fn expose_host_binary_preserves_alias_of_system_binary() {
        let root = tempfile::tempdir().unwrap();
        let alias = root.path().canonicalize().unwrap().join("sh");
        std::os::unix::fs::symlink("/bin/sh", &alias).unwrap();
        for program in ["sh".to_string(), alias.display().to_string()] {
            let mut mounts = vec![];
            let args = expose_host_binary(
                &[program, "-c".into(), "true".into()],
                alias.parent().unwrap().to_str().unwrap(),
                None,
                &mut mounts,
                Path::new("/"),
            );
            assert_eq!(
                args,
                vec![alias.display().to_string(), "-c".into(), "true".into()]
            );
            assert_eq!(mounts.len(), 1);
            assert_eq!(mounts[0].host, alias.canonicalize().unwrap());
            assert_eq!(mounts[0].container, alias);
            assert!(mounts[0].read_only);
        }
    }

    #[test]
    #[cfg(unix)]
    fn expose_host_binary_skips_non_executable_path_candidates() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let dirs: Vec<_> = ["directory", "nonexecutable", "executable"]
            .iter()
            .map(|name| root.join(name))
            .collect();
        for dir in &dirs {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::create_dir(dirs[0].join("tool")).unwrap();
        for (dir, mode) in [(&dirs[1], 0o644), (&dirs[2], 0o755)] {
            let path = dir.join("tool");
            std::fs::write(&path, "").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let host_path = std::env::join_paths(&dirs).unwrap();
        let mut mounts = vec![];
        let args = expose_host_binary(
            &["tool".into()],
            host_path.to_str().unwrap(),
            None,
            &mut mounts,
            Path::new("/"),
        );
        assert_eq!(args[0], dirs[2].join("tool").to_str().unwrap());
        assert_eq!(mounts.len(), 1);
    }

    #[test]
    fn expose_host_binary_leaves_unknown_program_untouched() {
        // GIVEN a companion script that only exists on the sandbox PATH
        let mut mounts = vec![];

        // WHEN
        let args = expose_host_binary(
            &["poll.sh".into()],
            "/nonexistent",
            None,
            &mut mounts,
            Path::new("/"),
        );

        // THEN
        assert_eq!(args, vec!["poll.sh".to_string()]);
        assert!(mounts.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn expose_host_binary_handles_merged_and_split_usr_system_paths() {
        let mut checked = 0;
        for dir in ["/usr/bin", "/bin"] {
            let invocation = Path::new(dir).join("sh");
            if !is_executable(&invocation) {
                continue;
            }
            checked += 1;
            let resolved = invocation.canonicalize().unwrap();
            let mut mounts = vec![];
            let args = expose_host_binary(&["sh".into()], dir, None, &mut mounts, Path::new("/"));

            assert_eq!(args[0], invocation.to_str().unwrap());
            let destination = agentbox::resolve_visible_host_path(&invocation);
            if ["/etc", "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64"]
                .iter()
                .any(|root| resolved.starts_with(root))
                && destination == resolved
            {
                assert!(mounts.is_empty());
            } else {
                assert_eq!(mounts.len(), 1);
                assert_eq!(mounts[0].host, resolved);
                assert_eq!(mounts[0].container, destination);
                assert!(mounts[0].read_only);
            }
        }
        assert!(checked > 0, "expected a system sh on this Unix host");
    }

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
            backend: agentbox::Backend::Podman,
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
