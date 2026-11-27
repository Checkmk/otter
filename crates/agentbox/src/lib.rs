mod error;

pub use error::AgentboxError;

use std::path::{Path, PathBuf};

pub const DEFAULT_IMAGE: &str = "localhost/agentbox:latest";

const CONTAINERFILE_TEMPLATE: &str = include_str!("../Containerfile.template");

#[derive(Debug, Clone)]
pub struct Mount {
    pub host: PathBuf,
    pub container: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default)]
pub enum NetworkMode {
    #[default]
    Bridge,
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Backend {
    #[default]
    Podman,
    /// Experimental: bubblewrap over the host's read-only /usr and /etc; no image.
    Bwrap,
}

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub backend: Backend,
    pub image: Option<String>,
    pub workspace_dir: PathBuf,
    pub extra_mounts: Vec<Mount>,
    pub network: NetworkMode,
    pub cpus: Option<String>,
    pub env_vars: Vec<(String, String)>,
    pub tty: bool,
}

/// Wraps a command so it runs inside the sandbox selected by `config.backend`.
pub fn wrap_command(command: &[String], config: &SandboxConfig) -> Vec<String> {
    match config.backend {
        Backend::Podman => wrap_podman(command, config),
        Backend::Bwrap => wrap_bwrap(command, config, Path::new("/")),
    }
}

/// Mirrors podman's layout (`/workspace`, `/home/sandbox`, tmpfs scratch dirs) so
/// workflows behave the same under both backends. The sandbox root starts empty:
/// only the host's /usr and /etc (read-only) and explicit mounts are visible,
/// which hides $HOME and every socket under /run/user.
///
/// `host_root` is where the host's top-level dirs are inspected; tests point it at
/// a fixture to exercise merged and split /usr layouts.
fn wrap_bwrap(command: &[String], config: &SandboxConfig, host_root: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec!["bwrap".into(), "--die-with-parent".into()];
    // Unlike Podman, bwrap does not allocate a separate PTY for tty mode.
    // Always detach so the sandbox cannot inject host keystrokes (TIOCSTI).
    args.push("--new-session".into());
    args.push("--unshare-all".into());
    if matches!(config.network, NetworkMode::Bridge) {
        args.push("--share-net".into());
    }

    for dir in ["/usr", "/etc"] {
        args.extend(["--ro-bind-try".into(), dir.into(), dir.into()]);
    }
    for dir in ["/bin", "/sbin", "/lib", "/lib32", "/lib64"] {
        let host = host_root.join(dir.trim_start_matches('/'));
        match std::fs::read_link(&host) {
            Ok(target) => {
                args.extend(["--symlink".into(), target.display().to_string(), dir.into()])
            }
            Err(_) => args.extend(["--ro-bind-try".into(), dir.into(), dir.into()]),
        }
    }

    args.extend(
        [
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/run",
            "--tmpfs",
            "/home/sandbox",
        ]
        .map(String::from),
    );
    if matches!(config.network, NetworkMode::Bridge) {
        // Mount at the first hidden link target, bypassing any further hidden
        // symlinks. Mounting only the canonical target leaves those links broken.
        if let Ok(resolver) = host_root.join("etc/resolv.conf").canonicalize() {
            if let Ok(relative) = resolver.strip_prefix(host_root) {
                let source = Path::new("/").join(relative).display().to_string();
                let target = resolve_visible_host_path_at(Path::new("/etc/resolv.conf"), host_root);
                args.extend(["--ro-bind-try".into(), source, target.display().to_string()]);
            }
        }
    }

    args.extend([
        "--bind".into(),
        config.workspace_dir.display().to_string(),
        "/workspace".into(),
    ]);
    for mount in &config.extra_mounts {
        let flag = if mount.read_only {
            "--ro-bind"
        } else {
            "--bind"
        };
        args.extend([
            flag.into(),
            mount.host.display().to_string(),
            mount.container.display().to_string(),
        ]);
    }

    args.push("--clearenv".into());
    if !config.env_vars.iter().any(|(k, _)| k == "PATH") {
        args.extend(["--setenv", "PATH", "/usr/local/bin:/usr/bin:/bin"].map(String::from));
    }
    for (key, value) in &config.env_vars {
        args.extend(["--setenv".into(), key.clone(), value.clone()]);
    }

    args.extend(["--chdir".into(), "/workspace".into(), "--".into()]);
    args.extend_from_slice(command);
    args
}

