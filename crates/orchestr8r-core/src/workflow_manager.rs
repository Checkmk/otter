use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::bail;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::agent_runner::AgentRunner;
use crate::engine::Engine;
use crate::types::{
    EngineEvent, StorageBackend, WorkflowDef, WorkflowKind, WorkflowState, WorkflowStatus,
};
use orchestr8r_notify::Notifier;

struct WorkflowHandle {
    def: WorkflowDef,
    state: Arc<Mutex<WorkflowState>>,
    /// Shared with the spawned engine so pause/resume can be signalled externally.
    paused: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

pub struct WorkflowManager {
    handles: HashMap<String, WorkflowHandle>,
    event_tx: mpsc::Sender<EngineEvent>,
    storage: Arc<dyn StorageBackend>,
    data_dir: PathBuf,
    agent_runner: Arc<dyn AgentRunner>,
    notifier: Arc<dyn Notifier>,
}

impl WorkflowManager {
    pub fn new(
        storage: Arc<dyn StorageBackend>,
        data_dir: PathBuf,
        event_tx: mpsc::Sender<EngineEvent>,
        agent_runner: Arc<dyn AgentRunner>,
        notifier: Arc<dyn Notifier>,
    ) -> Self {
        Self {
            handles: HashMap::new(),
            event_tx,
            storage,
            data_dir,
            agent_runner,
            notifier,
        }
    }

    /// Register a workflow definition. The workflow starts in Dormant state.
    pub fn register(&mut self, def: WorkflowDef) {
        let name = def.name.clone();
        let kind = def.kind.clone();
        let handle = WorkflowHandle {
            def,
            state: Arc::new(Mutex::new(WorkflowState::Dormant)),
            paused: Arc::new(AtomicBool::new(false)),
            shutdown: Arc::new(AtomicBool::new(false)),
            task: None,
        };
        self.handles.insert(name.clone(), handle);
        let _ = self
            .event_tx
            .try_send(EngineEvent::WorkflowRegistered { name: name.clone(), kind });
        let _ = self.event_tx.try_send(EngineEvent::WorkflowStateChanged {
            name,
            state: WorkflowState::Dormant,
        });
    }

