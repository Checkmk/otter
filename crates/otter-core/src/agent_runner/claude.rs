use async_trait::async_trait;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::types::ProgressChunk;

use super::{
    AgentError, AgentOutput, AgentRunner, AgentSessionHandle, AgentSpec,
    classify_agent_error, run_subprocess, run_subprocess_streaming,
};

/// Agent runner targeting the Claude Code CLI.
///
/// - `start()` runs `claude [base_args] --session-id <uuid>` with the message on stdin.
/// - `prompt()` runs `claude [base_args] --resume <id>` with the message on stdin.
/// - `stop()` is a no-op — the CLI manages its own session cleanup.
pub struct ClaudeCodeRunner {
    base_args: Vec<String>,
}

impl ClaudeCodeRunner {
    pub fn new(allowed_tools: Option<Vec<String>>, permission_mode: Option<String>) -> Self {
        let mut base_args = Vec::new();
        if let Some(tools) = allowed_tools {
            base_args.push("--allowed-tools".to_string());
            base_args.push(tools.join(","));
        }
        if let Some(mode) = permission_mode {
            base_args.push("--permission-mode".to_string());
            base_args.push(mode);
        }
        Self { base_args }
    }
}

#[async_trait]
impl AgentRunner for ClaudeCodeRunner {
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

        let mut cmd_args = vec!["claude".to_string()];
        cmd_args.extend(self.base_args.clone());

        let output = if let Some(tx) = progress_tx {
            cmd_args.push("--output-format".to_string());
            cmd_args.push("stream-json".to_string());
            cmd_args.push("--verbose".to_string());
            cmd_args.push("--session-id".to_string());
            cmd_args.push(session_id);
            let cmd_args = spec.resource_limiter.apply(&cmd_args);
            run_subprocess_streaming(&cmd_args, &spec.working_dir, &spec.message, &tx, spec.scripts_dir.as_deref(), &spec.secrets, spec.sandbox_config.as_ref()).await?
        } else {
            cmd_args.push("--print".to_string());
            cmd_args.push("--session-id".to_string());
            cmd_args.push(session_id);
            let cmd_args = spec.resource_limiter.apply(&cmd_args);
            run_subprocess(&cmd_args, &spec.working_dir, &spec.message, spec.scripts_dir.as_deref(), &spec.secrets, spec.sandbox_config.as_ref()).await?
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
        let mut cmd_args = vec!["claude".to_string()];
        cmd_args.extend(self.base_args.clone());

        let output = if let Some(tx) = progress_tx {
            cmd_args.push("--output-format".to_string());
            cmd_args.push("stream-json".to_string());
            cmd_args.push("--verbose".to_string());
            cmd_args.push("--resume".to_string());
            cmd_args.push(session.id.clone());
            let cmd_args = session.resource_limiter.apply(&cmd_args);
            run_subprocess_streaming(&cmd_args, &session.working_dir, message, &tx, session.scripts_dir.as_deref(), secrets, session.sandbox_config.as_ref()).await?
        } else {
            cmd_args.push("--print".to_string());
            cmd_args.push("--resume".to_string());
            cmd_args.push(session.id.clone());
            let cmd_args = session.resource_limiter.apply(&cmd_args);
            run_subprocess(&cmd_args, &session.working_dir, message, session.scripts_dir.as_deref(), secrets, session.sandbox_config.as_ref()).await?
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

/// Parse a single line of Claude's `--output-format stream-json` output into progress chunks.
///
/// Returns an empty vec for events we don't care about (system init, rate limits, etc.).
pub(crate) fn parse_claude_stream_line(line: &str) -> Vec<ProgressChunk> {
    let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
        return vec![];
    };

    let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");

    match event_type {
        "assistant" => parse_assistant_event(&val),
        "result" => parse_result_event(&val),
        _ => vec![],
    }
}

fn parse_assistant_event(val: &serde_json::Value) -> Vec<ProgressChunk> {
    let Some(content) = val.pointer("/message/content").and_then(|c| c.as_array()) else {
        return vec![];
    };

    let mut chunks = Vec::new();
    for block in content {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "thinking" => {
                chunks.push(ProgressChunk::Status("Thinking...".to_string()));
            }
            "tool_use" => {
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                let input_summary = summarize_tool_input(name, block.get("input"));
                chunks.push(ProgressChunk::Status(format!("Using tool: {name}{input_summary}")));
            }
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        chunks.push(ProgressChunk::Stdout(text.to_string()));
                    }
                }
            }
            _ => {}
        }
    }
    chunks
}

fn parse_result_event(val: &serde_json::Value) -> Vec<ProgressChunk> {
    if let Some(result) = val.get("result").and_then(|r| r.as_str()) {
        if !result.is_empty() {
            return vec![ProgressChunk::Stdout(result.to_string())];
        }
    }
    vec![]
}

