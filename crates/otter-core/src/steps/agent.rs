use super::StepExecutor;
use crate::agent_runner::{describe_exit_code, AgentError};
use crate::process::inherited_env;
use crate::requirements::resolve_requires;
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

        let working_dir = ctx.workspace_dir.as_deref().unwrap_or(&ctx.scratch_dir);

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
            tokio::spawn(async move {
                while let Some(chunk) = rx.recv().await {
                    f(chunk);
                }
            });
            tx
        });

        let mut env = inherited_env(&ctx.inherit_env);
        env.extend(
            resolve_requires(
                step_def.requires.as_deref().unwrap_or_default(),
                ctx.requirements.as_deref(),
                ctx.scripts_dir.as_deref(),
                ctx.secret_store.as_ref(),
                &ctx.workflow_name,
            )
            .map_err(|e| StepError::ExecutionFailed(e.to_string()))?,
        );

        let result = manager
            .run_step(
                step_def.session.as_deref(),
                &step_def.agent,
                step_def.command.as_deref(),
                message,
                working_dir,
                progress_tx,
                ctx.resource_limiter.clone(),
                ctx.scripts_dir.as_deref(),
                &env,
                ctx.sandbox_config.clone(),
            )
            .await;

        let stream_file = format!("step-{}-stream.jsonl", ctx.step_index);
        let (output, report) = match &result {
            Ok(output) => (Some(output), output.stdout.clone()),
            Err(e) => (e.output(), failure_report(e, &stream_file)),
        };
        if let Some(output) = output.filter(|o| !o.stream.is_empty()) {
            let _ = tokio::fs::write(ctx.scratch_dir.join(&stream_file), &output.stream).await;
        }
        let out_path = ctx
            .scratch_dir
            .join(format!("step-{}-output.md", ctx.step_index));
        let _ = tokio::fs::write(&out_path, report).await;

        let output = result.map_err(|e| StepError::ExecutionFailed(e.to_string()))?;
        Ok(StepOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
            accepted: None,
        })
    }
}

