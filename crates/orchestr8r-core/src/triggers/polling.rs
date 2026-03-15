use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::types::{TriggerError, TriggerEvent};
use super::TriggerSource;

#[derive(Serialize, Deserialize)]
struct SeenHashes {
    hashes: Vec<String>,
}

pub struct PollingTrigger {
    name: String,
    command: Vec<String>,
    interval: Duration,
    seen_path: PathBuf,
    scratch_base: PathBuf,
    workspace: Option<PathBuf>,
}

impl PollingTrigger {
    pub fn new(
        name: String,
        command: Vec<String>,
        interval: Duration,
        seen_path: PathBuf,
        scratch_base: PathBuf,
        workspace: Option<PathBuf>,
    ) -> Self {
        Self {
            name,
            command,
            interval,
            seen_path,
            scratch_base,
            workspace,
        }
    }

    fn load_seen(&self) -> anyhow::Result<HashSet<String>> {
        if !self.seen_path.exists() {
            return Ok(HashSet::new());
        }
        let content = std::fs::read_to_string(&self.seen_path)?;
        let data: SeenHashes = serde_json::from_str(&content)?;
        Ok(data.hashes.into_iter().collect())
    }

    fn save_seen(&self, seen: &HashSet<String>) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.seen_path.parent().unwrap())?;
        let mut hashes: Vec<String> = seen.iter().cloned().collect();
        hashes.sort();
        let data = SeenHashes { hashes };
        let content = serde_json::to_string(&data)?;
        std::fs::write(&self.seen_path, content)?;
        Ok(())
    }
}

#[async_trait]
impl TriggerSource for PollingTrigger {
    fn name(&self) -> &str {
        &self.name
    }

    async fn subscribe(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError> {
        info!("polling trigger started: command={:?}, interval={:?}, seen_path={}",
              self.command, self.interval, self.seen_path.display());
        loop {
            if let Err(e) = self.poll_once(&tx).await {
                error!("polling error: {}", e);
            }
            tokio::time::sleep(self.interval).await;
        }
    }

    async fn fire_once(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError> {
        self.poll_once(&tx)
            .await
            .map_err(|e| TriggerError::Failed(e.to_string()))
    }
}

impl PollingTrigger {
    async fn poll_once(&self, tx: &mpsc::Sender<TriggerEvent>) -> anyhow::Result<()> {
        debug!("polling: running '{}' with --poll", self.command[0]);
        let output = Command::new(&self.command[0])
            .args(&self.command[1..])
            .arg("--poll")
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run poll command '{}': {}", self.command[0], e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "poll command '{}' exited with status {}\nstderr: {}",
                self.command[0],
                output.status,
                stderr
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        debug!("poll command output: {}", stdout);

        let hashes: Vec<String> = serde_json::from_str(&stdout).map_err(|e| {
            anyhow::anyhow!("failed to parse poll output as JSON: {} (output was: {})", e, stdout)
        })?;

        info!("polling found {} hash(es)", hashes.len());
        let mut seen = self.load_seen()?;

        for hash in hashes {
            if !seen.contains(&hash) {
                info!("new hash from polling: {}", hash);
                seen.insert(hash.clone());
                self.save_seen(&seen)?;
                debug!("saved seen-hash file");

                let (run_id, ctx_dir) = if let Some(workspace) = &self.workspace {
                    let ctx_dir = workspace.join("trigger-context");
                    std::fs::create_dir_all(&ctx_dir)?;
                    debug!("using workspace context dir: {}", ctx_dir.display());
                    (None, ctx_dir)
                } else {
                    let run_id = Uuid::new_v4();
                    let scratch_dir = self.scratch_base.join(run_id.to_string());
                    let ctx_dir = scratch_dir.join("trigger-context");
                    std::fs::create_dir_all(&ctx_dir)?;
                    debug!("pre-allocated run_id {}, context dir: {}", run_id, ctx_dir.display());
                    (Some(run_id), ctx_dir)
                };

                debug!("running context command: {} --context {} {}", self.command[0], hash, ctx_dir.display());
                let context_output = Command::new(&self.command[0])
                    .args(&self.command[1..])
                    .arg("--context")
                    .arg(&hash)
                    .arg(&ctx_dir)
                    .output()
                    .await;

                match context_output {
                    Ok(output) => {
                        if !output.status.success() {
                            let stderr = String::from_utf8_lossy(&output.stderr);
                            warn!(
                                "context command failed for hash {}: {} (stderr: {})",
                                hash,
                                output.status,
                                stderr
                            );
                            continue;
                        }
                        debug!("context command succeeded for hash {}", hash);
                    }
                    Err(e) => {
                        warn!("failed to run context command for hash {}: {}", hash, e);
                        continue;
                    }
                }

                let event = TriggerEvent {
                    source: self.name.clone(),
                    payload: hash.clone(),
                    preallocated_run_id: run_id,
                };

                info!("sending trigger event for hash {}", hash);
                tx.send(event)
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to send trigger event: {}", e))?;
            } else {
                debug!("hash {} already seen, skipping", hash);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use crate::test_helpers::write_executable_script;

    #[tokio::test]
    async fn poll_fires_for_new_hashes() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\necho '[\"hash1\", \"hash2\"]'",
        ).unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            Duration::from_millis(100),
            temp_dir.path().join("seen.json"),
            temp_dir.path().to_path_buf(),
            None,
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN
        trigger.poll_once(&tx).await.unwrap();

        // THEN
        let event1 = rx.recv().await.expect("expected first event");
        assert_eq!(event1.payload, "hash1");
        assert_eq!(event1.source, "test");
        assert!(event1.preallocated_run_id.is_some());

        let event2 = rx.recv().await.expect("expected second event");
        assert_eq!(event2.payload, "hash2");
        assert!(event2.preallocated_run_id.is_some());
    }

    #[tokio::test]
    async fn poll_skips_seen_hashes() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        ).unwrap();

        let seen_path = temp_dir.path().join("seen.json");
        let data = SeenHashes {
            hashes: vec!["hash1".to_string()],
        };
        fs::write(&seen_path, serde_json::to_string(&data).unwrap()).unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            Duration::from_millis(100),
            seen_path,
            temp_dir.path().to_path_buf(),
            None,
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN
        trigger.poll_once(&tx).await.unwrap();

        // THEN
        assert!(rx.try_recv().is_err(), "no new events should be fired");
    }

    #[tokio::test]
    async fn poll_persists_seen_on_disk() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        ).unwrap();

