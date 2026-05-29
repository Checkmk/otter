use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;

use super::ServiceManager;

pub struct SystemdServiceManager {
    unit_dir: PathBuf,
}

impl SystemdServiceManager {
    pub fn new() -> Self {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()));
        let unit_dir = home.join(".config/systemd/user");
        Self { unit_dir }
    }

    fn service_unit_path(&self) -> PathBuf {
        self.unit_dir.join("otter.service")
    }

    /// Path to the legacy socket unit from earlier socket-activated versions.
    /// Used so `enable()` can clean it up on upgrade.
    fn legacy_socket_unit_path(&self) -> PathBuf {
        self.unit_dir.join("otter.socket")
    }

    fn systemctl(&self, args: &[&str]) -> anyhow::Result<()> {
        let status = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .status()
            .context("failed to run systemctl")?;
        if !status.success() {
            anyhow::bail!("systemctl --user {} failed", args.join(" "));
        }
        Ok(())
    }

    fn write_unit_files(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.unit_dir).context("create systemd user unit directory")?;

        let binary = std::env::current_exe().context("resolve current binary path")?;
        let binary_str = binary.to_string_lossy();

        std::fs::write(
            self.service_unit_path(),
            format!(
                "[Unit]\n\
                 Description=Otter workflow automation daemon\n\
                 \n\
                 [Service]\n\
                 Environment=SHELL=/bin/bash\n\
                 ExecStart={binary_str} _daemon\n\
                 Restart=on-failure\n\
                 \n\
                 [Install]\n\
                 WantedBy=default.target\n"
            ),
        )
        .context("write otter.service unit")?;

        Ok(())
    }

    /// Remove the legacy socket-activation unit left behind by older,
    /// socket-activated otter versions.
    fn cleanup_legacy_socket(&self) -> bool {
        let legacy_socket = self.legacy_socket_unit_path();
        if !legacy_socket.exists() {
            return false;
        }
        let _ = self.systemctl(&["disable", "--now", "otter.socket"]);
        let _ = std::fs::remove_file(&legacy_socket);
        true
    }

    /// Clean up a leftover legacy socket unit before (re)starting and clear any
    /// failed state the resulting restart loop may have left on the service.
    fn migrate_legacy_socket_before_start(&self) -> anyhow::Result<()> {
        if self.cleanup_legacy_socket() {
            let _ = self.systemctl(&["reset-failed", "otter.service"]);
            self.systemctl(&["daemon-reload"])?;
        }
        Ok(())
    }
}

/// Ensure linger is enabled for the current user so the service keeps running after logout.
/// On modern systemd, `loginctl enable-linger` (no username) targets the calling user and
/// is allowed via polkit without escalation; on stricter setups it may prompt or fail.
/// Failures are non-fatal — we surface a hint and let `enable()` succeed regardless.
fn enable_linger() {
    if linger_enabled() {
        return;
    }
    let status = Command::new("loginctl").arg("enable-linger").status();
    match status {
        Ok(s) if s.success() => {}
        _ => {
            let user = std::env::var("USER").unwrap_or_else(|_| "<user>".into());
            eprintln!(
                "warning: could not enable linger for this user. The service will stop at logout.\n\
                 To keep it running across logouts, run:  sudo loginctl enable-linger {user}"
            );
        }
    }
}

fn linger_enabled() -> bool {
    Command::new("loginctl")
        .args(["show-user", "--property=Linger", "--value"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
        .unwrap_or(false)
}

impl ServiceManager for SystemdServiceManager {
    fn enable(&self) -> anyhow::Result<()> {
        // Clean up legacy socket unit from socket-activated versions before reloading.
        self.cleanup_legacy_socket();
        self.write_unit_files()?;
        self.systemctl(&["daemon-reload"])?;
        self.systemctl(&["enable", "--now", "otter.service"])?;
        enable_linger();
        println!("otter service enabled. The service will start on login.");
        Ok(())
    }

    fn disable(&self) -> anyhow::Result<()> {
        let _ = self.systemctl(&["stop", "otter.service"]);
        let _ = self.systemctl(&["disable", "otter.service"]);

        let _ = std::fs::remove_file(self.service_unit_path());
        let _ = std::fs::remove_file(self.legacy_socket_unit_path());

        self.systemctl(&["daemon-reload"])?;

        println!("otter service disabled.");
        Ok(())
    }

    fn start(&self) -> anyhow::Result<()> {
        if self.service_unit_path().exists() {
            self.migrate_legacy_socket_before_start()?;
            return self.systemctl(&["start", "otter.service"]);
        }
        super::start_session_daemon()
    }

    fn stop(&self) -> anyhow::Result<()> {
        if self.service_unit_path().exists() {
            return self.systemctl(&["stop", "otter.service"]);
        }
        super::stop_session_daemon()
    }

    fn restart(&self) -> anyhow::Result<()> {
        if self.service_unit_path().exists() {
            self.migrate_legacy_socket_before_start()?;
            return self.systemctl(&["restart", "otter.service"]);
        }
        super::restart_session_daemon()
    }

    fn is_enabled(&self) -> bool {
        self.service_unit_path().exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn manager_in(dir: &std::path::Path) -> SystemdServiceManager {
        SystemdServiceManager {
            unit_dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn enable_writes_service_unit() {
        // GIVEN a temp unit directory
        let tmp = tempdir().unwrap();
        let mgr = manager_in(tmp.path());

        // WHEN unit files are written
        mgr.write_unit_files().unwrap();

        // THEN the service unit exists with expected content
        let service_content = fs::read_to_string(mgr.service_unit_path()).unwrap();
        assert!(service_content.contains("_daemon"));
        assert!(service_content.contains("WantedBy=default.target"));
        assert!(service_content.contains("Restart=on-failure"));
        assert!(service_content.contains("Environment=SHELL=/bin/bash"));
        // No socket unit is written
        assert!(!mgr.legacy_socket_unit_path().exists());
    }

    #[test]
    fn cleanup_legacy_socket_removes_leftover_unit() {
        // GIVEN a unit dir with a leftover legacy otter.socket from an old version
        let tmp = tempdir().unwrap();
        let mgr = manager_in(tmp.path());
        fs::write(
            mgr.legacy_socket_unit_path(),
            "[Socket]\nListenStream=...\n",
        )
        .unwrap();

        // WHEN the legacy socket is cleaned up
        let removed = mgr.cleanup_legacy_socket();

        // THEN it reports removal and the unit file is gone
        assert!(removed);
        assert!(!mgr.legacy_socket_unit_path().exists());
    }

    #[test]
    fn cleanup_legacy_socket_is_noop_without_leftover() {
        // GIVEN a unit dir with no legacy socket unit
        let tmp = tempdir().unwrap();
        let mgr = manager_in(tmp.path());

        // WHEN/THEN cleanup reports nothing to do
        assert!(!mgr.cleanup_legacy_socket());
    }

    #[test]
    fn is_enabled_reflects_service_unit_existence() {
        // GIVEN a temp unit directory with no files
        let tmp = tempdir().unwrap();
        let mgr = manager_in(tmp.path());
        assert!(!mgr.is_enabled());

        // WHEN the service unit is written
        fs::write(mgr.service_unit_path(), "[Service]").unwrap();
        // THEN is_enabled returns true
        assert!(mgr.is_enabled());

        // WHEN it is removed
        fs::remove_file(mgr.service_unit_path()).unwrap();
        // THEN is_enabled returns false again
        assert!(!mgr.is_enabled());
    }
}
