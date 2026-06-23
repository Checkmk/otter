use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::types::{TriggerError, TriggerEvent};

use super::TriggerSource;

/// Fires a single event immediately, then completes. Used to trigger a
/// one-off run of a triggered workflow on demand (e.g. via `start`).
pub struct OneShotTrigger {
    name: String,
}

impl OneShotTrigger {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

#[async_trait]
impl TriggerSource for OneShotTrigger {
    fn name(&self) -> &str {
        &self.name
    }

    async fn fire_once(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError> {
        tx.send(TriggerEvent {
            source: self.name.clone(),
            payload: String::new(),
            preallocated_run_id: None,
            pending_context: None,
            inline_context: None,
        })
        .await
        .map_err(|_| TriggerError::Failed("receiver dropped".to_string()))
    }

    async fn subscribe(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError> {
        self.fire_once(tx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn fires_exactly_once() {
        // GIVEN
        let trigger = OneShotTrigger::new("oneshot");
        let (tx, mut rx) = mpsc::channel(8);

        // WHEN
        trigger.subscribe(tx).await.unwrap();

        // THEN
        let event = rx.recv().await.expect("expected one event");
        assert_eq!(event.source, "oneshot");
        assert!(
            rx.recv().await.is_none(),
            "channel should be empty after one event"
        );
    }

    #[tokio::test]
    async fn fire_once_emits_one_event() {
        // GIVEN
        let trigger = OneShotTrigger::new("oneshot");
        let (tx, mut rx) = mpsc::channel(8);

        // WHEN
        trigger.fire_once(tx).await.unwrap();

        // THEN
        assert!(rx.recv().await.is_some(), "expected one event");
        assert!(rx.recv().await.is_none(), "should only emit one event");
    }
}
