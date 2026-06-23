use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use otter_secrets::SecretStore;

use super::TriggerSource;
use crate::process::{inject_isolated_env, PrependScriptsDir};
use crate::requirements::{resolve_requires, Requirements};
use crate::types::{PendingContext, TriggerError, TriggerEvent};

#[derive(Serialize, Deserialize)]
struct SeenHashes {
    hashes: Vec<String>,
}

/// Returns the path to the consumed-triggers file for the given workflow.
pub fn consumed_triggers_path(data_dir: &Path, workflow_name: &str) -> PathBuf {
    data_dir
        .join("triggers")
        .join(format!("{}-seen.json", workflow_name))
}

/// Loads consumed trigger IDs from `path`, returning a sorted Vec. Returns empty Vec if the file doesn't exist.
pub fn load_consumed_triggers(path: &Path) -> anyhow::Result<Vec<String>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(path)?;
    let data: SeenHashes = serde_json::from_str(&content)?;
    let mut triggers = data.hashes;
    triggers.sort();
    Ok(triggers)
}

/// Removes `trigger` from the consumed-triggers file at `path`. No-op if the trigger is not present.
pub fn delete_consumed_trigger(path: &Path, trigger: &str) -> anyhow::Result<()> {
    let mut set: HashSet<String> = load_consumed_triggers(path)?.into_iter().collect();
    set.remove(trigger);
    save_consumed_triggers(path, &set)
}

fn save_consumed_triggers(path: &Path, seen: &HashSet<String>) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut hashes: Vec<String> = seen.iter().cloned().collect();
    hashes.sort();
    let data = SeenHashes { hashes };
    let content = serde_json::to_string(&data)?;
    std::fs::write(path, content)?;
    Ok(())
}

pub struct PollingTrigger {
    name: String,
    workflow_name: String,
    poll_command: Vec<String>,
    context_command: Option<Vec<String>>,
    interval: Duration,
    seen_path: PathBuf,
    in_flight: Arc<Mutex<HashSet<String>>>,
    scripts_dir: Option<PathBuf>,
    secret_store: Arc<dyn SecretStore>,
    requirements: Option<Arc<Requirements>>,
    poll_secrets: Vec<String>,
}

impl PollingTrigger {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        workflow_name: String,
        poll_command: Vec<String>,
        context_command: Option<Vec<String>>,
        interval: Duration,
        seen_path: PathBuf,
        scripts_dir: Option<PathBuf>,
        secret_store: Arc<dyn SecretStore>,
        requirements: Option<Arc<Requirements>>,
        poll_secrets: Vec<String>,
    ) -> Self {
        Self {
            name,
            workflow_name,
            poll_command,
            context_command,
            interval,
            seen_path,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            scripts_dir,
            secret_store,
            requirements,
            poll_secrets,
        }
    }

    fn load_seen(&self) -> anyhow::Result<HashSet<String>> {
        Ok(load_consumed_triggers(&self.seen_path)?
            .into_iter()
            .collect())
    }

    fn save_seen(&self, seen: &HashSet<String>) -> anyhow::Result<()> {
        save_consumed_triggers(&self.seen_path, seen)
    }
}

#[async_trait]
impl TriggerSource for PollingTrigger {
    fn name(&self) -> &str {
        &self.name
    }

