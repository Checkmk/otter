use async_trait::async_trait;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub command: Vec<String>,
    pub message: String,
    pub working_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AgentSessionHandle {
    pub id: String,
    pub command: Vec<String>,
    pub working_dir: PathBuf,
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
    async fn start(&self, spec: AgentSpec) -> Result<(AgentSessionHandle, AgentOutput), AgentError>;
    async fn prompt(
        &self,
        session: &AgentSessionHandle,
        message: &str,
    ) -> Result<AgentOutput, AgentError>;
    async fn stop(&self, session: &AgentSessionHandle) -> Result<(), AgentError>;
}

/// Agent runner targeting the Claude Code CLI.
///
/// - `start()` spawns `command --session-id <uuid>` with the message on stdin.
/// - `prompt()` spawns `command --resume <id>` with the new message on stdin.
/// - `stop()` is a no-op — the CLI manages its own session cleanup.
pub struct ClaudeCodeRunner;

#[async_trait]
impl AgentRunner for ClaudeCodeRunner {
    async fn start(&self, spec: AgentSpec) -> Result<(AgentSessionHandle, AgentOutput), AgentError> {
        let session_id = Uuid::new_v4().to_string();

        let handle = AgentSessionHandle {
            id: session_id.clone(),
            command: spec.command.clone(),
            working_dir: spec.working_dir,
        };

        let mut cmd_args = spec.command.clone();
        cmd_args.push("--session-id".to_string());
        cmd_args.push(session_id);

        let output = run_subprocess(&cmd_args, &handle.working_dir, &spec.message).await?;

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
    ) -> Result<AgentOutput, AgentError> {
        let mut cmd_args = session.command.clone();
        cmd_args.push("--resume".to_string());
        cmd_args.push(session.id.clone());

        let output = run_subprocess(&cmd_args, &session.working_dir, message).await?;

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

fn classify_agent_error(code: i32, output: &AgentOutput) -> AgentError {
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

async fn run_subprocess(
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
