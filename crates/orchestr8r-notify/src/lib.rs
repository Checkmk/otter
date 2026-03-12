pub struct Notification {
    pub summary: String,
    pub body: String,
}

#[derive(thiserror::Error, Debug)]
pub enum NotifyError {
    #[error("notification error: {0}")]
    Failed(String),
}

#[async_trait::async_trait]
pub trait Notifier: Send + Sync {
    fn name(&self) -> &str;
    async fn send(&self, notification: &Notification) -> Result<(), NotifyError>;
}

/// Silently discards all notifications. Used in tests and when no notifier is configured.
pub struct NoOpNotifier;

#[async_trait::async_trait]
impl Notifier for NoOpNotifier {
    fn name(&self) -> &str {
        "noop"
    }
    async fn send(&self, _: &Notification) -> Result<(), NotifyError> {
        Ok(())
    }
}

/// Sends OS desktop notifications via `notify-rust`.
pub struct DesktopNotifier;

#[async_trait::async_trait]
impl Notifier for DesktopNotifier {
    fn name(&self) -> &str {
        "desktop"
    }
    async fn send(&self, n: &Notification) -> Result<(), NotifyError> {
        notify_rust::Notification::new()
            .summary(&n.summary)
            .body(&n.body)
            .show()
            .map_err(|e| NotifyError::Failed(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_notifier_always_succeeds() {
        let notifier = NoOpNotifier;
        let result = notifier
            .send(&Notification {
                summary: "test".to_string(),
                body: "body".to_string(),
            })
            .await;
        assert!(result.is_ok());
    }
}
