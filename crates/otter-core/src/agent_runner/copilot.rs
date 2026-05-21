use async_trait::async_trait;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{
    classify_agent_error, run_agent_subprocess, AgentError, AgentOutput, AgentRunner,
    AgentSessionHandle, AgentSpec, SubprocessSpec,
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
        progress_tx: Option<mpsc::Sender<ProgressChunk>>,
    ) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
        let session_id = Uuid::new_v4().to_string();

        let handle = AgentSessionHandle {
            id: session_id.clone(),
            working_dir: spec.working_dir.clone(),
            resource_limiter: spec.resource_limiter.clone(),
            scripts_dir: spec.scripts_dir.clone(),
            sandbox_config: spec.sandbox_config.clone(),
        };

        let mut cmd_args = vec!["copilot".to_string()];
        cmd_args.extend(self.base_args.clone());
        cmd_args.push(format!("--resume={session_id}"));
        cmd_args.push("-p".to_string());
        cmd_args.push(spec.message.clone());

        let output = if let Some(tx) = progress_tx {
            cmd_args.push("--stream".to_string());
            cmd_args.push("on".to_string());
            cmd_args.push("--output-format".to_string());
            cmd_args.push("json".to_string());
            let cmd_args = spec.resource_limiter.apply(&cmd_args);
            run_agent_subprocess(
                &SubprocessSpec {
                    cmd_args: &cmd_args,
                    working_dir: &spec.working_dir,
                    stdin_message: None,
                    scripts_dir: spec.scripts_dir.as_deref(),
                    secrets: &spec.secrets,
                    sandbox_config: spec.sandbox_config.as_ref(),
                },
                Some((&tx, parse_copilot_stream_line)),
            )
            .await?
        } else {
            let cmd_args = spec.resource_limiter.apply(&cmd_args);
            run_agent_subprocess(
                &SubprocessSpec {
                    cmd_args: &cmd_args,
                    working_dir: &spec.working_dir,
                    stdin_message: None,
                    scripts_dir: spec.scripts_dir.as_deref(),
                    secrets: &spec.secrets,
                    sandbox_config: spec.sandbox_config.as_ref(),
                },
                None,
            )
            .await?
        };

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
        progress_tx: Option<mpsc::Sender<ProgressChunk>>,
        secrets: &[(String, String)],
    ) -> Result<AgentOutput, AgentError> {
        let mut cmd_args = vec!["copilot".to_string()];
        cmd_args.extend(self.base_args.clone());
        cmd_args.push(format!("--resume={}", session.id));
        cmd_args.push("-p".to_string());
        cmd_args.push(message.to_string());

        let output = if let Some(tx) = progress_tx {
            cmd_args.push("--stream".to_string());
            cmd_args.push("on".to_string());
            cmd_args.push("--output-format".to_string());
            cmd_args.push("json".to_string());
            let cmd_args = session.resource_limiter.apply(&cmd_args);
            run_agent_subprocess(
                &SubprocessSpec {
                    cmd_args: &cmd_args,
                    working_dir: &session.working_dir,
                    stdin_message: None,
                    scripts_dir: session.scripts_dir.as_deref(),
                    secrets,
                    sandbox_config: session.sandbox_config.as_ref(),
                },
                Some((&tx, parse_copilot_stream_line)),
            )
            .await?
        } else {
            let cmd_args = session.resource_limiter.apply(&cmd_args);
            run_agent_subprocess(
                &SubprocessSpec {
                    cmd_args: &cmd_args,
                    working_dir: &session.working_dir,
                    stdin_message: None,
                    scripts_dir: session.scripts_dir.as_deref(),
                    secrets,
                    sandbox_config: session.sandbox_config.as_ref(),
                },
                None,
            )
            .await?
        };

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

/// Parse a single JSONL event from `copilot --stream on --output-format json`.
///
/// Emits `ProgressChunk::Status` for tool execution and turn events.
/// Deltas are accumulated silently into `stdout`; the final `assistant.message`
/// sets the canonical content.
fn parse_copilot_stream_line(line: &str, stdout: &mut String) -> Vec<ProgressChunk> {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
        return vec![];
    };

    match val.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "assistant.message_delta" => {
            if let Some(delta) = val.pointer("/data/deltaContent").and_then(|v| v.as_str()) {
                stdout.push_str(delta);
            }
            vec![]
        }
        "assistant.message" => {
            if let Some(content) = val.pointer("/data/content").and_then(|v| v.as_str()) {
                if !content.is_empty() {
                    *stdout = content.to_string();
                }
            }
            // Report tool requests as status
            let mut chunks = Vec::new();
            if let Some(reqs) = val.pointer("/data/toolRequests").and_then(|v| v.as_array()) {
                for req in reqs {
                    let name = req
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown");
                    if name == "report_intent" {
                        if let Some(intent) =
                            req.pointer("/arguments/intent").and_then(|v| v.as_str())
                        {
                            chunks.push(ProgressChunk::Status(intent.to_string()));
                        }
                    } else {
                        chunks.push(ProgressChunk::Status(format!("Using tool: {name}")));
                    }
                }
            }
            chunks
        }
        "tool.execution_start" => {
            let name = val
                .pointer("/data/toolName")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            if name == "report_intent" {
                return vec![];
            }
            vec![ProgressChunk::Status(format!("Using tool: {name}"))]
        }
        _ => vec![],
    }
}
