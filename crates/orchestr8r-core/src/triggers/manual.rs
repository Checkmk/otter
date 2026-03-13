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
                })
                .await
                .map_err(|e| TriggerError::Failed(e.to_string()))?;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}