    async fn subscribe(&self, tx: mpsc::Sender<TriggerEvent>) -> Result<(), TriggerError> {
        info!(
            "polling trigger started: poll_command={:?}, interval={:?}, seen_path={}",
            self.poll_command,
            self.interval,
            self.seen_path.display()
        );
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

    async fn on_run_completed(&self, payload: &str, succeeded: bool) {
        let mut in_flight = self.in_flight.lock().unwrap();
        in_flight.remove(payload);
        drop(in_flight);

        if succeeded {
            match self.load_seen() {
                Ok(mut seen) => {
                    seen.insert(payload.to_string());
                    if let Err(e) = self.save_seen(&seen) {
                        warn!("failed to persist seen-hash for {}: {}", payload, e);
                    } else {
                        debug!("hash {} marked as seen after successful run", payload);
                    }
                }
                Err(e) => warn!(
                    "failed to load seen-hashes when completing run for {}: {}",
                    payload, e
                ),
            }
        } else {
            info!(
                "run failed for hash {}; hash not marked seen and will be retried on next poll",
                payload
            );
        }
    }
}

impl PollingTrigger {
    async fn poll_once(&self, tx: &mpsc::Sender<TriggerEvent>) -> anyhow::Result<()> {
        debug!("polling: running {:?}", self.poll_command);
        let resolved = resolve_requires(
            &self.poll_secrets,
            self.requirements.as_deref(),
            self.scripts_dir.as_deref(),
            self.secret_store.as_ref(),
            &self.workflow_name,
        )
        .map_err(|e| anyhow::anyhow!("requires resolution for poll command failed: {}", e))?;
        let mut cmd = Command::new(&self.poll_command[0]);
        cmd.args(&self.poll_command[1..]);
        inject_isolated_env(&mut cmd, &resolved, true);
        cmd.prepend_scripts_dir(self.scripts_dir.as_deref());
        let output = cmd.output().await.map_err(|e| {
            anyhow::anyhow!(
                "failed to run poll command '{}': {}",
                self.poll_command[0],
                e
            )
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!(
                "poll command '{}' exited with status {}\nstderr: {}",
                self.poll_command[0],
                output.status,
                stderr
            ));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        debug!("poll command output: {}", stdout);

        let hashes: Vec<String> = serde_json::from_str(&stdout).map_err(|e| {
            anyhow::anyhow!(
                "failed to parse poll output as JSON: {} (output was: {})",
                e,
                stdout
            )
        })?;

        let seen = self.load_seen()?;
        let total = hashes.len();
        let unseen: Vec<String> = {
            let in_flight = self.in_flight.lock().unwrap();
            hashes
                .into_iter()
                .filter(|h| !seen.contains(h) && !in_flight.contains(h))
                .collect()
        };

        info!("polling found {} hash(es), {} unseen", total, unseen.len());

        for hash in unseen {
            info!("new hash from polling: {}", hash);
            self.in_flight.lock().unwrap().insert(hash.clone());

            let run_id = Uuid::new_v4();

            let event = TriggerEvent {
                source: self.name.clone(),
                payload: hash.clone(),
                preallocated_run_id: Some(run_id),
                pending_context: self.context_command.as_ref().map(|cmd| PendingContext {
                    command: cmd.clone(),
                    hash: hash.clone(),
                    secrets: self.poll_secrets.clone(),
                }),
                inline_context: None,
            };

            info!("sending trigger event for hash {}", hash);
            tx.send(event)
                .await
                .map_err(|e| anyhow::anyhow!("failed to send trigger event: {}", e))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::write_executable_script;
    use otter_secrets::NoOpSecretStore;
    use std::fs;
    use tempfile::TempDir;

    fn no_secrets_store() -> Arc<dyn SecretStore> {
        Arc::new(NoOpSecretStore)
    }

    #[tokio::test]
    async fn poll_fires_for_new_hashes() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\necho '[\"hash1\", \"hash2\"]'",
        )
        .unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            None,
            Duration::from_millis(100),
            temp_dir.path().join("seen.json"),
            None,
            no_secrets_store(),
            None,
            vec![],
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
        )
        .unwrap();

        let seen_path = temp_dir.path().join("seen.json");
        let data = SeenHashes {
            hashes: vec!["hash1".to_string()],
        };
        fs::write(&seen_path, serde_json::to_string(&data).unwrap()).unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            None,
            Duration::from_millis(100),
            seen_path,
            None,
            no_secrets_store(),
            None,
            vec![],
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN
        trigger.poll_once(&tx).await.unwrap();

        // THEN
        assert!(rx.try_recv().is_err(), "no new events should be fired");
    }

    #[tokio::test]
    async fn poll_does_not_persist_seen_until_run_completes() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        )
        .unwrap();