        let seen_path = temp_dir.path().join("seen.json");

        let trigger = PollingTrigger::new(
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            Duration::from_millis(100),
            seen_path.clone(),
            temp_dir.path().to_path_buf(),
            None,
        );

        let (tx, _rx) = mpsc::channel(32);

        // WHEN
        trigger.poll_once(&tx).await.unwrap();

        // THEN
        assert!(seen_path.exists());
        let content = fs::read_to_string(&seen_path).unwrap();
        let data: SeenHashes = serde_json::from_str(&content).unwrap();
        assert_eq!(data.hashes, vec!["hash1"]);
    }

    #[tokio::test]
    async fn context_dir_created_on_poll() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\nif [[ \"$1\" == \"--context\" ]]; then touch \"$3/context.txt\"; fi\necho '[\"hash1\"]'",
        ).unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            Duration::from_millis(100),
            temp_dir.path().join("seen.json"),
            temp_dir.path().to_path_buf(),
            None,
        );

        let (tx, _rx) = mpsc::channel(32);

        // WHEN
        trigger.poll_once(&tx).await.unwrap();

        // THEN - verify context files exist
        let scratch_entries: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.metadata()
                    .map(|m| m.is_dir())
                    .unwrap_or(false)
            })
            .collect();

        assert!(!scratch_entries.is_empty(), "scratch directories should be created");

        // Find a context directory and verify it has the expected file
        let has_context = scratch_entries.iter().any(|entry| {
            let context_path = entry.path().join("trigger-context").join("context.txt");
            context_path.exists()
        });
        assert!(has_context, "context.txt should exist in trigger-context directory");
    }

    #[tokio::test]
    async fn fire_once_updates_seen_hashes() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        ).unwrap();

        let seen_path = temp_dir.path().join("seen.json");
        let trigger = PollingTrigger::new(
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            Duration::from_millis(100),
            seen_path.clone(),
            temp_dir.path().to_path_buf(),
            None,
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN
        trigger.fire_once(tx).await.unwrap();

        // THEN - event emitted
        let event = rx.recv().await.expect("expected event");
        assert_eq!(event.payload, "hash1");

        // AND - seen-hash file updated
        let content = fs::read_to_string(&seen_path).unwrap();
        let data: SeenHashes = serde_json::from_str(&content).unwrap();
        assert!(data.hashes.contains(&"hash1".to_string()));
    }

    #[tokio::test]
    async fn subscribe_continues_polling_after_interval() {
        // GIVEN a polling trigger with a script that returns new hashes each time
        let temp_dir = TempDir::new().unwrap();
        let script = r#"#!/bin/bash
if [[ "$1" == "--poll" ]]; then
  timestamp=$(date +%s%N)
  echo "[\"event-${timestamp}\"]"
  exit 0
elif [[ "$1" == "--context" ]]; then
  mkdir -p "$3"
  echo "event=$2" > "$3/metadata.txt"
  exit 0
fi
"#;
        let cmd_path = write_executable_script(temp_dir.path(), "mock-poller.sh", script).unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            Duration::from_millis(100),  // 100ms interval
            temp_dir.path().join("seen.json"),
            temp_dir.path().to_path_buf(),
            None,
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN we subscribe and let it run for 300ms (should get ~3 poll cycles)
        let subscribe_handle = tokio::spawn({
            let tx = tx.clone();
            async move {
                let _ = trigger.subscribe(tx).await;
            }
        });

        let mut fired_count = 0;
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_millis(300) {
            match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                Ok(Some(_event)) => {
                    fired_count += 1;
                    info!("Received event #{}", fired_count);
                }
                _ => {}
            }
        }

        // Abort the subscribe task
        subscribe_handle.abort();

        // THEN we should have received multiple Fired events (at least 2 from ~3 poll cycles)
        assert!(
            fired_count >= 2,
            "Expected at least 2 Fired events but got {}. Polling may not be continuing after interval.",
            fired_count
        );
    }

    #[tokio::test]
    async fn polling_trigger_shuts_down_cleanly() {
        // GIVEN a polling trigger that continuously polls
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\nif [[ \"$1\" == \"--poll\" ]]; then echo '[\"event1\"]'; fi\nif [[ \"$1\" == \"--context\" ]]; then mkdir -p \"$3\"; fi",
        ).unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            Duration::from_millis(50),
            temp_dir.path().join("seen.json"),
            temp_dir.path().to_path_buf(),
            None,
        );

        let (tx, _rx) = mpsc::channel(32);

        // WHEN we spawn subscribe and then abort it quickly
        let subscribe_handle = tokio::spawn({
            let tx = tx.clone();
            async move {
                let _ = trigger.subscribe(tx).await;
            }
        });

        // Give it time to start polling
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Abort the subscribe task
        subscribe_handle.abort();

        // Wait a bit to let any pending operations complete
        tokio::time::sleep(Duration::from_millis(50)).await;

        // THEN the abort should complete without panic or error (no "channel closed" errors in logs)
        assert!(subscribe_handle.is_finished());
    }
}