    /// Start a dormant workflow. For indefinite workflows this begins the continuous loop;
    /// for triggered workflows this fires one immediate run.
    pub async fn start(&mut self, name: &str) -> anyhow::Result<()> {
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

        // Reset per-run flags.
        handle.paused.store(false, Ordering::Relaxed);
        handle.shutdown.store(false, Ordering::Relaxed);

        let engine = Engine::new(
            self.storage.clone(),
            self.data_dir.join("runs"),
            self.agent_runner.clone(),
            self.notifier.clone(),
        );

        // Share the engine's paused flag so pause()/resume() can control it.
        let paused_flag = engine.paused_flag();
        handle.paused = paused_flag;

        let def = handle.def.clone();
        let shutdown = handle.shutdown.clone();
        let event_tx = self.event_tx.clone();
        let state = handle.state.clone();
        let is_triggered = matches!(def.kind, WorkflowKind::Triggered);

        let task = tokio::spawn(async move {
            let result = if is_triggered {
                // "start" for a triggered workflow = fire one immediate run.
                engine.run_once(&def, shutdown, Some(event_tx.clone())).await
            } else {
                engine.run(&def, shutdown, Some(event_tx.clone())).await
            };

            if let Err(e) = result {
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

    /// Pause a running indefinite workflow between iterations.
    /// Returns an error for triggered workflows (pause has no defined meaning there).
    pub fn pause(&mut self, name: &str) -> anyhow::Result<()> {
        let handle = self
            .handles
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", name))?;

        if matches!(handle.def.kind, WorkflowKind::Triggered) {
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

    /// Resume a paused indefinite workflow.
    pub fn resume(&mut self, name: &str) -> anyhow::Result<()> {
        let handle = self
            .handles
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("workflow '{}' not found", name))?;

        {
            let state = handle.state.lock().unwrap();
            if *state != WorkflowState::Paused {
                bail!("workflow '{}' is not paused", name);
            }
        }

        handle.paused.store(false, Ordering::Relaxed);
        *handle.state.lock().unwrap() = WorkflowState::Running;
        let _ = self.event_tx.try_send(EngineEvent::WorkflowStateChanged {
            name: name.to_string(),
            state: WorkflowState::Running,
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
                kind: h.def.kind.clone(),
                state: h.state.lock().unwrap().clone(),
            })
            .collect();
        statuses.sort_by(|a, b| a.name.cmp(&b.name));
        statuses
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runner::{AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec};
    use crate::storage::InMemoryStorage;
    use crate::types::{StepDef, StepType, WorkflowKind};
    use orchestr8r_notify::NoOpNotifier;

    struct NoOpAgentRunner;

    #[async_trait::async_trait]
    impl AgentRunner for NoOpAgentRunner {
        async fn start(
            &self,
            spec: AgentSpec,
        ) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
            Ok((
                AgentSessionHandle {
                    id: "noop".to_string(),
                    command: spec.command,
                    working_dir: spec.working_dir,
                },
                AgentOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: Some(0),
                },
            ))
        }
        async fn prompt(
            &self,
            _session: &AgentSessionHandle,
            _message: &str,
        ) -> Result<AgentOutput, AgentError> {
            Ok(AgentOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        }
        async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
            Ok(())
        }
    }

    fn make_manager(event_tx: mpsc::Sender<EngineEvent>) -> WorkflowManager {
        let storage = Arc::new(InMemoryStorage::new());
        let data_dir = std::env::temp_dir().join("orchestr8r-wm-tests");
        WorkflowManager::new(
            storage,
            data_dir,
            event_tx,
            Arc::new(NoOpAgentRunner),
            Arc::new(NoOpNotifier),
        )
    }

    fn indefinite_workflow(name: &str) -> WorkflowDef {
        WorkflowDef {
            name: name.to_string(),
            kind: WorkflowKind::Indefinite,
            trigger: None,
            steps: vec![StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["true".to_string()]),
                message: None,
                path: None,
                output_file: None,
                session: None,
                notify: None,
            }],
        }
    }

    fn triggered_workflow(name: &str) -> WorkflowDef {
        WorkflowDef {
            name: name.to_string(),
            kind: WorkflowKind::Triggered,
            trigger: None,
            steps: vec![StepDef {
                step_type: StepType::Shell,
                command: Some(vec!["true".to_string()]),
                message: None,
                path: None,
                output_file: None,
                session: None,
                notify: None,
            }],
        }
    }

    #[test]
    fn register_makes_workflow_dormant() {
        // GIVEN
        let (tx, _rx) = mpsc::channel(32);
        let mut manager = make_manager(tx);

        // WHEN
        manager.register(indefinite_workflow("hello"));

        // THEN
        let status = manager.status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].name, "hello");
        assert_eq!(status[0].state, WorkflowState::Dormant);
    }

    #[test]
    fn register_emits_registered_and_state_changed_events() {
        // GIVEN
        let (tx, mut rx) = mpsc::channel(32);
        let mut manager = make_manager(tx);

        // WHEN
        manager.register(indefinite_workflow("hello"));

        // THEN
        let ev1 = rx.try_recv().expect("WorkflowRegistered");
        assert!(matches!(ev1, EngineEvent::WorkflowRegistered { ref name, .. } if name == "hello"));
        let ev2 = rx.try_recv().expect("WorkflowStateChanged");
        assert!(
            matches!(ev2, EngineEvent::WorkflowStateChanged { ref name, state: WorkflowState::Dormant } if name == "hello")
        );
    }

    #[tokio::test]
    async fn start_transitions_to_running_and_stop_returns_to_dormant() {
        // GIVEN
        let (tx, _rx) = mpsc::channel(64);
        let mut manager = make_manager(tx);
        manager.register(indefinite_workflow("hello"));

        // WHEN
        manager.start("hello").await.unwrap();

        // THEN
        assert_eq!(manager.status()[0].state, WorkflowState::Running);

        // WHEN
        manager.stop("hello").await.unwrap();

        // THEN
        assert_eq!(manager.status()[0].state, WorkflowState::Dormant);
    }

