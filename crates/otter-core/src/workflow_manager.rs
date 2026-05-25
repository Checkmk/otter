use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::bail;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::engine::Engine;
use crate::types::{EngineEvent, StorageBackend, WorkflowDef, WorkflowState, WorkflowStatus};
use otter_notify::Notifier;
use otter_secrets::{NoOpSecretStore, SecretStore};

struct WorkflowHandle {
    def: WorkflowDef,
    toml_content: String,
    state: Arc<Mutex<WorkflowState>>,
    shutdown: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
    scripts_dir: Option<PathBuf>,
}

pub struct WorkflowManager {
    handles: HashMap<String, WorkflowHandle>,
    event_tx: mpsc::Sender<EngineEvent>,
    storage: Arc<dyn StorageBackend>,
    data_dir: PathBuf,
    notifier: Arc<dyn Notifier>,
    secret_store: Arc<dyn SecretStore>,
}

impl WorkflowManager {
    pub fn new(
        storage: Arc<dyn StorageBackend>,
        data_dir: PathBuf,
        event_tx: mpsc::Sender<EngineEvent>,
        notifier: Arc<dyn Notifier>,
    ) -> Self {
        Self {
            handles: HashMap::new(),
            event_tx,
            storage,
            data_dir,
            notifier,
            secret_store: Arc::new(NoOpSecretStore),
        }
    }

    pub fn new_with_secret_store(
        storage: Arc<dyn StorageBackend>,
        data_dir: PathBuf,
        event_tx: mpsc::Sender<EngineEvent>,
        notifier: Arc<dyn Notifier>,
        secret_store: Arc<dyn SecretStore>,
    ) -> Self {
        Self {
            handles: HashMap::new(),
            event_tx,
            storage,
            data_dir,
            notifier,
            secret_store,
        }
    }

    /// Register a workflow definition. The workflow starts in Dormant state.
    pub fn register(&mut self, def: WorkflowDef, toml_content: String) {
        self.register_with_scripts_dir(def, toml_content, None);
    }

    /// Register a workflow definition with an optional scripts directory.
    pub fn register_with_scripts_dir(
        &mut self,
        def: WorkflowDef,
        toml_content: String,
        scripts_dir: Option<PathBuf>,
    ) {
        let name = def.name.clone();
        let handle = WorkflowHandle {
            def,
            toml_content,
            state: Arc::new(Mutex::new(WorkflowState::Dormant)),
            shutdown: Arc::new(AtomicBool::new(false)),
            task: None,
            scripts_dir,
        };
        self.handles.insert(name.clone(), handle);
        let _ = self.storage.register_workflow(&name);
        let _ = self.event_tx.try_send(EngineEvent::WorkflowStateChanged {
            name,
            state: WorkflowState::Dormant,
        });
    }

    pub fn unregister(&mut self, name: &str) -> anyhow::Result<()> {
        let handle = self
            .handles
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", name))?;

        {
            let state = handle.state.lock().unwrap();
            if *state != WorkflowState::Dormant {
                bail!("cannot remove workflow '{}': it is {:?}", name, *state);
            }
        }

        let _ = self.storage.deregister_workflow(name);
        self.handles.remove(name);
        Ok(())
    }

    /// Reconcile the registered workflows against a freshly loaded set.
    pub fn reload(&mut self, new_workflows: Vec<(WorkflowDef, String, Option<PathBuf>)>) {
        let new_names: std::collections::HashSet<String> = new_workflows
            .iter()
            .map(|(d, _, _)| d.name.clone())
            .collect();

        for (def, toml_content, scripts_dir) in new_workflows {
            let name = def.name.clone();
            let is_dormant = self
                .handles
                .get(&name)
                .map(|h| *h.state.lock().unwrap() == WorkflowState::Dormant)
                .unwrap_or(false);

            if !self.handles.contains_key(&name) {
                self.register_with_scripts_dir(def, toml_content, scripts_dir);
            } else if is_dormant {
                // Replace the dormant handle in place with the updated def + toml.
                let _ = self.storage.deregister_workflow(&name);
                self.handles.remove(&name);
                self.register_with_scripts_dir(def, toml_content, scripts_dir);
            }
            // else: Running — leave as-is
        }

        // Remove dormant workflows no longer in the config dir
        let to_remove: Vec<String> = self
            .handles
            .keys()
            .filter(|n| !new_names.contains(*n))
            .cloned()
            .collect();
        for name in to_remove {
            let is_dormant = self
                .handles
                .get(&name)
                .map(|h| *h.state.lock().unwrap() == WorkflowState::Dormant)
                .unwrap_or(false);
            if is_dormant {
                let _ = self.unregister(&name);
            }
            // else: Running — leave; it will disappear after finishing
        }
    }