/// Resolve links in the sandbox's visible system directories, stopping at the
/// first hidden path. Callers must first verify that the host path resolves.
pub fn resolve_visible_host_path(path: &Path) -> PathBuf {
    resolve_visible_host_path_at(path, Path::new("/"))
}

fn resolve_visible_host_path_at(path: &Path, host_root: &Path) -> PathBuf {
    use std::collections::VecDeque;

    let mut remaining: VecDeque<_> = path
        .components()
        .map(|part| part.as_os_str().to_os_string())
        .collect();
    let mut destination = PathBuf::new();
    while let Some(part) = remaining.pop_front() {
        if part == ".." {
            destination.pop();
            continue;
        }
        destination.push(part);
        let visible = ["/etc", "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64"]
            .iter()
            .any(|root| destination.starts_with(root));
        if destination != Path::new("/") && !visible {
            // The rest of this path is absent from the sandbox, so expose the
            // file here rather than recreating the hidden link chain.
            destination.extend(remaining);
            break;
        }
        let host = host_root.join(destination.strip_prefix("/").unwrap());
        if let Ok(target) = std::fs::read_link(host) {
            destination.pop();
            for part in target.components().rev() {
                remaining.push_front(part.as_os_str().to_os_string());
            }
        }
    }
    destination
}

fn wrap_podman(command: &[String], config: &SandboxConfig) -> Vec<String> {
    let image = config.image.as_deref().unwrap_or(DEFAULT_IMAGE);

    let mut args = vec![
        "podman".to_string(),
        "run".to_string(),
        "--rm".to_string(),
        "-i".to_string(),
    ];

    if config.tty {
        args.push("-t".to_string());
    }

    // Security hardening
    args.extend([
        "--cap-drop=ALL".to_string(),
        "--security-opt=no-new-privileges".to_string(),
        "--read-only".to_string(),
    ]);

    // Writable tmpfs areas
    args.extend([
        "--tmpfs".to_string(),
        "/tmp:rw,noexec,nosuid,size=1g".to_string(),
        "--tmpfs".to_string(),
        "/run:rw,noexec,nosuid".to_string(),
        "--tmpfs".to_string(),
        "/home/sandbox:rw,exec,nosuid,mode=1777".to_string(),
    ]);

    // UID mapping
    args.push("--userns=keep-id:uid=1000,gid=1000".to_string());

    // Network mode
    match config.network {
        NetworkMode::Bridge => args.push("--network=bridge".to_string()),
        NetworkMode::None => args.push("--network=none".to_string()),
    }

    // CPU limit
    if let Some(ref cpus) = config.cpus {
        args.push(format!("--cpus={cpus}"));
    }

    // Workspace bind mount
    args.extend([
        "-v".to_string(),
        format!("{}:/workspace:Z", config.workspace_dir.display()),
    ]);

    // Extra bind mounts
    for mount in &config.extra_mounts {
        let ro = if mount.read_only { "ro," } else { "" };
        args.extend([
            "-v".to_string(),
            format!(
                "{}:{}:{ro}Z",
                mount.host.display(),
                mount.container.display()
            ),
        ]);
    }

    // Environment variables
    for (key, value) in &config.env_vars {
        args.extend(["-e".to_string(), format!("{key}={value}")]);
    }

    // Working directory
    args.extend(["-w".to_string(), "/workspace".to_string()]);

    // Image
    args.push(image.to_string());

    // Command
    args.extend_from_slice(command);

    args
}

