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

    fn socket_unit_path(&self) -> PathBuf {
        self.unit_dir.join("otter.socket")
    }

    fn service_unit_path(&self) -> PathBuf {
        self.unit_dir.join("otter.service")
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
        std::fs::create_dir_all(&self.unit_dir)
            .context("create systemd user unit directory")?;

        let binary = std::env::current_exe().context("resolve current binary path")?;
        let binary_str = binary.to_string_lossy();
        let socket_str = crate::socket_path().display().to_string();

        std::fs::write(
            self.socket_unit_path(),
            format!(
                "[Unit]\n\
                 Description=Otter daemon socket\n\
                 \n\
                 [Socket]\n\
                 ListenStream={socket_str}\n\
                 RemoveOnStop=yes\n\
                 \n\
                 [Install]\n\
                 WantedBy=sockets.target\n"
            ),
        )
        .context("write otter.socket unit")?;

        std::fs::write(
            self.service_unit_path(),
            format!(
                "[Unit]\n\
                 Description=Otter workflow automation daemon\n\
                 Requires=otter.socket\n\
                 After=otter.socket\n\
                 \n\
                 [Service]\n\
                 ExecStart={binary_str} _daemon\n\
                 Restart=on-failure\n"
            ),
        )
        .context("write otter.service unit")?;

        Ok(())
    }
}

impl ServiceManager for SystemdServiceManager {
    fn enable(&self) -> anyhow::Result<()> {
        self.write_unit_files()?;
        self.systemctl(&["daemon-reload"])?;
        self.systemctl(&["enable", "--now", "otter.socket"])?;
        println!("otter service enabled. The service will start on boot.");
        Ok(())
    }

    fn disable(&self) -> anyhow::Result<()> {
        let _ = self.systemctl(&["stop", "otter.socket", "otter.service"]);
        let _ = self.systemctl(&["disable", "otter.socket", "otter.service"]);

        let _ = std::fs::remove_file(self.socket_unit_path());
        let _ = std::fs::remove_file(self.service_unit_path());

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
        self.socket_unit_path().exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn manager_in(dir: &std::path::Path) -> SystemdServiceManager {
        SystemdServiceManager { unit_dir: dir.to_path_buf() }
    }

    #[test]
    fn enable_writes_socket_and_service_units() {
        // GIVEN a temp unit directory
        let tmp = tempdir().unwrap();
        let mgr = manager_in(tmp.path());

        // WHEN unit files are written
        mgr.write_unit_files().unwrap();

        // THEN both unit files exist with expected content
        let socket_content = fs::read_to_string(mgr.socket_unit_path()).unwrap();
        assert!(socket_content.contains("ListenStream="));
        assert!(socket_content.contains("RemoveOnStop=yes"));
        assert!(socket_content.contains("WantedBy=sockets.target"));

        let service_content = fs::read_to_string(mgr.service_unit_path()).unwrap();
        assert!(service_content.contains("_daemon"));
        assert!(service_content.contains("Requires=otter.socket"));
        assert!(service_content.contains("Restart=on-failure"));
    }

    #[test]
    fn is_enabled_reflects_socket_unit_existence() {
        // GIVEN a temp unit directory with no files
        let tmp = tempdir().unwrap();
        let mgr = manager_in(tmp.path());
        assert!(!mgr.is_enabled());

        // WHEN the socket unit is written
        fs::write(mgr.socket_unit_path(), "[Socket]").unwrap();
        // THEN is_enabled returns true
        assert!(mgr.is_enabled());

        // WHEN it is removed
        fs::remove_file(mgr.socket_unit_path()).unwrap();
        // THEN is_enabled returns false again
        assert!(!mgr.is_enabled());
    }
}