fn failure_report(error: &AgentError, stream_file: &str) -> String {
    let Some(output) = error.output() else {
        return format!("## Step failed\n\n{error}\n");
    };
    let headline = match error {
        AgentError::Exited { code, .. } => {
            format!("agent exited with code {}", describe_exit_code(*code))
        }
        AgentError::RateLimited { .. } => "rate limited".to_string(),
        other => other.to_string(),
    };

    let mut report = String::new();
    if !output.stdout.trim().is_empty() {
        report.push_str(output.stdout.trim_end());
        report.push_str("\n\n");
    }
    report.push_str(&format!("## Step failed\n\n{headline}\n"));
    if let Some(session_id) = &output.session_id {
        report.push_str(&format!("\nSession: `{session_id}`\n"));
    }
    if !output.stream.is_empty() {
        report.push_str(&format!("\nFull agent output: `{stream_file}`\n"));
    }
    if !output.stderr.trim().is_empty() {
        report.push_str(&format!(
            "\n### stderr\n\n```text\n{}\n```\n",
            output.stderr.trim_end()
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_runner::{
        AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec,
    };
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
                AgentSessionHandle {
                    id: "s".into(),
                    working_dir: spec.working_dir.clone(),
                    resource_limiter: Arc::new(NoOpLimiter),
                    scripts_dir: None,
                    sandbox_config: None,
                },
                AgentOutput {
                    stdout: "agent output".into(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    ..Default::default()
                },
            ))
        }

        async fn prompt(
            &self,
            _session: &AgentSessionHandle,
            _message: &str,
            _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
            _secrets: &[(String, String)],
        ) -> Result<AgentOutput, AgentError> {
            Ok(AgentOutput {
                stdout: "agent output".into(),
                stderr: String::new(),
                exit_code: Some(0),
                ..Default::default()
            })
        }

        async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
            Ok(())
        }
    }

    struct CrashingRunner;

    #[async_trait::async_trait]
    impl AgentRunner for CrashingRunner {
        async fn start(
            &self,
            _spec: AgentSpec,
            _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
        ) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
            Err(AgentError::Exited {
                code: -1073740791,
                detail: String::new(),
                output: Box::new(AgentOutput {
                    stdout: "Reading the router".into(),
                    exit_code: Some(-1073740791),
                    stream: "{\"type\":\"system\"}\n{\"type\":\"assistant\"}\n".into(),
                    session_id: Some("crashed-session".into()),
                    ..Default::default()
                }),
            })
        }

        async fn prompt(
            &self,
            _session: &AgentSessionHandle,
            _message: &str,
            _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
            _secrets: &[(String, String)],
        ) -> Result<AgentOutput, AgentError> {
            unreachable!("a crashed session is never resumed")
        }

        async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
            Ok(())
        }
    }

    fn step_context(
        scratch_dir: &std::path::Path,
        manager: crate::session::AgentSessionManager,
    ) -> StepContext {
        StepContext {
            run_id: Uuid::new_v4(),
            workflow_name: "test".into(),
            iteration: 0,
            step_index: 2,
            scratch_dir: scratch_dir.to_path_buf(),
            workspace_dir: None,
            scripts_dir: None,
            checkpoint_tx: None,
            session_manager: Some(Arc::new(manager)),
            notifier: std::sync::Arc::new(otter_notify::NoOpNotifier),
            log_fn: None,
            progress_fn: None,
            resource_limiter: Arc::new(NoOpLimiter),
            secret_store: Arc::new(otter_secrets::NoOpSecretStore),
            requirements: None,
            inherit_env: Vec::new(),
            sandbox_config: None,
        }
    }

    fn agent_step(agent: crate::types::AgentConfig, command: Option<Vec<String>>) -> StepDef {
        StepDef {
            step_type: StepType::Agent,
            command,
            message: Some("do work".into()),
            message_file: None,
            session: None,
            notify: None,
            requires: None,
            sandbox: None,
            agent,
        }
    }

    #[tokio::test]
    async fn execute_writes_output_to_scratch_dir() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let ctx = step_context(
            scratch.path(),
            crate::session::AgentSessionManager::new_with_runner_override(Arc::new(FixedRunner)),
        );
        let step_def = agent_step(
            crate::types::AgentConfig {
                provider: Some("claude".into()),
                ..Default::default()
            },
            None,
        );

        // WHEN
        let output = AgentExecutor.execute(&step_def, &ctx).await.unwrap();

        // THEN
        assert_eq!(output.stdout, "agent output");
        let written = std::fs::read_to_string(scratch.path().join("step-2-output.md")).unwrap();
        assert_eq!(written, "agent output");
    }

    #[tokio::test]
    async fn execute_writes_partial_output_when_agent_fails() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let script = crate::test_helpers::write_executable_script(
            scratch.path(),
            "failing-agent.sh",
            "#!/bin/bash\necho 'partial work'\necho 'boom' >&2\nexit 3\n",
        )
        .unwrap();
        let ctx = step_context(scratch.path(), crate::session::AgentSessionManager::new());
        let step_def = agent_step(
            Default::default(),
            Some(vec![script.to_string_lossy().to_string()]),
        );

        // WHEN
        let result = AgentExecutor.execute(&step_def, &ctx).await;

        // THEN
        assert!(result.is_err());
        let written = std::fs::read_to_string(scratch.path().join("step-2-output.md")).unwrap();
        assert!(written.contains("partial work"), "{written}");
        assert!(written.contains("boom"), "{written}");
        assert!(written.contains("exited with code 3"), "{written}");
    }

    #[tokio::test]
    async fn crashed_agent_leaves_session_id_and_stream_in_scratch_dir() {
        // GIVEN
        let scratch = tempfile::tempdir().unwrap();
        let ctx = step_context(
            scratch.path(),
            crate::session::AgentSessionManager::new_with_runner_override(Arc::new(CrashingRunner)),
        );
        let step_def = agent_step(
            crate::types::AgentConfig {
                provider: Some("claude".into()),
                ..Default::default()
            },
            None,
        );

        // WHEN
        let result = AgentExecutor.execute(&step_def, &ctx).await;

        // THEN
        assert!(result.is_err());
        let written = std::fs::read_to_string(scratch.path().join("step-2-output.md")).unwrap();
        assert!(written.contains("Reading the router"), "{written}");
        assert!(written.contains("0xC0000409"), "{written}");
        assert!(written.contains("crashed-session"), "{written}");
        assert!(written.contains("step-2-stream.jsonl"), "{written}");
        let stream = std::fs::read_to_string(scratch.path().join("step-2-stream.jsonl")).unwrap();
        assert_eq!(stream, "{\"type\":\"system\"}\n{\"type\":\"assistant\"}\n");
    }
}