    #[tokio::test]
    async fn pause_and_resume_lifecycle() {
        // GIVEN
        let (tx, _rx) = mpsc::channel(64);
        let mut manager = make_manager(tx);
        manager.register(indefinite_workflow("hello"));
        manager.start("hello").await.unwrap();

        // WHEN
        manager.pause("hello").unwrap();

        // THEN
        assert_eq!(manager.status()[0].state, WorkflowState::Paused);

        // WHEN
        manager.resume("hello").unwrap();

        // THEN
        assert_eq!(manager.status()[0].state, WorkflowState::Running);

        // cleanup
        manager.stop("hello").await.unwrap();
    }

    #[tokio::test]
    async fn pause_rejected_for_triggered_workflow() {
        // GIVEN
        let (tx, _rx) = mpsc::channel(64);
        let mut manager = make_manager(tx);
        manager.register(triggered_workflow("on-demand"));
        manager.start("on-demand").await.unwrap();

        // WHEN / THEN
        assert!(manager.pause("on-demand").is_err());

        // cleanup — wait for one-shot run to finish
        manager.stop("on-demand").await.unwrap();
    }

    #[tokio::test]
    async fn start_fails_if_already_running() {
        // GIVEN
        let (tx, _rx) = mpsc::channel(64);
        let mut manager = make_manager(tx);
        manager.register(indefinite_workflow("hello"));
        manager.start("hello").await.unwrap();

        // WHEN / THEN
        assert!(manager.start("hello").await.is_err());

        manager.stop("hello").await.unwrap();
    }

    #[tokio::test]
    async fn stop_unknown_workflow_returns_error() {
        // GIVEN
        let (tx, _rx) = mpsc::channel(32);
        let mut manager = make_manager(tx);

        // WHEN / THEN
        assert!(manager.stop("nope").await.is_err());
    }

    #[tokio::test]
    async fn triggered_workflow_completes_and_returns_to_dormant() {
        // GIVEN
        let (tx, _rx) = mpsc::channel(64);
        let mut manager = make_manager(tx);
        manager.register(triggered_workflow("job"));

        // WHEN
        manager.start("job").await.unwrap();
        // Give the task time to complete the single run.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // THEN
        assert_eq!(manager.status()[0].state, WorkflowState::Dormant);
    }

    #[tokio::test]
    async fn status_reports_all_workflows_sorted() {
        // GIVEN
        let (tx, _rx) = mpsc::channel(64);
        let mut manager = make_manager(tx);
        manager.register(indefinite_workflow("beta"));
        manager.register(indefinite_workflow("alpha"));

        // WHEN
        let statuses = manager.status();

        // THEN
        assert_eq!(statuses[0].name, "alpha");
        assert_eq!(statuses[1].name, "beta");

        manager.stop("alpha").await.unwrap();
        manager.stop("beta").await.unwrap();
    }

    #[tokio::test]
    async fn paused_engine_loop_actually_pauses() {
        // GIVEN — workflow with a shell step that increments a counter
        let (tx, _rx) = mpsc::channel(64);
        let storage = Arc::new(InMemoryStorage::new());
        let data_dir = std::env::temp_dir().join("orchestr8r-pause-test");
        let mut manager = WorkflowManager::new(
            storage.clone(),
            data_dir,
            tx,
            Arc::new(NoOpAgentRunner),
            Arc::new(NoOpNotifier),
        );
        manager.register(indefinite_workflow("counter"));
        manager.start("counter").await.unwrap();

        // Let it run for at least one iteration.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let runs_before_pause = storage.runs().len();

        // WHEN — pause
        manager.pause("counter").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let runs_after_pause = storage.runs().len();

        // THEN — no additional runs created while paused (at most 1 in-flight completes)
        assert!(
            runs_after_pause <= runs_before_pause + 1,
            "engine should not advance while paused"
        );

        // cleanup
        manager.stop("counter").await.unwrap();
    }
}
