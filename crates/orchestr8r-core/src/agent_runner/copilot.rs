use async_trait::async_trait;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{
    AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec,
    classify_agent_error, run_subprocess_no_stdin,
};
use crate::types::ProgressChunk;

/// Agent runner targeting the GitHub Copilot CLI.
///
/// - `start()` pre-generates a UUID and runs `copilot [base_args] --resume=<uuid> -p <message>`.
///   Copilot treats `--resume=<unknown-uuid>` as "start new session with this UUID".
/// - `prompt()` runs `copilot [base_args] --resume=<uuid> -p <message>` to resume.
/// - `stop()` is a no-op.
pub struct CopilotRunner {
    base_args: Vec<String>,
}

impl CopilotRunner {
    pub fn new(allowed_tools: Option<Vec<String>>) -> Self {
        let mut base_args = Vec::new();
        if let Some(tools) = allowed_tools {
            for tool in tools {
                base_args.push(format!("--allow-tool={tool}"));
            }
        }
        Self { base_args }
    }
}

#[async_trait]
impl AgentRunner for CopilotRunner {
    async fn start(
        &self,
        spec: AgentSpec,
        _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
    ) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
        let session_id = Uuid::new_v4().to_string();

        let handle = AgentSessionHandle {
            id: session_id.clone(),
            working_dir: spec.working_dir.clone(),
        };

        let mut cmd_args = vec!["copilot".to_string()];
        cmd_args.extend(self.base_args.clone());
        cmd_args.push(format!("--resume={session_id}"));
        cmd_args.push("-p".to_string());
        cmd_args.push(spec.message.clone());

        let output = run_subprocess_no_stdin(&cmd_args, &spec.working_dir).await?;

        if let Some(code) = output.exit_code {
            if code != 0 {
                return Err(classify_agent_error(code, &output));
            }
        }

        Ok((handle, output))
    }

    async fn prompt(
        &self,
        session: &AgentSessionHandle,
        message: &str,
        _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
    ) -> Result<AgentOutput, AgentError> {
        let mut cmd_args = vec!["copilot".to_string()];
        cmd_args.extend(self.base_args.clone());
        cmd_args.push(format!("--resume={}", session.id));
        cmd_args.push("-p".to_string());
        cmd_args.push(message.to_string());

        let output = run_subprocess_no_stdin(&cmd_args, &session.working_dir).await?;

        if let Some(code) = output.exit_code {
            if code != 0 {
                return Err(classify_agent_error(code, &output));
            }
        }

        Ok(output)
    }

    async fn stop(&self, _session: &AgentSessionHandle) -> Result<(), AgentError> {
        Ok(())
    }
}