/// Build the default agentbox container image from the embedded Containerfile template.
pub async fn build_image(tag: Option<&str>) -> Result<(), AgentboxError> {
    check_podman().await?;

    let tag = tag.unwrap_or(DEFAULT_IMAGE);
    let dir = tempfile::tempdir().map_err(AgentboxError::Io)?;
    let containerfile = dir.path().join("Containerfile");
    std::fs::write(&containerfile, CONTAINERFILE_TEMPLATE).map_err(AgentboxError::Io)?;

    let output = tokio::process::Command::new("podman")
        .args(["build", "-t", tag, "-f"])
        .arg(&containerfile)
        .arg(dir.path())
        .output()
        .await
        .map_err(AgentboxError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let error_lines: Vec<&str> = stderr
            .lines()
            .filter(|l| {
                let lower = l.to_ascii_lowercase();
                lower.starts_with("error") || lower.contains(": error")
            })
            .collect();
        let message = if error_lines.is_empty() {
            // Fall back to last non-empty line if no error lines found
            stderr
                .lines()
                .rfind(|l| !l.trim().is_empty())
                .unwrap_or("unknown error")
                .to_string()
        } else {
            error_lines.join("\n")
        };
        return Err(AgentboxError::BuildFailed(message));
    }

    Ok(())
}

/// Export the embedded Containerfile template to a file on disk.
pub fn export_template(dest: &Path) -> Result<(), std::io::Error> {
    std::fs::write(dest, CONTAINERFILE_TEMPLATE)
}

/// Check that podman is installed and accessible.
pub async fn check_podman() -> Result<(), AgentboxError> {
    match tokio::process::Command::new("podman")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(AgentboxError::PodmanCheckFailed(format!(
            "podman exited with {}",
            status
        ))),
        Err(_) => Err(AgentboxError::PodmanNotFound),
    }
}

/// Convert a systemd-style CPU quota (e.g. "200%") to a podman --cpus value (e.g. "2.0").
pub fn quota_to_cpus(quota: &str) -> Option<String> {
    let pct = quota.trim_end_matches('%').parse::<f64>().ok()?;
    Some(format!("{:.1}", pct / 100.0))
}

