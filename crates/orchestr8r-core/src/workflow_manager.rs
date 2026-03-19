use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::bail;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::engine::Engine;
use crate::types::{
    EngineEvent, StorageBackend, WorkflowDef, WorkflowType,
    WorkflowState, WorkflowStatus,
};
use orchestr8r_notify::Notifier;

struct WorkflowHandle {
    def: WorkflowDef,
    state: Arc<Mutex<WorkflowState>>,
    /// Shared with the spawned engine so pause/resume can be signalled externally.
    paused: Arc<AtomicBool>,
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
        }
    }

    /// Register a workflow definition. The workflow starts in Dormant state.
    pub fn register(&mut self, def: WorkflowDef) {
        self.register_with_scripts_dir(def, None);
    }

    /// Register a workflow definition with an optional scripts directory.
    pub fn register_with_scripts_dir(&mut self, def: WorkflowDef, scripts_dir: Option<PathBuf>) {
        let name = def.name.clone();
        let kind = def.workflow_type.clone();
        let trigger = def.trigger.clone();
        let handle = WorkflowHandle {
            def,
            state: Arc::new(Mutex::new(WorkflowState::Dormant)),
            paused: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            task: None,
            scripts_dir,
        };
        self.handles.insert(name.clone(), handle);
        let _ = self.storage.register_workflow(&name);
        let _ = self
            .event_tx
            .try_send(EngineEvent::WorkflowRegistered { name: name.clone(), kind, trigger });
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
        let _ = self.event_tx.try_send(EngineEvent::WorkflowRemoved { name: name.to_string() });
        Ok(())
    }

    /// Reconcile the registered workflows against a freshly loaded set.
    pub fn reload(&mut self, new_workflows: Vec<(WorkflowDef, Option<PathBuf>)>) {
        let new_names: std::collections::HashSet<String> =
            new_workflows.iter().map(|(d, _)| d.name.clone()).collect();

        for (def, scripts_dir) in new_workflows {
            let name = def.name.clone();
            let is_dormant = self
                .handles
                .get(&name)
                .map(|h| *h.state.lock().unwrap() == WorkflowState::Dormant)
                .unwrap_or(false);

            if !self.handles.contains_key(&name) {
                self.register_with_scripts_dir(def, scripts_dir);
            } else if is_dormant {
                // Unregister old handle (emits WorkflowRemoved), then re-register with updated def
                let _ = self.unregister(&name);
                self.register_with_scripts_dir(def, scripts_dir);
            }
            // else: Running/Paused — leave as-is
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
            // else: Running/Paused — leave; it will disappear after finishing
        }
    }

    /// Start a dormant workflow, or resume a paused one.
    /// For looping workflows this begins the continuous loop; for triggered workflows this fires one immediate run.
    pub async fn start(&mut self, name: &str) -> anyhow::Result<()> {
        // Check state first with a shared borrow
        let current_state = {
            let handle = self
                .handles
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", name))?;
            handle.state.lock().unwrap().clone()
        };

        // If paused, just clear the flag — the existing engine task is still running
        if current_state == WorkflowState::Paused {
            let handle = self.handles.get_mut(name).unwrap();
            handle.paused.store(false, Ordering::Relaxed);
            *handle.state.lock().unwrap() = WorkflowState::Running;
            let _ = self.event_tx.try_send(EngineEvent::WorkflowStateChanged {
                name: name.to_string(),
                state: WorkflowState::Running,
            });
            return Ok(());
        }

        // Check workflow exists and is dormant, extract def early
        let (def, scripts_dir) = {
            let handle = self.handles.get_mut(name).unwrap();

            {
                let state = handle.state.lock().unwrap();
                if *state != WorkflowState::Dormant {
                    bail!(
                        "workflow '{}' is not dormant or paused (current state: {:?})",
                        name,
                        *state
                    );
                }
            }

            // Reset per-run flags.
            handle.paused.store(false, Ordering::Relaxed);
            handle.shutdown.store(false, Ordering::Relaxed);

            let def = handle.def.clone();
            let scripts_dir = handle.scripts_dir.clone();
            (def, scripts_dir)
        };

        // Now that we've released the mutable borrow, we can call methods on self
        let engine = Engine::new_with_scripts_dir(
            self.storage.clone(),
            self.data_dir.join("runs"),
            self.notifier.clone(),
            scripts_dir,
        );

        // Re-acquire mutable borrow to update handle
        let handle = self.handles.get_mut(name).unwrap();

        // Share the engine's paused flag so pause()/resume() can control it.
        let paused_flag = engine.paused_flag();
        handle.paused = paused_flag;

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

    /// Pause a running looping workflow between iterations.
    /// Returns an error for triggered workflows (pause has no defined meaning there).
    pub fn pause(&mut self, name: &str) -> anyhow::Result<()> {
        let handle = self
            .handles
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", name))?;

        if matches!(handle.def.workflow_type, WorkflowType::Triggered) {
            bail!("cannot pause triggered workflow '{}'", name);
        }

        {
            let state = handle.state.lock().unwrap();
            if *state != WorkflowState::Running {
                bail!("workflow '{}' is not running", name);
            }
        }

        handle.paused.store(true, Ordering::Relaxed);
        *handle.state.lock().unwrap() = WorkflowState::Paused;
        let _ = self.event_tx.try_send(EngineEvent::WorkflowStateChanged {
            name: name.to_string(),
            state: WorkflowState::Paused,
        });

        Ok(())
    }

    /// Stop a running or paused workflow. Awaits the engine task before returning.
    pub async fn stop(&mut self, name: &str) -> anyhow::Result<()> {
        let handle = self
            .handles
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", name))?;

        // Unpause first so the engine loop can observe the shutdown flag.
        handle.paused.store(false, Ordering::Relaxed);
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

    /// Return the current status of all registered workflows, sorted by name.
    pub fn status(&self) -> Vec<WorkflowStatus> {
        let mut statuses: Vec<WorkflowStatus> = self
            .handles
            .values()
            .map(|h| WorkflowStatus {
                name: h.def.name.clone(),
                kind: h.def.workflow_type.clone(),
                state: h.state.lock().unwrap().clone(),
                trigger: h.def.trigger.clone(),
            })
            .collect();
        statuses.sort_by(|a, b| a.name.cmp(&b.name));
        statuses
    }
}

#[cfg(test)]
#[path = "workflow_manager_tests.rs"]
mod tests;
