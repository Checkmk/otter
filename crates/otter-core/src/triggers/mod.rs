pub mod manual;
pub mod oneshot;
pub mod polling;

use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

use otter_secrets::SecretStore;

use crate::types::{TriggerDef, TriggerError, TriggerEvent};

#[async_trait]
pub trait TriggerSource: Send + Sync {
    fn name(&self) -> &str;
    async fn subscribe(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError>;
    async fn fire_once(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError>;
    /// Called after a workflow run completes. `succeeded` is true if the run reached
    /// `RunStatus::Completed`; false on failure or user-initiated stop.
    async fn on_run_completed(&self, _payload: &str, _succeeded: bool) {}
}

pub fn build_trigger(
    def: &TriggerDef,
    workflow_name: &str,
    data_dir: &Path,
    _scratch_base: &Path,
    _workspace_config: Option<&crate::types::WorkspaceConfig>,
    scripts_dir: Option<&Path>,
    secret_store: Arc<dyn SecretStore>,
) -> Result<Arc<dyn TriggerSource>, anyhow::Error> {
    match def {
        TriggerDef::Manual => {
            let signal_path = data_dir.join("triggers").join(workflow_name);
            Ok(Arc::new(manual::ManualTrigger::new(
                "manual".to_string(),
                signal_path,
            )))
        }
        TriggerDef::Polling { poll_command, context_command, interval_secs, secrets } => {
            let seen_path = data_dir.join("triggers").join(format!("{}-seen.json", workflow_name));
            Ok(Arc::new(polling::PollingTrigger::new(
                "polling".to_string(),
                poll_command.clone(),
                context_command.clone(),
                std::time::Duration::from_secs(*interval_secs),
                seen_path,
                scripts_dir.map(|p| p.to_path_buf()),
                secret_store,
                secrets.clone().unwrap_or_default(),
            )))
        }
    }
}
