pub mod manual;
pub mod oneshot;
pub mod polling;

use async_trait::async_trait;
use std::path::Path;
use tokio::sync::mpsc;

use crate::types::{TriggerDef, TriggerError, TriggerEvent, WorkspaceConfig};

#[async_trait]
pub trait TriggerSource: Send + Sync {
    fn name(&self) -> &str;
    async fn subscribe(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError>;
    async fn fire_once(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError>;
}

pub fn build_trigger(
    def: &TriggerDef,
    workflow_name: &str,
    data_dir: &Path,
    scratch_base: &Path,
    workspace_config: Option<&WorkspaceConfig>,
) -> Result<Box<dyn TriggerSource>, anyhow::Error> {
    match def {
        TriggerDef::Manual => {
            let signal_path = data_dir.join("triggers").join(workflow_name);
            Ok(Box::new(manual::ManualTrigger::new(
                "manual".to_string(),
                signal_path,
            )))
        }
        TriggerDef::Polling { poll_command, context_command, interval_secs } => {
            let seen_path = data_dir.join("triggers").join(format!("{}-seen.json", workflow_name));
            Ok(Box::new(polling::PollingTrigger::new(
                "polling".to_string(),
                workflow_name.to_string(),
                poll_command.clone(),
                context_command.clone(),
                std::time::Duration::from_secs(*interval_secs),
                seen_path,
                scratch_base.to_path_buf(),
                workspace_config.cloned(),
            )))
        }
    }
}
