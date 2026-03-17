use async_trait::async_trait;
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::types::{TriggerError, TriggerEvent};
use super::TriggerSource;

pub struct ManualTrigger {
    name: String,
    signal_path: PathBuf,
}

impl ManualTrigger {
    pub fn new(name: String, signal_path: PathBuf) -> Self {
        Self { name, signal_path }
    }
}

#[async_trait]
impl TriggerSource for ManualTrigger {
    fn name(&self) -> &str {
        &self.name
    }

    async fn subscribe(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError> {
        loop {
            if self.signal_path.exists() {
                let _ = std::fs::remove_file(&self.signal_path);
                tx.send(TriggerEvent {
                    source: self.name.clone(),
                    payload: String::new(),
                    preallocated_run_id: None,
                    resolved_workspace: None,
                })
                .await
                .map_err(|e| TriggerError::Failed(e.to_string()))?;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }

    async fn fire_once(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError> {
        tx.send(TriggerEvent {
            source: self.name.clone(),
            payload: String::new(),
            preallocated_run_id: None,
            resolved_workspace: None,
        })
        .await
        .map_err(|e| TriggerError::Failed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fire_once_emits_one_event() {
        // GIVEN
        let trigger = ManualTrigger::new("manual".to_string(), "/tmp/signal".into());
        let (tx, mut rx) = mpsc::channel(8);

        // WHEN
        trigger.fire_once(tx).await.unwrap();

        // THEN
        assert!(rx.recv().await.is_some(), "expected one event");
        assert!(rx.recv().await.is_none(), "should only emit one event");
    }
}
