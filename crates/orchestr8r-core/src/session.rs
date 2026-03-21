use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tracing::warn;
use uuid::Uuid;

use crate::agent_runner::{
    AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec, CustomRunner,
    build_runner,
};
use crate::resource_limiter::ResourceLimiter;
use crate::types::{AgentConfig, ProgressChunk};
use tokio::sync::mpsc;

struct SessionEntry {
    handle: AgentSessionHandle,
    runner: Arc<dyn AgentRunner>,
}

pub struct AgentSessionManager {
    sessions: Mutex<HashMap<String, SessionEntry>>,
    last_key: Mutex<Option<String>>,
    #[cfg(test)]
    runner_override: Option<Arc<dyn AgentRunner>>,
}

impl AgentSessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            last_key: Mutex::new(None),
            #[cfg(test)]
            runner_override: None,
        }
    }

    #[cfg(test)]
    pub fn new_with_runner_override(runner: Arc<dyn AgentRunner>) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            last_key: Mutex::new(None),
            runner_override: Some(runner),
        }
    }

    /// Start or resume a session and run a prompt.
    ///
    /// - `session_name`: key for named sessions (resumed across steps); `None` creates an anonymous session.
    /// - `config`: provider and tool config for new sessions; ignored when resuming an existing one.
    /// - `command`: escape-hatch command array (`CustomRunner`); mutually exclusive with `config.provider`.
    ///
    /// For existing sessions `config` and `command` are ignored.
    pub async fn run_step(
        &self,
        session_name: Option<&str>,
        config: &AgentConfig,
        command: Option<&[String]>,
        message: &str,
        working_dir: &Path,
        progress_tx: Option<mpsc::Sender<ProgressChunk>>,
        resource_limiter: Arc<dyn ResourceLimiter>,
        scripts_dir: Option<&Path>,
        secrets: &[(String, String)],
    ) -> Result<AgentOutput, AgentError> {
        let session_key = session_name
            .map(str::to_string)
            .unwrap_or_else(|| format!("__anon_{}", Uuid::new_v4()));

        let existing = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(&session_key)
                .map(|e| (e.handle.clone(), e.runner.clone()))
        };

        let output = if let Some((handle, runner)) = existing {
            runner.prompt(&handle, message, progress_tx, secrets).await?
        } else {
            let runner = self.resolve_runner(config, command)?;

            let spec = AgentSpec {
                message: message.to_string(),
                working_dir: working_dir.to_path_buf(),
                resource_limiter,
                scripts_dir: scripts_dir.map(|p| p.to_path_buf()),
                secrets: secrets.to_vec(),
            };

            let (handle, output) = runner.start(spec, progress_tx).await?;
            self.sessions
                .lock()
                .unwrap()
                .insert(session_key.clone(), SessionEntry { handle, runner });
            output
        };

        *self.last_key.lock().unwrap() = Some(session_key);
        Ok(output)
    }

    fn resolve_runner(
        &self,
        config: &AgentConfig,
        command: Option<&[String]>,
    ) -> Result<Arc<dyn AgentRunner>, AgentError> {
        #[cfg(test)]
        if let Some(ref r) = self.runner_override {
            return Ok(r.clone());
        }

        match (config.provider.as_deref(), command) {
            (Some(_), Some(_)) => Err(AgentError::Failed(
                "agent step must specify either provider or command, not both".to_string(),
            )),
            (Some(p), None) => build_runner(
                p,
                config.allowed_tools.as_deref(),
                config.permission_mode.as_deref(),
            ),
            (None, Some(cmd)) => Ok(Arc::new(CustomRunner::new(cmd.to_vec()))),
            (None, None) => Err(AgentError::Failed(
                "agent step must specify provider or command for new session".to_string(),
            )),
        }
    }

    /// Prompt the most recently used session. Returns `None` if no session is active.
    pub async fn prompt_last(
        &self,
        message: &str,
        progress_tx: Option<mpsc::Sender<ProgressChunk>>,
        secrets: &[(String, String)],
    ) -> Result<Option<AgentOutput>, AgentError> {
        let key = self.last_key.lock().unwrap().clone();
        let entry = key.and_then(|k| {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .get(&k)
                .map(|e| (e.handle.clone(), e.runner.clone()))
        });
        match entry {
            None => Ok(None),
            Some((handle, runner)) => Ok(Some(runner.prompt(&handle, message, progress_tx, secrets).await?)),
        }
    }

    pub fn has_active_session(&self) -> bool {
        let key = self.last_key.lock().unwrap().clone();
        key.map_or(false, |k| self.sessions.lock().unwrap().contains_key(&k))
    }

    pub async fn cleanup(&self) {
        let entries: Vec<(AgentSessionHandle, Arc<dyn AgentRunner>)> = {
            let sessions = self.sessions.lock().unwrap();
            sessions
                .values()
                .map(|e| (e.handle.clone(), e.runner.clone()))
                .collect()
        };
        for (handle, runner) in entries {
            if let Err(e) = runner.stop(&handle).await {
                warn!(error = %e, "Failed to stop agent session");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runner::{AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec};
    use crate::resource_limiter::NoOpLimiter;
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
        async fn start(
            &self,
            spec: AgentSpec,
            _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
        ) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
            self.calls.lock().unwrap().push(format!("start:{}", spec.message));
            Ok((
                AgentSessionHandle { id: "s1".into(), working_dir: spec.working_dir, resource_limiter: Arc::new(NoOpLimiter), scripts_dir: None },
                AgentOutput { stdout: format!("resp:{}", spec.message), stderr: String::new(), exit_code: Some(0) },
            ))
        }

        async fn prompt(
            &self,
            _session: &AgentSessionHandle,
            message: &str,
            _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
            _secrets: &[(String, String)],
        ) -> Result<AgentOutput, AgentError> {
            self.calls.lock().unwrap().push(format!("prompt:{}", message));
            Ok(AgentOutput { stdout: format!("resp:{}", message), stderr: String::new(), exit_code: Some(0) })
        }

        async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
            self.calls.lock().unwrap().push("stop".into());
            Ok(())
        }
    }

    fn manager_with_mock(runner: Arc<dyn AgentRunner>) -> AgentSessionManager {
        AgentSessionManager {
            sessions: Mutex::new(HashMap::new()),
            last_key: Mutex::new(None),
            runner_override: Some(runner),
        }
    }

    fn claude() -> AgentConfig {
        AgentConfig { provider: Some("claude".into()), ..Default::default() }
    }

    #[tokio::test]
    async fn named_session_resumes_on_second_call() {
        // GIVEN
        let runner = MockRunner::new();
        let manager = manager_with_mock(runner.clone());
        let dir = std::path::PathBuf::from("/tmp");

        // WHEN
        manager.run_step(Some("planner"), &claude(), None, "first", &dir, None, Arc::new(NoOpLimiter), None, &[]).await.unwrap();
        manager.run_step(Some("planner"), &AgentConfig::default(), None, "second", &dir, None, Arc::new(NoOpLimiter), None, &[]).await.unwrap();

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
        let manager = manager_with_mock(runner.clone());

        // WHEN
        let result = manager.prompt_last("hello", None, &[]).await.unwrap();

        // THEN
        assert!(result.is_none());
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn has_active_session_reflects_state() {
        // GIVEN
        let runner = MockRunner::new();
        let manager = manager_with_mock(runner.clone());
        let dir = std::path::PathBuf::from("/tmp");

        // WHEN / THEN
        assert!(!manager.has_active_session());
        manager.run_step(Some("s"), &claude(), None, "hi", &dir, None, Arc::new(NoOpLimiter), None, &[]).await.unwrap();
        assert!(manager.has_active_session());
    }

    #[tokio::test]
    async fn new_session_requires_provider_or_command() {
        // GIVEN
        let manager = AgentSessionManager::new();
        let dir = std::path::PathBuf::from("/tmp");

        // WHEN / THEN — neither provided
        let err = manager.run_step(None, &AgentConfig::default(), None, "hi", &dir, None, Arc::new(NoOpLimiter), None, &[]).await.unwrap_err();
        assert!(err.to_string().contains("provider or command"));
    }

    #[tokio::test]
    async fn new_session_rejects_both_provider_and_command() {
        // GIVEN
        let manager = AgentSessionManager::new();
        let dir = std::path::PathBuf::from("/tmp");
        let cmd = vec!["echo".to_string()];

        // WHEN / THEN
        let err = manager
            .run_step(None, &claude(), Some(&cmd), "hi", &dir, None, Arc::new(NoOpLimiter), None, &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not both"));
    }

    #[tokio::test]
    async fn anonymous_sessions_are_single_use() {
        // GIVEN two anonymous (unnamed) sessions using a mock runner
        let runner = MockRunner::new();
        let manager = manager_with_mock(runner.clone());
        let dir = std::path::PathBuf::from("/tmp");

        // WHEN
        manager.run_step(None, &claude(), None, "task one", &dir, None, Arc::new(NoOpLimiter), None, &[]).await.unwrap();
        manager.run_step(None, &claude(), None, "task two", &dir, None, Arc::new(NoOpLimiter), None, &[]).await.unwrap();

        // THEN — two separate start calls (each anonymous session is independent)
        let calls = runner.calls();
        let starts = calls.iter().filter(|c| c.starts_with("start:")).count();
        assert_eq!(starts, 2, "each anonymous session should start independently");
    }

    #[tokio::test]
    async fn sessions_cleaned_up_at_end() {
        // GIVEN a named session
        let runner = MockRunner::new();
        let manager = manager_with_mock(runner.clone());
        let dir = std::path::PathBuf::from("/tmp");

        // WHEN
        manager.run_step(Some("worker"), &claude(), None, "do work", &dir, None, Arc::new(NoOpLimiter), None, &[]).await.unwrap();
        manager.cleanup().await;

        // THEN
        let calls = runner.calls();
        assert!(calls.iter().any(|c| c == "stop"), "cleanup should call stop");
    }
}
