use super::StepExecutor;
use crate::types::{ProgressChunk, StepContext, StepDef, StepError, StepOutput};
use async_trait::async_trait;
use tokio::sync::mpsc;

pub struct AgentExecutor;

#[async_trait]
impl StepExecutor for AgentExecutor {
    fn step_type(&self) -> crate::types::StepType {
        crate::types::StepType::Agent
    }

    async fn execute(
        &self,
        step_def: &StepDef,
        ctx: &StepContext,
    ) -> Result<StepOutput, StepError> {
        let manager = ctx
            .session_manager
            .as_ref()
            .ok_or_else(|| StepError::ExecutionFailed("no session manager in context".into()))?;

        let message = step_def
            .message
            .as_deref()
            .ok_or_else(|| StepError::ExecutionFailed("agent step missing message".into()))?;

        let working_dir = ctx
            .workspace_dir
            .as_deref()
            .unwrap_or(&ctx.scratch_dir);

        if let Some(ref log_fn) = ctx.log_fn {
            let provider = step_def.agent.provider.as_deref().unwrap_or("custom");
            log_fn(crate::types::LogEntry {
                run_id: ctx.run_id,
                iteration: ctx.iteration,
                step_index: ctx.step_index,
                step_type: "agent".to_string(),
                stdout: format!("Running {provider} agent..."),
                stderr: String::new(),
                exit_code: None,
                accepted: None,
                feedback: None,
                timestamp: chrono::Utc::now(),
            });
        }

        let progress_tx: Option<mpsc::Sender<ProgressChunk>> = ctx.progress_fn.as_ref().map(|f| {
            let (tx, mut rx) = mpsc::channel::<ProgressChunk>(64);
            let f = f.clone();
            tokio::spawn(async move { while let Some(chunk) = rx.recv().await { f(chunk); } });
            tx
        });

        let output = manager
            .run_step(
                step_def.session.as_deref(),
                &step_def.agent,
                step_def.command.as_deref(),
                message,
                working_dir,
                progress_tx,
                ctx.resource_limiter.clone(),
            )
            .await
            .map_err(|e| StepError::ExecutionFailed(e.to_string()))?;

        let out_path = ctx
            .scratch_dir
            .join(format!("step-{}-output.md", ctx.step_index));
        let _ = tokio::fs::write(&out_path, &output.stdout).await;

        Ok(StepOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
            accepted: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runner::{AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec};
    use crate::resource_limiter::NoOpLimiter;
    use crate::types::{ProgressChunk, StepType};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    struct FixedRunner;

    #[async_trait::async_trait]
    impl AgentRunner for FixedRunner {
        async fn start(
            &self,
            spec: AgentSpec,
            _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
        ) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
            Ok((
                AgentSessionHandle { id: "s".into(), working_dir: spec.working_dir.clone(), resource_limiter: Arc::new(NoOpLimiter) },
                AgentOutput { stdout: "agent output".into(), stderr: String::new(), exit_code: Some(0) },
            ))
        }

        async fn prompt(
            &self,
            _session: &AgentSessionHandle,
            _message: &str,
            _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
        ) -> Result<AgentOutput, AgentError> {
            Ok(AgentOutput { stdout: "agent output".into(), stderr: String::new(), exit_code: Some(0) })
        }

        async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn execute_writes_output_to_scratch_dir() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let manager = Arc::new(crate::session::AgentSessionManager::new_with_runner_override(Arc::new(FixedRunner)));
        let ctx = StepContext {
            run_id: Uuid::new_v4(),
            workflow_name: "test".into(),
            iteration: 0,
            step_index: 2,
            scratch_dir: scratch.path().to_path_buf(),
            workspace_dir: None,
            checkpoint_tx: None,
            session_manager: Some(manager),
            notifier: std::sync::Arc::new(orchestr8r_notify::NoOpNotifier),
            log_fn: None,
            progress_fn: None,
            resource_limiter: Arc::new(NoOpLimiter),
        };
        let step_def = StepDef {
            step_type: StepType::Agent,
            command: None,
            message: Some("do work".into()),
            session: None,
            notify: None,
            agent: crate::types::AgentConfig { provider: Some("claude".into()), ..Default::default() },
        };

        // WHEN
        let output = AgentExecutor.execute(&step_def, &ctx).await.unwrap();

        // THEN
        assert_eq!(output.stdout, "agent output");
        let written = std::fs::read_to_string(scratch.path().join("step-2-output.md")).unwrap();
        assert_eq!(written, "agent output");
    }
}