fn summarize_tool_input(tool_name: &str, input: Option<&serde_json::Value>) -> String {
    let Some(input) = input else { return String::new() };
    // For file-oriented tools, show the file path
    let path = input.get("file_path")
        .or_else(|| input.get("path"))
        .or_else(|| input.get("pattern"))
        .and_then(|v| v.as_str());
    if let Some(p) = path {
        return format!("({p})");
    }
    // For Bash, show a truncated command
    if tool_name == "Bash" {
        if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
            let short: String = cmd.chars().take(60).collect();
            let suffix = if cmd.len() > 60 { "..." } else { "" };
            return format!("({short}{suffix})");
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_thinking_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Let me think...","signature":"abc"}]}}"#;
        let chunks = parse_claude_stream_line(line);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], ProgressChunk::Status(s) if s == "Thinking..."));
    }

    #[test]
    fn parse_tool_use_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/main.rs"}}]}}"#;
        let chunks = parse_claude_stream_line(line);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], ProgressChunk::Status(s) if s == "Using tool: Read(src/main.rs)"));
    }

    #[test]
    fn parse_tool_use_with_pattern() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Glob","input":{"pattern":"**/*.rs"}}]}}"#;
        let chunks = parse_claude_stream_line(line);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], ProgressChunk::Status(s) if s == "Using tool: Glob(**/*.rs)"));
    }

    #[test]
    fn parse_bash_tool_truncates_long_commands() {
        let cmd = "a".repeat(100);
        let line = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"{cmd}"}}}}]}}}}"#
        );
        let chunks = parse_claude_stream_line(&line);
        assert_eq!(chunks.len(), 1);
        if let ProgressChunk::Status(s) = &chunks[0] {
            assert!(s.contains("..."), "should truncate: {s}");
            assert!(s.len() < 100, "should be shorter than original: {s}");
        } else {
            panic!("expected Status chunk");
        }
    }

    #[test]
    fn parse_text_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Hello, world!"}]}}"#;
        let chunks = parse_claude_stream_line(line);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], ProgressChunk::Stdout(s) if s == "Hello, world!"));
    }

    #[test]
    fn parse_result_event() {
        let line = r#"{"type":"result","subtype":"success","result":"The answer is 42","session_id":"abc"}"#;
        let chunks = parse_claude_stream_line(line);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], ProgressChunk::Stdout(s) if s == "The answer is 42"));
    }

    #[test]
    fn parse_system_init_ignored() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/tmp","session_id":"abc"}"#;
        let chunks = parse_claude_stream_line(line);
        assert!(chunks.is_empty());
    }

    #[test]
    fn parse_rate_limit_ignored() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{}}"#;
        let chunks = parse_claude_stream_line(line);
        assert!(chunks.is_empty());
    }

    #[test]
    fn parse_invalid_json_ignored() {
        let chunks = parse_claude_stream_line("not json at all");
        assert!(chunks.is_empty());
    }

    #[test]
    fn parse_multiple_content_blocks() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm","signature":"x"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"a.rs"}}]}}"#;
        let chunks = parse_claude_stream_line(line);
        assert_eq!(chunks.len(), 2);
        assert!(matches!(&chunks[0], ProgressChunk::Status(s) if s == "Thinking..."));
        assert!(matches!(&chunks[1], ProgressChunk::Status(s) if s.contains("Edit")));
    }

    #[test]
    fn parse_empty_text_ignored() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":""}]}}"#;
        let chunks = parse_claude_stream_line(line);
        assert!(chunks.is_empty());
    }

    #[test]
    fn parse_tool_use_no_input() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"WebSearch"}]}}"#;
        let chunks = parse_claude_stream_line(line);
        assert_eq!(chunks.len(), 1);
        assert!(matches!(&chunks[0], ProgressChunk::Status(s) if s == "Using tool: WebSearch"));
    }

    #[tokio::test]
    async fn streaming_subprocess_emits_chunks() {
        // GIVEN a script that prints stream-json events
        let dir = tempfile::tempdir().unwrap();
        let script = crate::test_helpers::write_executable_script(dir.path(), "mock-claude.sh", r#"#!/bin/bash
echo '{"type":"system","subtype":"init"}'
echo '{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm","signature":"x"}]}}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"foo.rs"}}]}}'
echo '{"type":"result","subtype":"success","result":"done"}'
"#).unwrap();

        let (tx, mut rx) = mpsc::channel(32);
        let cmd = vec![script.to_string_lossy().to_string()];

        // WHEN
        let output = run_subprocess_streaming(&cmd, dir.path(), "", &tx, None, &[], None).await.unwrap();

        // THEN — output contains the result text
        assert_eq!(output.stdout, "done");

        // Collect all chunks
        drop(tx);
        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }

        // Should have: Thinking, Using tool: Read(foo.rs), Stdout("done")
        assert!(chunks.iter().any(|c| matches!(c, ProgressChunk::Status(s) if s == "Thinking...")));
        assert!(chunks.iter().any(|c| matches!(c, ProgressChunk::Status(s) if s.contains("Read"))));
        assert!(chunks.iter().any(|c| matches!(c, ProgressChunk::Stdout(s) if s == "done")));
    }
}
