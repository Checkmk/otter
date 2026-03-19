use async_trait::async_trait;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::resource_limiter::ResourceLimiter;
use crate::types::ProgressChunk;

mod claude;
mod copilot;

pub use claude::ClaudeCodeRunner;
pub use copilot::CopilotRunner;

#[derive(Clone)]
pub struct AgentSpec {
    pub message: String,
    pub working_dir: PathBuf,
    pub resource_limiter: Arc<dyn ResourceLimiter>,
}

#[derive(Clone)]
pub struct AgentSessionHandle {
    pub id: String,
    pub working_dir: PathBuf,
    pub resource_limiter: Arc<dyn ResourceLimiter>,
}

#[derive(Debug, Clone)]
pub struct AgentOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent failed: {0}")]
    Failed(String),
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn start(
        &self,
        spec: AgentSpec,
        progress_tx: Option<mpsc::Sender<ProgressChunk>>,
    ) -> Result<(AgentSessionHandle, AgentOutput), AgentError>;
    async fn prompt(
        &self,
        session: &AgentSessionHandle,
        message: &str,
        progress_tx: Option<mpsc::Sender<ProgressChunk>>,
    ) -> Result<AgentOutput, AgentError>;
    async fn stop(&self, session: &AgentSessionHandle) -> Result<(), AgentError>;
}

/// Wraps an arbitrary command. No session resumption — each start/prompt runs a fresh
/// subprocess with the message on stdin. Used by the `command` escape hatch in step defs.
pub struct CustomRunner {
    command: Vec<String>,
}

impl CustomRunner {
    pub fn new(command: Vec<String>) -> Self {
        Self { command }
    }
}

#[async_trait]
impl AgentRunner for CustomRunner {
    async fn start(
        &self,
        spec: AgentSpec,
        _progress_tx: Option<mpsc::Sender<ProgressChunk>>,
    ) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
        let handle = AgentSessionHandle {
            id: uuid::Uuid::new_v4().to_string(),
            working_dir: spec.working_dir.clone(),
            resource_limiter: spec.resource_limiter.clone(),
        };

        let cmd = spec.resource_limiter.apply(&self.command);
        let output = run_subprocess(&cmd, &spec.working_dir, &spec.message).await?;

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
        let cmd = session.resource_limiter.apply(&self.command);
        let output = run_subprocess(&cmd, &session.working_dir, message).await?;

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

/// Build an `AgentRunner` from a provider name and optional config.
///
/// Returns an error for unknown provider names.
pub fn build_runner(
    provider: &str,
    allowed_tools: Option<&[String]>,
    permission_mode: Option<&str>,
) -> Result<Arc<dyn AgentRunner>, AgentError> {
    match provider {
        "claude" => Ok(Arc::new(ClaudeCodeRunner::new(
            allowed_tools.map(|t| t.to_vec()),
            permission_mode.map(str::to_string),
        ))),
        "copilot" => Ok(Arc::new(CopilotRunner::new(
            allowed_tools.map(|t| t.to_vec()),
        ))),
        other => Err(AgentError::Failed(format!("unknown agent provider: {other}"))),
    }
}

pub(super) fn classify_agent_error(code: i32, output: &AgentOutput) -> AgentError {
    let combined = format!("{} {}", output.stdout, output.stderr);
    if combined.contains("out of extra usage")
        || combined.contains("rate limit")
        || combined.contains("Rate limit")
        || combined.contains("429")
    {
        let msg = output.stdout.trim().to_string();
        return AgentError::RateLimited(if msg.is_empty() { output.stderr.trim().to_string() } else { msg });
    }
    let detail = match (output.stdout.trim(), output.stderr.trim()) {
        ("", "") => String::new(),
        ("", err) => format!(" stderr: {err}"),
        (out, "") => format!(" stdout: {out}"),
        (out, err) => format!(" stdout: {out} | stderr: {err}"),
    };
    AgentError::Failed(format!("agent exited with code {code}{detail}"))
}

pub(super) async fn run_subprocess(
    cmd_args: &[String],
    working_dir: &std::path::Path,
    message: &str,
) -> Result<AgentOutput, AgentError> {
    if cmd_args.is_empty() {
        return Err(AgentError::Failed("empty command".to_string()));
    }

    let mut child = tokio::process::Command::new(&cmd_args[0])
        .args(&cmd_args[1..])
        .current_dir(working_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(message.as_bytes()).await?;
    }

    let output = child.wait_with_output().await?;

    Ok(AgentOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code(),
    })
}

pub(super) async fn run_subprocess_streaming(
    cmd_args: &[String],
    working_dir: &std::path::Path,
    message: &str,
    progress_tx: &mpsc::Sender<ProgressChunk>,
) -> Result<AgentOutput, AgentError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    if cmd_args.is_empty() {
        return Err(AgentError::Failed("empty command".to_string()));
    }

    let mut child = tokio::process::Command::new(&cmd_args[0])
        .args(&cmd_args[1..])
        .current_dir(working_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(message.as_bytes()).await?;
    }

    let stdout_pipe = child.stdout.take()
        .ok_or_else(|| AgentError::Failed("no stdout pipe".to_string()))?;
    let stderr_pipe = child.stderr.take()
        .ok_or_else(|| AgentError::Failed("no stderr pipe".to_string()))?;

    let mut stdout_reader = BufReader::new(stdout_pipe).lines();
    let mut stderr_reader = BufReader::new(stderr_pipe).lines();

    let mut result_text = String::new();
    let mut stderr_buf = String::new();

    loop {
        tokio::select! {
            line = stdout_reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        for chunk in claude::parse_claude_stream_line(&line) {
                            if let ProgressChunk::Stdout(ref text) = chunk {
                                result_text.push_str(text);
                                result_text.push('\n');
                            }
                            let _ = progress_tx.try_send(chunk);
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "error reading agent stdout");
                        break;
                    }
                }
            }
            line = stderr_reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if !stderr_buf.is_empty() {
                            stderr_buf.push('\n');
                        }
                        stderr_buf.push_str(&line);
                        let _ = progress_tx.try_send(ProgressChunk::Stderr(line));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "error reading agent stderr");
                    }
                }
            }
        }
    }

    let status = child.wait().await?;

    // Trim trailing newline from accumulated result text
    let stdout = result_text.trim_end().to_string();

    Ok(AgentOutput {
        stdout,
        stderr: stderr_buf,
        exit_code: status.code(),
    })
}

pub(super) async fn run_subprocess_no_stdin(
    cmd_args: &[String],
    working_dir: &std::path::Path,
) -> Result<AgentOutput, AgentError> {
    if cmd_args.is_empty() {
        return Err(AgentError::Failed("empty command".to_string()));
    }

    let output = tokio::process::Command::new(&cmd_args[0])
        .args(&cmd_args[1..])
        .current_dir(working_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await?;

    Ok(AgentOutput {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        exit_code: output.status.code(),
    })
}
