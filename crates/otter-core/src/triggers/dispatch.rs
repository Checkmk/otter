use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::types::{TriggerError, TriggerEvent};

use super::TriggerSource;

/// A run handed to a `dispatch`-triggered workflow by `otter dispatch`.
#[derive(Debug, Clone)]
pub struct DispatchMsg {
    pub payload: String,
    /// `(filename, contents)` pairs written into the run's `trigger-context/`.
    pub context_files: Vec<(String, String)>,
}

/// Maps a running `dispatch`-triggered workflow name to the sender of its inbox.
/// Owned by the workflow manager; the daemon pushes into it on `DaemonCommand::Dispatch`,
/// and each `DispatchTrigger` registers its sender here when its engine starts.
pub type DispatchRegistry = Arc<Mutex<HashMap<String, mpsc::Sender<DispatchMsg>>>>;

/// Trigger that only fires when another workflow dispatches a run to it.
pub struct DispatchTrigger {
    name: String,
    inbox: Mutex<Option<mpsc::Receiver<DispatchMsg>>>,
}

impl DispatchTrigger {
    /// Create the trigger and register its inbox sender under `workflow` in `registry`,
    /// replacing any previous registration (e.g. from an earlier engine start).
    pub fn new(name: impl Into<String>, workflow: impl Into<String>, registry: DispatchRegistry) -> Self {
        let (tx, rx) = mpsc::channel::<DispatchMsg>(32);
        // Recover from a poisoned lock rather than panicking the engine task: the
        // registry is a plain name→sender map, so a panic elsewhere can't leave it
        // in a state that makes inserting unsafe.
        registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(workflow.into(), tx);
        Self {
            name: name.into(),
            inbox: Mutex::new(Some(rx)),
        }
    }
}

#[async_trait]
impl TriggerSource for DispatchTrigger {
    fn name(&self) -> &str {
        &self.name
    }

    async fn subscribe(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError> {
        let mut rx = self
            .inbox
            .lock()
            .map_err(|_| TriggerError::Failed("dispatch inbox poisoned".to_string()))?
            .take()
            .ok_or_else(|| TriggerError::Failed("dispatch inbox already consumed".to_string()))?;

        while let Some(msg) = rx.recv().await {
            let event = TriggerEvent {
                source: self.name.clone(),
                payload: msg.payload,
                preallocated_run_id: None,
                pending_context: None,
                inline_context: Some(msg.context_files),
            };
            if tx.send(event).await.is_err() {
                break;
            }
        }
        Ok(())
    }

    /// A dispatch trigger never fires on its own; runs only arrive via the inbox.
    async fn fire_once(&self, _tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dispatched_message_becomes_trigger_event() {
        // GIVEN a dispatch trigger registered in a registry
        let registry: DispatchRegistry = Arc::new(Mutex::new(HashMap::new()));
        let trigger = DispatchTrigger::new("dispatch", "wf", registry.clone());
        let (event_tx, mut event_rx) = mpsc::channel::<TriggerEvent>(8);
        let handle = tokio::spawn(async move { trigger.subscribe(event_tx).await });

        // WHEN a message is pushed to the workflow's inbox
        let sender = registry.lock().unwrap().get("wf").unwrap().clone();
        sender
            .send(DispatchMsg {
                payload: "ch-42".to_string(),
                context_files: vec![("summary.txt".to_string(), "hello".to_string())],
            })
            .await
            .unwrap();

        // THEN subscribe emits a matching trigger event with inline context
        let event = event_rx.recv().await.expect("expected one event");
        assert_eq!(event.payload, "ch-42");
        assert_eq!(
            event.inline_context.as_deref(),
            Some(&[("summary.txt".to_string(), "hello".to_string())][..])
        );

        handle.abort();
    }
}