        let seen_path = temp_dir.path().join("seen.json");

        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            None,
            Duration::from_millis(100),
            seen_path.clone(),
            None,
            no_secrets_store(),
            None,
            vec![],
        );

        let (tx, _rx) = mpsc::channel(32);

        // WHEN - poll fires but run has not yet completed
        trigger.poll_once(&tx).await.unwrap();

        // THEN - seen file is NOT written yet
        assert!(
            !seen_path.exists(),
            "seen file must not be written before run completes"
        );

        // WHEN - run completes successfully
        trigger.on_run_completed("hash1", true).await;

        // THEN - seen file is written
        assert!(seen_path.exists());
        let content = fs::read_to_string(&seen_path).unwrap();
        let data: SeenHashes = serde_json::from_str(&content).unwrap();
        assert_eq!(data.hashes, vec!["hash1"]);
    }

    #[tokio::test]
    async fn poll_with_context_command_sets_pending_context_on_event() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let poll_path = write_executable_script(
            temp_dir.path(),
            "mock-poll.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        )
        .unwrap();
        let ctx_path = write_executable_script(
            temp_dir.path(),
            "mock-context.sh",
            "#!/bin/bash\ntouch \"$2/context.txt\"",
        )
        .unwrap();

        let ctx_cmd = vec![ctx_path.to_string_lossy().to_string()];
        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![poll_path.to_string_lossy().to_string()],
            Some(ctx_cmd.clone()),
            Duration::from_millis(100),
            temp_dir.path().join("seen.json"),
            None,
            no_secrets_store(),
            None,
            vec![],
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN
        trigger.poll_once(&tx).await.unwrap();

        // THEN - event carries pending_context; no context files are created yet
        let event = rx.recv().await.expect("expected event");
        let pending = event
            .pending_context
            .expect("expected pending_context to be set");
        assert_eq!(pending.command, ctx_cmd);
        assert_eq!(pending.hash, "hash1");

        // No scratch directories created at poll time
        let dirs: Vec<_> = fs::read_dir(temp_dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.metadata().map(|m| m.is_dir()).unwrap_or(false))
            .collect();
        assert!(
            dirs.is_empty(),
            "no directories should be created at poll time"
        );
    }

    #[tokio::test]
    async fn fire_once_updates_seen_hashes_after_successful_run() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        )
        .unwrap();

        let seen_path = temp_dir.path().join("seen.json");
        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            None,
            Duration::from_millis(100),
            seen_path.clone(),
            None,
            no_secrets_store(),
            None,
            vec![],
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN
        trigger.fire_once(tx).await.unwrap();

        // THEN - event emitted
        let event = rx.recv().await.expect("expected event");
        assert_eq!(event.payload, "hash1");

        // AND - seen-hash file NOT yet written (run hasn't completed)
        assert!(
            !seen_path.exists(),
            "seen file must not be written before run completes"
        );

        // WHEN - run completes successfully
        trigger.on_run_completed("hash1", true).await;

        // THEN - seen-hash file written
        let content = fs::read_to_string(&seen_path).unwrap();
        let data: SeenHashes = serde_json::from_str(&content).unwrap();
        assert!(data.hashes.contains(&"hash1".to_string()));
    }

    #[tokio::test]
    async fn hash_not_in_seen_file_after_failed_run() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        )
        .unwrap();

        let seen_path = temp_dir.path().join("seen.json");
        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            None,
            Duration::from_millis(100),
            seen_path.clone(),
            None,
            no_secrets_store(),
            None,
            vec![],
        );

        let (tx, _rx) = mpsc::channel(32);
        trigger.poll_once(&tx).await.unwrap();

        // WHEN - run fails
        trigger.on_run_completed("hash1", false).await;

        // THEN - seen file is not written
        assert!(
            !seen_path.exists(),
            "failed run must not write hash to seen file"
        );
    }

    #[tokio::test]
    async fn hash_not_double_fired_while_in_flight() {
        // GIVEN
        let temp_dir = TempDir::new().unwrap();
        let cmd_path = write_executable_script(
            temp_dir.path(),
            "mock-poller.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        )
        .unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![cmd_path.to_string_lossy().to_string()],
            None,
            Duration::from_millis(100),
            temp_dir.path().join("seen.json"),
            None,
            no_secrets_store(),
            None,
            vec![],
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN - first poll fires the hash (run still in-flight)
        trigger.poll_once(&tx).await.unwrap();
        let event = rx.recv().await.expect("expected first event");
        assert_eq!(event.payload, "hash1");

        // WHEN - second poll while run is still in-flight
        trigger.poll_once(&tx).await.unwrap();

        // THEN - no second event fired
        assert!(
            rx.try_recv().is_err(),
            "hash must not be fired again while in-flight"
        );
    }

    #[tokio::test]
    async fn subscribe_continues_polling_after_interval() {
        // GIVEN a polling trigger with a script that returns new hashes each time
        let temp_dir = TempDir::new().unwrap();
        let poll_path = write_executable_script(
            temp_dir.path(),
            "mock-poll.sh",
            "#!/bin/bash\ntimestamp=$(date +%s%N)\necho \"[\\\"event-${timestamp}\\\"]\"\n",
        )
        .unwrap();
        let ctx_path = write_executable_script(
            temp_dir.path(),
            "mock-context.sh",
            "#!/bin/bash\nmkdir -p \"$2\"\necho \"event=$1\" > \"$2/metadata.txt\"\n",
        )
        .unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![poll_path.to_string_lossy().to_string()],
            Some(vec![ctx_path.to_string_lossy().to_string()]),
            Duration::from_millis(100), // 100ms interval
            temp_dir.path().join("seen.json"),
            None,
            no_secrets_store(),
            None,
            vec![],
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
        let poll_path = write_executable_script(
            temp_dir.path(),
            "mock-poll.sh",
            "#!/bin/bash\necho '[\"event1\"]'",
        )
        .unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![poll_path.to_string_lossy().to_string()],
            None,
            Duration::from_millis(50),
            temp_dir.path().join("seen.json"),
            None,
            no_secrets_store(),
            None,
            vec![],
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

    #[tokio::test]
    async fn context_command_none_still_fires_event() {
        // GIVEN a trigger with no context_command
        let temp_dir = TempDir::new().unwrap();
        let poll_path = write_executable_script(
            temp_dir.path(),
            "mock-poll.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        )
        .unwrap();

        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![poll_path.to_string_lossy().to_string()],
            None,
            Duration::from_millis(100),
            temp_dir.path().join("seen.json"),
            None,
            no_secrets_store(),
            None,
            vec![],
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN
        trigger.poll_once(&tx).await.unwrap();

        // THEN event is fired even without a context command
        let event = rx.recv().await.expect("expected event");
        assert_eq!(event.payload, "hash1");
        assert!(event.preallocated_run_id.is_some());
    }

    #[tokio::test]
    async fn poll_command_env_is_isolated_and_secret_injected() {
        // GIVEN a poll command that writes $POLL_SECRET and $OTTER_TEST_LEAK to a file,
        // then outputs a JSON hash array.
        let temp_dir = TempDir::new().unwrap();
        let env_file = temp_dir.path().join("env_check.txt");
        std::env::set_var("OTTER_TEST_LEAK", "should-not-appear");
        let poll_path = write_executable_script(
            temp_dir.path(),
            "mock-poll.sh",
            &format!(
                "#!/bin/bash\nprintf '%s|%s' \"$POLL_SECRET\" \"$OTTER_TEST_LEAK\" > '{}'\necho '[\"hash1\"]'",
                env_file.display()
            ),
        ).unwrap();

        struct OneSecret;
        impl SecretStore for OneSecret {
            fn get(&self, key: &str) -> Option<String> {
                if key == "POLL_SECRET" {
                    Some("injected".to_string())
                } else {
                    None
                }
            }
            fn list(&self) -> Vec<String> {
                vec!["POLL_SECRET".to_string()]
            }
            fn set(&self, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn delete(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![poll_path.to_string_lossy().to_string()],
            None,
            Duration::from_millis(100),
            temp_dir.path().join("seen.json"),
            None,
            Arc::new(OneSecret),
            None,
            vec!["POLL_SECRET".to_string()],
        );

        let (tx, _rx) = mpsc::channel(32);

        // WHEN
        trigger.poll_once(&tx).await.unwrap();

        // THEN — secret was injected; daemon-env var did not leak
        let contents = fs::read_to_string(&env_file).unwrap();
        let parts: Vec<&str> = contents.trim().splitn(2, '|').collect();
        assert_eq!(parts[0], "injected", "declared secret must be injected");
        assert_eq!(parts[1], "", "daemon env must not leak into poll command");
    }

    #[tokio::test]
    async fn pending_context_carries_poll_secrets() {
        // GIVEN
        struct DummyStore;
        impl SecretStore for DummyStore {
            fn get(&self, _: &str) -> Option<String> {
                Some("dummy".to_string())
            }
            fn list(&self) -> Vec<String> {
                vec![]
            }
            fn set(&self, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn delete(&self, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let temp_dir = TempDir::new().unwrap();
        let poll_path = write_executable_script(
            temp_dir.path(),
            "mock-poll.sh",
            "#!/bin/bash\necho '[\"hash1\"]'",
        )
        .unwrap();
        let ctx_cmd = vec!["ctx.sh".to_string()];
        let trigger = PollingTrigger::new(
            "test".to_string(),
            "test".to_string(),
            vec![poll_path.to_string_lossy().to_string()],
            Some(ctx_cmd),
            Duration::from_millis(100),
            temp_dir.path().join("seen.json"),
            None,
            Arc::new(DummyStore),
            None,
            vec!["API_KEY".to_string()],
        );

        let (tx, mut rx) = mpsc::channel(32);

        // WHEN
        trigger.poll_once(&tx).await.unwrap();

        // THEN — PendingContext carries the poll_secrets
        let event = rx.recv().await.unwrap();
        let ctx = event.pending_context.unwrap();
        assert_eq!(ctx.secrets, vec!["API_KEY".to_string()]);
    }
}
