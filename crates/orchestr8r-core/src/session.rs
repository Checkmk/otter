use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::warn;
use uuid::Uuid;

use crate::agent_runner::{AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec};

pub struct AgentSessionManager {
    runner: Arc<dyn AgentRunner>,
    sessions: Mutex<HashMap<String, AgentSessionHandle>>,
    last_key: Mutex<Option<String>>,
}

impl AgentSessionManager {
    pub fn new(runner: Arc<dyn AgentRunner>) -> Self {
        Self {
            runner,
            sessions: Mutex::new(HashMap::new()),
            last_key: Mutex::new(None),
        }
    }

    /// Start or resume a session and run a prompt. Named sessions are keyed by
    /// `session_name`; anonymous steps get a fresh uuid key so they are never resumed.
    pub async fn run_step(
        &self,
        session_name: Option<&str>,
        command: Option<&[String]>,
        message: &str,
        working_dir: &Path,
    ) -> Result<AgentOutput, AgentError> {
        let session_key = session_name
            .map(str::to_string)
            .unwrap_or_else(|| format!("__anon_{}", Uuid::new_v4()));

        let existing = self
            .sessions
            .lock()
            .unwrap()
            .get(&session_key)
            .cloned();

        let output = if let Some(session) = existing {
            self.runner.prompt(&session, message).await?
        } else {
            let command = command
                .ok_or_else(|| {
                    AgentError::Failed(
                        "agent step missing command (required for new session)".to_string(),
                    )
                })?
                .to_vec();

            let spec = AgentSpec {
                command,
                message: message.to_string(),
                working_dir: working_dir.to_path_buf(),
            };

            let (handle, output) = self.runner.start(spec).await?;
            self.sessions
                .lock()
                .unwrap()
                .insert(session_key.clone(), handle);
            output
        };

        *self.last_key.lock().unwrap() = Some(session_key);
        Ok(output)
    }

    /// Prompt the most recently used session. Returns `None` if no session is active.
    pub async fn prompt_last(&self, message: &str) -> Result<Option<AgentOutput>, AgentError> {
        let key = self.last_key.lock().unwrap().clone();
        let session = key.and_then(|k| self.sessions.lock().unwrap().get(&k).cloned());
        match session {
            None => Ok(None),
            Some(s) => Ok(Some(self.runner.prompt(&s, message).await?)),
        }
    }

    pub fn has_active_session(&self) -> bool {
        let key = self.last_key.lock().unwrap().clone();
        key.map_or(false, |k| self.sessions.lock().unwrap().contains_key(&k))
    }

    pub async fn cleanup(&self) {
        let sessions: Vec<AgentSessionHandle> =
            self.sessions.lock().unwrap().values().cloned().collect();
        for session in sessions {
            if let Err(e) = self.runner.stop(&session).await {
                warn!(error = %e, "Failed to stop agent session");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runner::{AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec};
    use std::sync::Mutex;

    struct MockRunner {
        calls: Mutex<Vec<String>>,
    }

    impl MockRunner {
        fn new() -> Arc<Self> {
            Arc::new(Self { calls: Mutex::new(Vec::new()) })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl AgentRunner for MockRunner {
        async fn start(&self, spec: AgentSpec) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
            self.calls.lock().unwrap().push(format!("start:{}", spec.message));
            Ok((
                AgentSessionHandle { id: "s1".into(), command: spec.command, working_dir: spec.working_dir },
                AgentOutput { stdout: format!("resp:{}", spec.message), stderr: String::new(), exit_code: Some(0) },
            ))
        }

        async fn prompt(&self, _session: &AgentSessionHandle, message: &str) -> Result<AgentOutput, AgentError> {
            self.calls.lock().unwrap().push(format!("prompt:{}", message));
            Ok(AgentOutput { stdout: format!("resp:{}", message), stderr: String::new(), exit_code: Some(0) })
        }

        async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
            self.calls.lock().unwrap().push("stop".into());
            Ok(())
        }
    }

    #[tokio::test]
    async fn named_session_resumes_on_second_call() {
        // GIVEN
        let runner = MockRunner::new();
        let manager = AgentSessionManager::new(runner.clone());
        let dir = std::path::PathBuf::from("/tmp");
        let cmd = vec!["agent".to_string()];

        // WHEN
        manager.run_step(Some("planner"), Some(&cmd), "first", &dir).await.unwrap();
        manager.run_step(Some("planner"), None, "second", &dir).await.unwrap();

        // THEN — start once, prompt once
        let calls = runner.calls();
        assert_eq!(calls[0], "start:first");
        assert_eq!(calls[1], "prompt:second");
        assert_eq!(calls.iter().filter(|c| c.starts_with("start:")).count(), 1);
    }

    #[tokio::test]
    async fn prompt_last_returns_none_when_no_session() {
        // GIVEN
        let runner = MockRunner::new();
        let manager = AgentSessionManager::new(runner.clone());

        // WHEN
        let result = manager.prompt_last("hello").await.unwrap();

        // THEN
        assert!(result.is_none());
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn has_active_session_reflects_state() {
        // GIVEN
        let runner = MockRunner::new();
        let manager = AgentSessionManager::new(runner.clone());
        let dir = std::path::PathBuf::from("/tmp");
        let cmd = vec!["agent".to_string()];

        // WHEN / THEN
        assert!(!manager.has_active_session());
        manager.run_step(Some("s"), Some(&cmd), "hi", &dir).await.unwrap();
        assert!(manager.has_active_session());
    }
}
