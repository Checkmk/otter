use std::path::Path;

use crate::types::{ResourceConfig, SandboxBackend, SandboxDef, StepDef, StepType};

/// Resolve sandbox configuration for a step, combining workflow-level and step-level settings.
///
/// Returns `None` when sandboxing is not active for this step.
pub fn resolve_sandbox_config(
    workflow_sandbox: Option<&SandboxDef>,
    step_def: &StepDef,
    workspace_dir: &Path,
    scripts_dir: Option<&Path>,
    resources: Option<&ResourceConfig>,
    _scratch_dir: &Path,
) -> Option<agentbox::SandboxConfig> {
    // [sandbox] present → on for all shell/agent steps; step.sandbox=false opts out.
    // [sandbox] absent  → off unless step.sandbox=true opts in.
    let enabled = step_def.sandbox.unwrap_or(workflow_sandbox.is_some());

    if !enabled {
        return None;
    }

    // Only sandbox shell and agent steps
    if !matches!(step_def.step_type, StepType::Shell | StepType::Agent) {
        return None;
    }

    let wf = workflow_sandbox.cloned().unwrap_or_default();

    let cpus = resources
        .and_then(|r| r.cpu_quota.as_deref())
        .and_then(agentbox::quota_to_cpus);

    let network = wf
        .network
        .as_deref()
        .map(agentbox::parse_network_mode)
        .unwrap_or_default();

    let mut extra_mounts = Vec::new();

    // Mount scripts dir if present
    if let Some(dir) = scripts_dir {
        extra_mounts.push(agentbox::Mount {
            host: dir.to_path_buf(),
            container: "/opt/scripts".into(),
            read_only: true,
        });
    }

    if let Some(home) = std::env::var_os("HOME") {
        extra_mounts.extend(claude_mounts(step_def, Path::new(&home)));
    }

    // Build env vars: safe system vars + PATH extension for scripts
    let mut env_vars: Vec<(String, String)> = Vec::new();

    // Inject safe system vars into container
    for key in &["HOME", "USER", "LANG", "LC_ALL", "TERM"] {
        if let Ok(val) = std::env::var(key) {
            env_vars.push((key.to_string(), val));
        }
    }
    // Override HOME to sandbox user's home
    env_vars.push(("HOME".to_string(), "/home/sandbox".to_string()));

    // Extend PATH inside container to include /opt/scripts if scripts_dir is set
    if scripts_dir.is_some() {
        env_vars.push((
            "PATH".to_string(),
            "/opt/scripts:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        ));
    }

    let backend = match wf.backend {
        SandboxBackend::Podman => agentbox::Backend::Podman,
        SandboxBackend::Bwrap => agentbox::Backend::Bwrap,
    };

    Some(agentbox::SandboxConfig {
        backend,
        image: wf.image,
        workspace_dir: workspace_dir.to_path_buf(),
        extra_mounts,
        network,
        cpus,
        env_vars,
        tty: false, // otter always uses piped stdio
    })
}

/// Claude agent steps get the host's `~/.claude` and `~/.claude.json` for auth and
/// session persistence.
fn claude_mounts(step_def: &StepDef, home: &Path) -> Vec<agentbox::Mount> {
    let is_claude_agent = step_def.step_type == StepType::Agent
        && step_def.agent.provider.as_deref() == Some("claude");
    if !is_claude_agent {
        return Vec::new();
    }

    let mut mounts = Vec::new();
    let claude_dir = home.join(".claude");
    if claude_dir.is_dir() {
        mounts.push(agentbox::Mount {
            host: claude_dir,
            container: "/home/sandbox/.claude".into(),
            read_only: false,
        });
    }
    let claude_json = home.join(".claude.json");
    if claude_json.is_file() {
        mounts.push(agentbox::Mount {
            host: claude_json,
            container: "/home/sandbox/.claude.json".into(),
            read_only: false,
        });
    }
    mounts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AgentConfig, StepType};

    fn shell_step() -> StepDef {
        StepDef {
            step_type: StepType::Shell,
            command: Some(vec!["echo".into(), "hello".into()]),
            message: None,
            message_file: None,
            session: None,
            notify: None,
            requires: None,
            sandbox: None,
            agent: AgentConfig::default(),
        }
    }

    #[test]
    fn returns_none_when_sandbox_not_enabled() {
        // GIVEN
        let step = shell_step();

        // WHEN
        let config = resolve_sandbox_config(
            None,
            &step,
            Path::new("/ws"),
            None,
            None,
            Path::new("/scratch"),
        );

        // THEN
        assert!(config.is_none());
    }

    #[test]
    fn returns_config_when_workflow_level_enabled() {
        // GIVEN
        let wf_sandbox = SandboxDef {
            backend: SandboxBackend::Podman,
            image: None,
            network: None,
        };
        let step = shell_step();

        // WHEN
        let config = resolve_sandbox_config(
            Some(&wf_sandbox),
            &step,
            Path::new("/ws"),
            None,
            None,
            Path::new("/scratch"),
        );

        // THEN
        assert!(config.is_some());
    }

    #[test]
    fn step_level_override_opts_out() {
        // GIVEN
        let wf_sandbox = SandboxDef {
            backend: SandboxBackend::Podman,
            image: None,
            network: None,
        };
        let mut step = shell_step();
        step.sandbox = Some(false);

        // WHEN
        let config = resolve_sandbox_config(
            Some(&wf_sandbox),
            &step,
            Path::new("/ws"),
            None,
            None,
            Path::new("/scratch"),
        );

        // THEN
        assert!(config.is_none());
    }

    #[test]
    fn step_level_override_opts_in() {
        // GIVEN — no workflow-level sandbox
        let mut step = shell_step();
        step.sandbox = Some(true);

        // WHEN
        let config = resolve_sandbox_config(
            None,
            &step,
            Path::new("/ws"),
            None,
            None,
            Path::new("/scratch"),
        );

        // THEN
        assert!(config.is_some());
    }

    #[test]
    fn checkpoint_step_is_never_sandboxed() {
        // GIVEN
        let wf_sandbox = SandboxDef {
            backend: SandboxBackend::Podman,
            image: None,
            network: None,
        };
        let step = StepDef {
            step_type: StepType::Checkpoint,
            sandbox: None,
            ..shell_step()
        };

        // WHEN
        let config = resolve_sandbox_config(
            Some(&wf_sandbox),
            &step,
            Path::new("/ws"),
            None,
            None,
            Path::new("/scratch"),
        );

        // THEN
        assert!(config.is_none());
    }

    #[test]
    fn cpu_quota_converted_to_cpus() {
        // GIVEN
        let wf_sandbox = SandboxDef {
            backend: SandboxBackend::Podman,
            image: None,
            network: None,
        };
        let resources = ResourceConfig {
            cpu_quota: Some("200%".to_string()),
        };
        let step = shell_step();

        // WHEN
        let config = resolve_sandbox_config(
            Some(&wf_sandbox),
            &step,
            Path::new("/ws"),
            None,
            Some(&resources),
            Path::new("/scratch"),
        );

        // THEN
        assert_eq!(config.unwrap().cpus, Some("2.0".to_string()));
    }

    #[test]
    fn custom_image_and_network_from_workflow() {
        // GIVEN
        let wf_sandbox = SandboxDef {
            backend: SandboxBackend::Podman,
            image: Some("my-image:v1".to_string()),
            network: Some("none".to_string()),
        };
        let step = shell_step();

        // WHEN
        let config = resolve_sandbox_config(
            Some(&wf_sandbox),
            &step,
            Path::new("/ws"),
            None,
            None,
            Path::new("/scratch"),
        )
        .unwrap();

        // THEN
        assert_eq!(config.image.as_deref(), Some("my-image:v1"));
        assert!(matches!(config.network, agentbox::NetworkMode::None));
    }

    #[test]
    fn scripts_dir_added_as_extra_mount() {
        // GIVEN
        let wf_sandbox = SandboxDef {
            backend: SandboxBackend::Podman,
            image: None,
            network: None,
        };
        let step = shell_step();

        // WHEN
        let config = resolve_sandbox_config(
            Some(&wf_sandbox),
            &step,
            Path::new("/ws"),
            Some(Path::new("/scripts")),
            None,
            Path::new("/scratch"),
        )
        .unwrap();

        // THEN
        assert!(config
            .extra_mounts
            .iter()
            .any(|m| m.container.to_str() == Some("/opt/scripts") && m.read_only));
    }

    fn claude_agent_step() -> StepDef {
        StepDef {
            step_type: StepType::Agent,
            command: None,
            message: Some("test".into()),
            message_file: None,
            session: None,
            notify: None,
            requires: None,
            sandbox: None,
            agent: AgentConfig {
                provider: Some("claude".into()),
                ..AgentConfig::default()
            },
        }
    }

    fn home_with_claude_config() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".claude")).unwrap();
        std::fs::write(home.path().join(".claude.json"), "{}").unwrap();
        home
    }

    fn containers(mounts: &[agentbox::Mount]) -> Vec<&str> {
        mounts
            .iter()
            .map(|m| m.container.to_str().unwrap())
            .collect()
    }

    #[test]
    fn claude_agent_gets_writable_claude_config_mounts() {
        // GIVEN
        let home = home_with_claude_config();
        let step = claude_agent_step();

        // WHEN
        let mounts = claude_mounts(&step, home.path());

        // THEN
        assert_eq!(
            containers(&mounts),
            ["/home/sandbox/.claude", "/home/sandbox/.claude.json"]
        );
        assert!(mounts.iter().all(|m| !m.read_only));
    }

    #[test]
    fn claude_agent_without_claude_config_gets_no_mounts() {
        // GIVEN
        let home = tempfile::tempdir().unwrap();
        let step = claude_agent_step();

        // WHEN
        let mounts = claude_mounts(&step, home.path());

        // THEN
        assert!(mounts.is_empty());
    }

    #[test]
    fn shell_step_does_not_get_claude_mount() {
        // GIVEN
        let home = home_with_claude_config();
        let step = shell_step();

        // WHEN
        let mounts = claude_mounts(&step, home.path());

        // THEN
        assert!(mounts.is_empty());
    }

    #[test]
    fn tty_is_always_false() {
        // GIVEN
        let wf_sandbox = SandboxDef {
            backend: SandboxBackend::Podman,
            image: None,
            network: None,
        };
        let step = shell_step();

        // WHEN
        let config = resolve_sandbox_config(
            Some(&wf_sandbox),
            &step,
            Path::new("/ws"),
            None,
            None,
            Path::new("/scratch"),
        )
        .unwrap();

        // THEN
        assert!(!config.tty);
    }
}