/// Parse a network mode string from TOML config.
pub fn parse_network_mode(s: &str) -> NetworkMode {
    match s {
        "none" => NetworkMode::None,
        _ => NetworkMode::Bridge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic_config() -> SandboxConfig {
        SandboxConfig {
            backend: Backend::Podman,
            image: None,
            workspace_dir: PathBuf::from("/home/user/project"),
            extra_mounts: vec![],
            network: NetworkMode::Bridge,
            cpus: None,
            env_vars: vec![],
            tty: false,
        }
    }

    #[test]
    fn wrap_command_basic_produces_correct_podman_invocation() {
        // GIVEN
        let config = basic_config();
        let cmd = vec!["echo".to_string(), "hello".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert_eq!(result[0], "podman");
        assert_eq!(result[1], "run");
        assert_eq!(result[2], "--rm");
        assert_eq!(result[3], "-i");
        assert!(result.contains(&"--cap-drop=ALL".to_string()));
        assert!(result.contains(&"--security-opt=no-new-privileges".to_string()));
        assert!(result.contains(&"--read-only".to_string()));
        assert!(result.contains(&"--userns=keep-id:uid=1000,gid=1000".to_string()));
        assert!(result.contains(&"--network=bridge".to_string()));
        assert!(result.contains(&"-w".to_string()));
        assert!(result.contains(&"/workspace".to_string()));
        // Last two elements should be the command
        assert_eq!(&result[result.len() - 2..], &["echo", "hello"]);
    }

    #[test]
    fn wrap_command_does_not_include_tty_flag_when_false() {
        // GIVEN
        let config = basic_config();
        let cmd = vec!["echo".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(!result.contains(&"-t".to_string()));
    }

    #[test]
    fn wrap_command_includes_tty_flag_when_true() {
        // GIVEN
        let mut config = basic_config();
        config.tty = true;
        let cmd = vec!["echo".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(result.contains(&"-t".to_string()));
    }

    #[test]
    fn wrap_command_uses_custom_image() {
        // GIVEN
        let mut config = basic_config();
        config.image = Some("my-custom:latest".to_string());
        let cmd = vec!["claude".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(result.contains(&"my-custom:latest".to_string()));
        assert!(!result.contains(&DEFAULT_IMAGE.to_string()));
    }

    #[test]
    fn wrap_command_uses_default_image_when_none() {
        // GIVEN
        let config = basic_config();
        let cmd = vec!["claude".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(result.contains(&DEFAULT_IMAGE.to_string()));
    }

    #[test]
    fn wrap_command_sets_network_none() {
        // GIVEN
        let mut config = basic_config();
        config.network = NetworkMode::None;
        let cmd = vec!["echo".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(result.contains(&"--network=none".to_string()));
        assert!(!result.contains(&"--network=bridge".to_string()));
    }

    #[test]
    fn wrap_command_includes_cpus_when_set() {
        // GIVEN
        let mut config = basic_config();
        config.cpus = Some("2.0".to_string());
        let cmd = vec!["echo".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(result.contains(&"--cpus=2.0".to_string()));
    }

    #[test]
    fn wrap_command_omits_cpus_when_none() {
        // GIVEN
        let config = basic_config();
        let cmd = vec!["echo".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(!result.iter().any(|a| a.starts_with("--cpus=")));
    }

    #[test]
    fn wrap_command_includes_workspace_bind_mount() {
        // GIVEN
        let config = basic_config();
        let cmd = vec!["echo".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        let mount_arg = result
            .windows(2)
            .find(|w| w[0] == "-v" && w[1].contains("/workspace:Z"));
        assert!(mount_arg.is_some(), "should have workspace bind mount");
    }

    #[test]
    fn wrap_command_includes_extra_mounts() {
        // GIVEN
        let mut config = basic_config();
        config.extra_mounts = vec![
            Mount {
                host: PathBuf::from("/host/scripts"),
                container: PathBuf::from("/opt/scripts"),
                read_only: true,
            },
            Mount {
                host: PathBuf::from("/host/data"),
                container: PathBuf::from("/data"),
                read_only: false,
            },
        ];
        let cmd = vec!["echo".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(result
            .iter()
            .any(|a| a.contains("/host/scripts:/opt/scripts:ro,Z")));
        assert!(result.iter().any(|a| a.contains("/host/data:/data:Z")));
    }

    #[test]
    fn wrap_command_includes_env_vars() {
        // GIVEN
        let mut config = basic_config();
        config.env_vars = vec![
            ("API_KEY".to_string(), "secret123".to_string()),
            ("FOO".to_string(), "bar".to_string()),
        ];
        let cmd = vec!["echo".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(result.iter().any(|a| a == "API_KEY=secret123"));
        assert!(result.iter().any(|a| a == "FOO=bar"));
    }

    #[test]
    fn wrap_command_includes_tmpfs_mounts() {
        // GIVEN
        let config = basic_config();
        let cmd = vec!["echo".to_string()];

        // WHEN
        let result = wrap_command(&cmd, &config);

        // THEN
        assert!(result.iter().any(|a| a.contains("/tmp:")));
        assert!(result.iter().any(|a| a.contains("/run:")));
        assert!(result.iter().any(|a| a.contains("/home/sandbox:")));
    }

    #[test]
    fn quota_to_cpus_converts_correctly() {
        assert_eq!(quota_to_cpus("200%"), Some("2.0".to_string()));
        assert_eq!(quota_to_cpus("100%"), Some("1.0".to_string()));
        assert_eq!(quota_to_cpus("50%"), Some("0.5".to_string()));
        assert_eq!(quota_to_cpus("400%"), Some("4.0".to_string()));
    }

    #[test]
    fn quota_to_cpus_returns_none_for_invalid() {
        assert_eq!(quota_to_cpus("abc"), None);
        assert_eq!(quota_to_cpus(""), None);
    }

    #[test]
    fn parse_network_mode_parses_correctly() {
        assert!(matches!(parse_network_mode("none"), NetworkMode::None));
        assert!(matches!(parse_network_mode("bridge"), NetworkMode::Bridge));
        assert!(matches!(
            parse_network_mode("anything"),
            NetworkMode::Bridge
        ));
    }

    // ── bwrap backend ──────────────────────────────────────────────────────

    fn bwrap_config() -> SandboxConfig {
        SandboxConfig {
            backend: Backend::Bwrap,
            ..basic_config()
        }
    }

    fn has_seq(args: &[String], seq: &[&str]) -> bool {
        args.windows(seq.len())
            .any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
    }

    fn cmd(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bwrap_always_detaches_from_the_controlling_terminal() {
        for tty in [false, true] {
            let config = SandboxConfig {
                tty,
                ..bwrap_config()
            };
            let args = wrap_command(&cmd(&["sh"]), &config);
            assert!(args.contains(&"--new-session".to_string()));
        }
    }

    #[test]
    fn bwrap_runs_command_in_workspace_with_fresh_home() {
        // GIVEN
        let config = bwrap_config();

        // WHEN
        let args = wrap_command(&cmd(&["claude", "-p", "hi"]), &config);

        // THEN
        assert_eq!(args[0], "bwrap");
        assert!(has_seq(&args, &["--unshare-all"]));
        assert!(has_seq(
            &args,
            &["--bind", "/home/user/project", "/workspace"]
        ));
        assert!(has_seq(&args, &["--tmpfs", "/home/sandbox"]));
        assert!(has_seq(&args, &["--ro-bind-try", "/usr", "/usr"]));
        assert!(has_seq(
            &args,
            &["--chdir", "/workspace", "--", "claude", "-p", "hi"]
        ));
        assert!(args.ends_with(&cmd(&["claude", "-p", "hi"])));
    }

    #[test]
    #[cfg(unix)]
    fn bwrap_bridge_shares_network_and_dns() {
        // GIVEN
        let config = bwrap_config();
        for resolver in [
            "etc/resolv.conf",
            "run/systemd/resolve/stub-resolv.conf",
            "run/NetworkManager/resolv.conf",
            "run/resolvconf/resolv.conf",
        ] {
            let root = tempfile::tempdir().unwrap();
            let root = root.path().canonicalize().unwrap();
            let target = root.join(resolver);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, "nameserver 127.0.0.53\n").unwrap();
            if resolver != "etc/resolv.conf" {
                std::fs::create_dir(root.join("etc")).unwrap();
                std::os::unix::fs::symlink(format!("../{resolver}"), root.join("etc/resolv.conf"))
                    .unwrap();
            }

            // WHEN
            let args = wrap_bwrap(&cmd(&["true"]), &config, &root);

            // THEN
            assert!(has_seq(&args, &["--share-net"]));
            let destination = format!("/{resolver}");
            assert!(has_seq(
                &args,
                &["--ro-bind-try", &destination, &destination]
            ));
            let isolated = SandboxConfig {
                network: NetworkMode::None,
                ..config.clone()
            };
            let args = wrap_bwrap(&cmd(&["true"]), &isolated, &root);
            assert!(!args.contains(&destination));
        }
    }

    #[test]
    #[cfg(unix)]
    fn bwrap_dns_bypasses_hidden_intermediate_links() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("etc/alternatives")).unwrap();
        std::fs::create_dir_all(root.join("run/NetworkManager")).unwrap();
        std::fs::create_dir_all(root.join("run/real")).unwrap();
        std::fs::write(root.join("run/real/resolv.conf"), "nameserver 127.0.0.53\n").unwrap();
        std::os::unix::fs::symlink(
            "../real/resolv.conf",
            root.join("run/NetworkManager/current"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "../../run/NetworkManager/current",
            root.join("etc/alternatives/dns"),
        )
        .unwrap();
        std::os::unix::fs::symlink("alternatives/dns", root.join("etc/resolv.conf")).unwrap();

        let args = wrap_bwrap(&cmd(&["true"]), &bwrap_config(), &root);
        assert!(has_seq(
            &args,
            &[
                "--ro-bind-try",
                "/run/real/resolv.conf",
                "/run/NetworkManager/current"
            ]
        ));
    }

    #[test]
    #[cfg(unix)]
    fn bwrap_dns_preserves_var_run_alias() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        for dir in ["etc", "var", "run/NetworkManager"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(
            root.join("run/NetworkManager/resolv.conf"),
            "nameserver 127.0.0.53\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("../run", root.join("var/run")).unwrap();
        std::os::unix::fs::symlink(
            "../var/run/NetworkManager/resolv.conf",
            root.join("etc/resolv.conf"),
        )
        .unwrap();
        let args = wrap_bwrap(&cmd(&["true"]), &bwrap_config(), &root);
        assert!(has_seq(
            &args,
            &[
                "--ro-bind-try",
                "/run/NetworkManager/resolv.conf",
                "/var/run/NetworkManager/resolv.conf"
            ]
        ));
    }

    #[test]
    #[cfg(unix)]
    fn visible_host_links_stop_at_the_first_hidden_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        for dir in [
            "usr/bin",
            "usr/share/tool",
            "opt/tool/versions",
            "etc/alternatives",
        ] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        // A script that uses adjacent files must retain its visible location.
        symlink("../share/tool/main.js", root.path().join("usr/bin/tool")).unwrap();
        assert_eq!(
            resolve_visible_host_path_at(Path::new("/usr/bin/tool"), root.path()),
            Path::new("/usr/share/tool/main.js")
        );

        // The target goes hidden, then returns to a visible system directory.
        symlink("/opt/tool/current", root.path().join("usr/bin/hidden")).unwrap();
        symlink("/usr/bin/tool", root.path().join("opt/tool/current")).unwrap();
        assert_eq!(
            resolve_visible_host_path_at(Path::new("/usr/bin/hidden"), root.path()),
            Path::new("/opt/tool/current")
        );

        // A directory symlink can also leave the visible filesystem.
        symlink("/opt/tool/versions", root.path().join("usr/bin/tools")).unwrap();
        assert_eq!(
            resolve_visible_host_path_at(Path::new("/usr/bin/tools/v1"), root.path()),
            Path::new("/opt/tool/versions/v1")
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    #[ignore = "requires bwrap and permission to create user namespaces"]
    fn bwrap_live_visible_and_hidden_executable_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let root = tempfile::tempdir().unwrap();
        for dir in [
            "usr/local/bin",
            "usr/local/share/tool",
            "opt/tool/versions",
            "workspace",
        ] {
            std::fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        for (file, body) in [
            (
                "usr/local/share/tool/main.sh",
                "cat \"$(dirname \"$(readlink -f \"$0\")\")/data\"",
            ),
            ("opt/tool/versions/v1", "printf hidden"),
        ] {
            let path = root.path().join(file);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(root.path().join("usr/local/share/tool/data"), "visible").unwrap();
        symlink(
            "../share/tool/main.sh",
            root.path().join("usr/local/bin/tool"),
        )
        .unwrap();
        symlink(
            "../../../opt/tool/current",
            root.path().join("usr/local/bin/hidden"),
        )
        .unwrap();
        symlink("versions/v1", root.path().join("opt/tool/current")).unwrap();

        for (name, expected) in [("tool", "visible"), ("hidden", "hidden")] {
            let invocation = Path::new("/usr/local/bin").join(name);
            let mut config = SandboxConfig {
                workspace_dir: root.path().join("workspace"),
                network: NetworkMode::None,
                ..bwrap_config()
            };
            for dir in ["usr/local/bin", "usr/local/share"] {
                config.extra_mounts.push(Mount {
                    host: root.path().join(dir),
                    container: Path::new("/").join(dir),
                    read_only: true,
                });
            }
            let destination = resolve_visible_host_path_at(&invocation, root.path());
            if name == "hidden" {
                config.extra_mounts.push(Mount {
                    host: root
                        .path()
                        .join("usr/local/bin/hidden")
                        .canonicalize()
                        .unwrap(),
                    container: destination,
                    read_only: true,
                });
            }
            let args = wrap_command(&[invocation.display().to_string()], &config);
            let output = std::process::Command::new(&args[0])
                .args(&args[1..])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        }
    }

    #[test]
    fn bwrap_network_none_keeps_network_unshared() {
        // GIVEN
        let config = SandboxConfig {
            network: NetworkMode::None,
            ..bwrap_config()
        };

        // WHEN
        let args = wrap_command(&cmd(&["true"]), &config);

        // THEN
        assert!(!args.contains(&"--share-net".to_string()));
        assert!(!args.contains(&"/run/systemd/resolve".to_string()));
    }

    #[test]
    fn bwrap_clears_env_and_sets_only_configured_vars() {
        // GIVEN
        let config = SandboxConfig {
            env_vars: vec![("JIRA_PAT".into(), "secret".into())],
            ..bwrap_config()
        };

        // WHEN
        let args = wrap_command(&cmd(&["true"]), &config);

        // THEN
        assert!(has_seq(&args, &["--clearenv"]));
        assert!(has_seq(&args, &["--setenv", "JIRA_PAT", "secret"]));
        assert!(has_seq(
            &args,
            &["--setenv", "PATH", "/usr/local/bin:/usr/bin:/bin"]
        ));
    }

    #[test]
    fn bwrap_keeps_configured_path() {
        // GIVEN
        let config = SandboxConfig {
            env_vars: vec![("PATH".into(), "/opt/scripts:/usr/bin".into())],
            ..bwrap_config()
        };

        // WHEN
        let args = wrap_command(&cmd(&["true"]), &config);

        // THEN
        let path_settings = args.iter().filter(|a| *a == "PATH").count();
        assert_eq!(path_settings, 1);
        assert!(has_seq(
            &args,
            &["--setenv", "PATH", "/opt/scripts:/usr/bin"]
        ));
    }

    #[test]
    fn bwrap_extra_mounts_respect_read_only() {
        // GIVEN
        let config = SandboxConfig {
            extra_mounts: vec![
                Mount {
                    host: "/pkg".into(),
                    container: "/opt/scripts".into(),
                    read_only: true,
                },
                Mount {
                    host: "/home/u/.claude".into(),
                    container: "/home/sandbox/.claude".into(),
                    read_only: false,
                },
            ],
            ..bwrap_config()
        };

        // WHEN
        let args = wrap_command(&cmd(&["true"]), &config);

        // THEN
        assert!(has_seq(&args, &["--ro-bind", "/pkg", "/opt/scripts"]));
        assert!(has_seq(
            &args,
            &["--bind", "/home/u/.claude", "/home/sandbox/.claude"]
        ));
    }

    #[test]
    #[cfg(unix)]
    fn bwrap_mirrors_host_top_level_layout() {
        // GIVEN a host with merged /bin but a real /lib64 directory
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("usr/bin", root.path().join("bin")).unwrap();
        std::fs::create_dir(root.path().join("lib64")).unwrap();

        // WHEN
        let args = wrap_bwrap(&cmd(&["true"]), &bwrap_config(), root.path());

        // THEN
        assert!(has_seq(&args, &["--symlink", "usr/bin", "/bin"]));
        assert!(has_seq(&args, &["--ro-bind-try", "/lib64", "/lib64"]));
    }

    /// Runs the real bwrap; skipped where it is missing or user namespaces are blocked.
    #[test]
    fn bwrap_sandbox_hides_home_and_allows_workspace_writes() {
        let usable = std::process::Command::new("bwrap")
            .args(["--ro-bind", "/", "/", "--unshare-user", "true"])
            .status()
            .is_ok_and(|s| s.success());
        if !usable {
            eprintln!("skipping: bwrap unavailable");
            return;
        }

        // GIVEN
        let workspace = tempfile::tempdir().unwrap();
        let scripts = tempfile::tempdir().unwrap();
        std::fs::write(scripts.path().join("poll.sh"), "").unwrap();
        let home = std::env::var("HOME").unwrap();
        let config = SandboxConfig {
            workspace_dir: workspace.path().to_path_buf(),
            network: NetworkMode::None,
            extra_mounts: vec![Mount {
                host: scripts.path().to_path_buf(),
                container: "/opt/scripts".into(),
                read_only: true,
            }],
            ..bwrap_config()
        };
        let script = format!(
            "test ! -e '{home}' && test -e /opt/scripts/poll.sh && touch /workspace/written \
             && ! touch /usr/forbidden 2>/dev/null"
        );

        // WHEN
        let args = wrap_command(&cmd(&["sh", "-c", &script]), &config);
        let status = std::process::Command::new(&args[0])
            .args(&args[1..])
            .status()
            .unwrap();

        // THEN
        assert!(status.success(), "sandbox checks failed: {args:?}");
        assert!(workspace.path().join("written").exists());
    }
}
