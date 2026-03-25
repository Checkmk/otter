#[derive(Debug, thiserror::Error)]
pub enum AgentboxError {
    #[error("podman not found — install podman for sandbox support")]
    PodmanNotFound,
    #[error("podman check failed: {0}")]
    PodmanCheckFailed(String),
    #[error("image build failed: {0}")]
    BuildFailed(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
