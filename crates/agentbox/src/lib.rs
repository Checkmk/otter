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

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub image: Option<String>,
    pub workspace_dir: PathBuf,
    pub extra_mounts: Vec<Mount>,
    pub network: NetworkMode,
    pub cpus: Option<String>,
    pub env_vars: Vec<(String, String)>,
    pub tty: bool,
}

/// Wraps a command in a `podman run` invocation with security hardening.
pub fn wrap_command(command: &[String], config: &SandboxConfig) -> Vec<String> {
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
}
