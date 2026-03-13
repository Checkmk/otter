pub mod manual;
pub mod oneshot;

use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;

use crate::types::{TriggerDef, TriggerError, TriggerEvent, TriggerType};

#[async_trait]
pub trait TriggerSource: Send + Sync {
    fn name(&self) -> &str;
    async fn subscribe(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError>;
}

pub fn build_trigger(
    def: &TriggerDef,
    workflow_name: &str,
    data_dir: &Path,
) -> Result<Box<dyn TriggerSource>, anyhow::Error> {
    match def.trigger_type {
        TriggerType::Manual => {
            let signal_path = data_dir.join("triggers").join(workflow_name);
            Ok(Box::new(manual::ManualTrigger::new(
                "manual".to_string(),
                signal_path,
            )))
        }
    }
}
