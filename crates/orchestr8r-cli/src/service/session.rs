use super::ServiceManager;

pub struct SessionServiceManager;

impl ServiceManager for SessionServiceManager {
    fn enable(&self) -> anyhow::Result<()> {
        anyhow::bail!("service management is not supported on this platform")
    }

    fn disable(&self) -> anyhow::Result<()> {
        anyhow::bail!("service management is not supported on this platform")
    }

    fn start(&self) -> anyhow::Result<()> {
        super::start_session_daemon()
    }

    fn stop(&self) -> anyhow::Result<()> {
        super::stop_session_daemon()
    }

    fn is_enabled(&self) -> bool {
        false
    }
}
