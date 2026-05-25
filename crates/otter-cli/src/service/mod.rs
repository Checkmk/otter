#[cfg(not(target_os = "linux"))]
mod session;
#[cfg(target_os = "linux")]
mod systemd;

pub trait ServiceManager {
    /// Install and enable automatic socket-activated startup (persists across reboots).
    fn enable(&self) -> anyhow::Result<()>;
    /// Disable automatic startup and stop any running service.
    fn disable(&self) -> anyhow::Result<()>;
    /// Start the daemon for this session without enabling boot persistence.
    fn start(&self) -> anyhow::Result<()>;
    /// Stop the running daemon.
    fn stop(&self) -> anyhow::Result<()>;
    /// Returns true if the service is configured for automatic startup (e.g. systemd enabled).
    fn is_enabled(&self) -> bool;
}

pub fn platform_service_manager() -> Box<dyn ServiceManager> {
    #[cfg(target_os = "linux")]
    return Box::new(systemd::SystemdServiceManager::new());
    #[cfg(not(target_os = "linux"))]
    return Box::new(session::SessionServiceManager);
}

/// Spawn the daemon as a background process and wait for its socket to become available.
pub(super) fn start_session_daemon() -> anyhow::Result<()> {
    use anyhow::Context;
    if is_service_running() {
        println!("Service is already running.");
        return Ok(());
    }
    let binary = std::env::current_exe().context("resolve binary path")?;
    std::process::Command::new(binary)
        .arg("_daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("failed to spawn service")?;
    // Wait until the socket is ready (up to 5 s).
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if is_service_running() {
            println!("Service started.");
            return Ok(());
        }
    }
    anyhow::bail!("service did not start within 5 seconds")
}

/// Send a termination signal to the daemon via the pid file written by the daemon.
pub(super) fn stop_session_daemon() -> anyhow::Result<()> {
    use anyhow::Context;
    let pid_path = crate::dirs_data_dir().join("daemon.pid");
    let pid_str = std::fs::read_to_string(&pid_path)
        .context("service is not running (pid file not found)")?;
    let pid = pid_str.trim().to_string();
    kill_by_pid(&pid)?;
    println!("Service stopped.");
    Ok(())
}

pub(crate) fn is_service_running() -> bool {
    let path = crate::socket_path();
    #[cfg(not(target_os = "windows"))]
    return std::os::unix::net::UnixStream::connect(&path).is_ok();
    #[cfg(target_os = "windows")]
    return std::fs::OpenOptions::new().read(true).open(&path).is_ok();
}

fn kill_by_pid(pid: &str) -> anyhow::Result<()> {
    use anyhow::Context;
    #[cfg(not(target_os = "windows"))]
    {
        let status = std::process::Command::new("kill")
            .arg(pid)
            .status()
            .context("failed to send SIGTERM to service")?;
        anyhow::ensure!(status.success(), "kill {} failed", pid);
    }
    #[cfg(target_os = "windows")]
    {
        let status = std::process::Command::new("taskkill")
            .args(["/PID", pid, "/F"])
            .status()
            .context("failed to terminate service")?;
        anyhow::ensure!(status.success(), "taskkill /PID {} failed", pid);
    }
    Ok(())
}