    /// Start a dormant workflow.
    /// For looping workflows this begins the continuous loop; for triggered workflows this fires one immediate run.
    pub async fn start(&mut self, name: &str) -> anyhow::Result<()> {
        let (def, scripts_dir) = {
            let handle = self
                .handles
                .get_mut(name)
                .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", name))?;

            {
                let state = handle.state.lock().unwrap();
                if *state != WorkflowState::Dormant {
                    bail!(
                        "workflow '{}' is not dormant (current state: {:?})",
                        name,
                        *state
                    );
                }
            }

            handle.shutdown.store(false, Ordering::Relaxed);

            let def = handle.def.clone();
            let scripts_dir = handle.scripts_dir.clone();
            (def, scripts_dir)
        };

        let requirements = def.require.clone().map(std::sync::Arc::new);
        let engine = Engine::new_with_secret_store(
            self.storage.clone(),
            self.data_dir.join("runs"),
            self.notifier.clone(),
            scripts_dir,
            self.secret_store.clone(),
        )
        .with_requirements(requirements);

        let handle = self.handles.get_mut(name).unwrap();
        let shutdown = handle.shutdown.clone();
        let event_tx = self.event_tx.clone();
        let state = handle.state.clone();

        let task = tokio::spawn(async move {
            if let Err(e) = engine.run(&def, shutdown, Some(event_tx.clone())).await {
                tracing::error!(workflow = %def.name, error = ?e, "Engine error");
            }

            *state.lock().unwrap() = WorkflowState::Dormant;
            let _ = event_tx.try_send(EngineEvent::WorkflowStateChanged {
                name: def.name.clone(),
                state: WorkflowState::Dormant,
            });
        });

        handle.task = Some(task);
        *handle.state.lock().unwrap() = WorkflowState::Running;
        let _ = self.event_tx.try_send(EngineEvent::WorkflowStateChanged {
            name: name.to_string(),
            state: WorkflowState::Running,
        });

        Ok(())
    }

    /// Stop a running workflow gracefully. Awaits the engine task before returning.
    pub async fn stop(&mut self, name: &str) -> anyhow::Result<()> {
        let handle = self
            .handles
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", name))?;

        handle.shutdown.store(true, Ordering::Relaxed);

        if let Some(task) = handle.task.take() {
            let _ = task.await;
        }

        // The task's completion handler already sets this, but be explicit.
        *handle.state.lock().unwrap() = WorkflowState::Dormant;
        let _ = self.event_tx.try_send(EngineEvent::WorkflowStateChanged {
            name: name.to_string(),
            state: WorkflowState::Dormant,
        });

        Ok(())
    }

    /// Abort the engine task for a workflow, killing any in-progress subprocess.
    /// Sets the workflow to Dormant immediately without waiting for the task to finish.
    /// No-op if the workflow is not found or has no running task.
    pub fn abort_and_stop(&mut self, name: &str) {
        let Some(handle) = self.handles.get_mut(name) else {
            return;
        };
        handle.shutdown.store(true, Ordering::Relaxed);
        if let Some(task) = handle.task.take() {
            task.abort();
        }
        *handle.state.lock().unwrap() = WorkflowState::Dormant;
        let _ = self.event_tx.try_send(EngineEvent::WorkflowStateChanged {
            name: name.to_string(),
            state: WorkflowState::Dormant,
        });
    }

    pub fn get_def(&self, name: &str) -> Option<WorkflowDef> {
        self.handles.get(name).map(|h| h.def.clone())
    }

    pub fn get_scripts_dir(&self, name: &str) -> Option<PathBuf> {
        self.handles.get(name).and_then(|h| h.scripts_dir.clone())
    }

    /// Return the current status of all registered workflows, sorted by name.
    /// `enabled` is left as `false`; callers that care (e.g. the daemon) fill it in.
    pub fn status(&self) -> Vec<WorkflowStatus> {
        let mut statuses: Vec<WorkflowStatus> = self
            .handles
            .values()
            .map(|h| WorkflowStatus {
                name: h.def.name.clone(),
                kind: h.def.workflow_type.clone(),
                state: h.state.lock().unwrap().clone(),
                trigger: h.def.trigger.clone(),
                toml_content: Some(h.toml_content.clone()),
                enabled: false,
                update_available: None,
                origin_dangling: false,
            })
            .collect();
        statuses.sort_by(|a, b| a.name.cmp(&b.name));
        statuses
    }
}

#[cfg(test)]
#[path = "workflow_manager_tests.rs"]
mod tests;
