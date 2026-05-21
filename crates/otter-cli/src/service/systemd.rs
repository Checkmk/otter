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
}

impl ServiceManager for SystemdServiceManager {
    fn enable(&self) -> anyhow::Result<()> {
        // Clean up legacy socket unit from socket-activated versions before reloading.
        let legacy_socket = self.legacy_socket_unit_path();
        if legacy_socket.exists() {
            let _ = self.systemctl(&["disable", "--now", "otter.socket"]);
            let _ = std::fs::remove_file(&legacy_socket);
        }
        self.write_unit_files()?;
        self.systemctl(&["daemon-reload"])?;
        self.systemctl(&["enable", "--now", "otter.service"])?;
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
