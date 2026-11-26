use std::collections::HashSet;
use std::fs::DirEntry;
use std::path::Path;
use std::time::SystemTime;

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use tracing::{info, warn};
use uuid::Uuid;

use crate::types::{StorageBackend, WorkflowRun};

pub fn delete_run(
    storage: &dyn StorageBackend,
    runs_dir: &Path,
    run_id: Uuid,
) -> anyhow::Result<()> {
    remove_dir_if_present(&runs_dir.join(run_id.to_string()))?;
    storage.delete_run(run_id)
}

pub fn cull_expired_runs(
    storage: &dyn StorageBackend,
    runs_dir: &Path,
    max_age: Duration,
    now: DateTime<Utc>,
) -> Vec<Uuid> {
    let cutoff = now - max_age;
    let runs = match storage.load_all_runs() {
        Ok(runs) => runs,
        Err(e) => {
            warn!("Run retention skipped, failed to load runs: {e:#}");
            return Vec::new();
        }
    };
    let stored_ids: HashSet<Uuid> = runs.iter().map(|r| r.id).collect();
    let is_expired = |run: &WorkflowRun| {
        run.status.is_finished()
            && run.started_at < cutoff
            && last_activity(storage, run).is_ok_and(|at| at < cutoff)
    };

    let mut deleted = Vec::new();
    for run in runs.iter().filter(|run| is_expired(run)) {
        match delete_run(storage, runs_dir, run.id) {
            Ok(()) => deleted.push(run.id),
            Err(e) => warn!(run_id = %run.id, "Failed to delete expired run: {e:#}"),
        }
    }

    let orphaned_dirs = remove_orphaned_run_dirs(runs_dir, &stored_ids, cutoff.into());
    if !deleted.is_empty() || orphaned_dirs > 0 {
        info!(
            runs = deleted.len(),
            orphaned_dirs, "Deleted runs past retention"
        );
    }
    deleted
}

fn last_activity(storage: &dyn StorageBackend, run: &WorkflowRun) -> anyhow::Result<DateTime<Utc>> {
    let last_log = storage
        .load_run_logs(run.id)?
        .into_iter()
        .map(|log| log.timestamp)
        .max();
    Ok(last_log.map_or(run.started_at, |at| at.max(run.started_at)))
}

fn remove_orphaned_run_dirs(
    runs_dir: &Path,
    stored_ids: &HashSet<Uuid>,
    cutoff: SystemTime,
) -> usize {
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let is_orphan = run_dir_id(&entry).is_some_and(|id| !stored_ids.contains(&id));
        let modified = entry.metadata().and_then(|m| m.modified());
        if !is_orphan || !modified.is_ok_and(|m| m < cutoff) {
            continue;
        }
        match remove_dir_if_present(&entry.path()) {
            Ok(()) => removed += 1,
            Err(e) => warn!("Failed to delete orphaned run dir: {e:#}"),
        }
    }
    removed
}

fn run_dir_id(entry: &DirEntry) -> Option<Uuid> {
    if !entry.file_type().is_ok_and(|t| t.is_dir()) {
        return None;
    }
    let name = entry.file_name().into_string().ok()?;
    Uuid::try_parse(&name)
        .ok()
        .filter(|id| id.to_string() == name)
}

fn remove_dir_if_present(dir: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("Failed to remove {}", dir.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorage;
    use crate::types::{LogEntry, RunStatus};
    use tempfile::TempDir;

    fn days(n: i64) -> Duration {
        Duration::days(n)
    }

    fn stored_run(
        storage: &InMemoryStorage,
        runs_dir: &Path,
        status: RunStatus,
        age: Duration,
    ) -> Uuid {
        let mut run = WorkflowRun::new("wf".to_string());
        run.status = status;
        run.started_at = Utc::now() - age;
        storage.save_workflow_run(&run).unwrap();
        std::fs::create_dir_all(runs_dir.join(run.id.to_string())).unwrap();
        run.id
    }

    fn log_entry(run_id: Uuid, age: Duration) -> LogEntry {
        LogEntry {
            run_id,
            iteration: 0,
            step_index: 0,
            step_type: "shell".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            accepted: None,
            feedback: None,
            timestamp: Utc::now() - age,
        }
    }

    fn stored_ids(storage: &InMemoryStorage) -> Vec<Uuid> {
        storage.runs().iter().map(|r| r.id).collect()
    }

    #[test]
    fn deletes_finished_runs_older_than_max_age() {
        // GIVEN finished runs on both sides of the cutoff
        let storage = InMemoryStorage::new();
        let dir = TempDir::new().unwrap();
        let old_completed = stored_run(&storage, dir.path(), RunStatus::Completed, days(31));
        let old_failed = stored_run(&storage, dir.path(), RunStatus::Failed, days(40));
        let old_stopped = stored_run(&storage, dir.path(), RunStatus::Stopped, days(50));
        let recent = stored_run(&storage, dir.path(), RunStatus::Completed, days(29));

        // WHEN
        let mut deleted = cull_expired_runs(&storage, dir.path(), days(30), Utc::now());

        // THEN only the old runs and their directories are gone
        let mut expected = vec![old_completed, old_failed, old_stopped];
        deleted.sort();
        expected.sort();
        assert_eq!(deleted, expected);
        assert_eq!(stored_ids(&storage), vec![recent]);
        for id in expected {
            assert!(!dir.path().join(id.to_string()).exists());
        }
        assert!(dir.path().join(recent.to_string()).exists());
    }

    #[test]
    fn keeps_unfinished_runs_regardless_of_age() {
        // GIVEN old runs that are still in progress
        let storage = InMemoryStorage::new();
        let dir = TempDir::new().unwrap();
        let running = stored_run(&storage, dir.path(), RunStatus::Running, days(90));
        let paused = stored_run(&storage, dir.path(), RunStatus::WaitingCheckpoint, days(90));

        // WHEN culling as if a year had passed
        let deleted = cull_expired_runs(&storage, dir.path(), days(30), Utc::now() + days(365));

        // THEN neither the runs nor their directories are touched
        assert!(deleted.is_empty());
        assert_eq!(stored_ids(&storage), vec![running, paused]);
        assert!(dir.path().join(running.to_string()).exists());
        assert!(dir.path().join(paused.to_string()).exists());
    }

    #[test]
    fn keeps_old_runs_with_recent_activity() {
        // GIVEN a run started long ago that finished after a recent checkpoint
        let storage = InMemoryStorage::new();
        let dir = TempDir::new().unwrap();
        let id = stored_run(&storage, dir.path(), RunStatus::Completed, days(35));
        storage.append_log(log_entry(id, days(1))).unwrap();

        // WHEN
        let deleted = cull_expired_runs(&storage, dir.path(), days(30), Utc::now());

        // THEN
        assert!(deleted.is_empty());
        assert_eq!(stored_ids(&storage), vec![id]);
    }

    #[test]
    fn deletes_old_runs_whose_last_activity_is_old() {
        // GIVEN
        let storage = InMemoryStorage::new();
        let dir = TempDir::new().unwrap();
        let id = stored_run(&storage, dir.path(), RunStatus::Completed, days(35));
        storage.append_log(log_entry(id, days(31))).unwrap();

        // WHEN
        let deleted = cull_expired_runs(&storage, dir.path(), days(30), Utc::now());

        // THEN
        assert_eq!(deleted, vec![id]);
    }

    #[test]
    fn deletes_stored_run_without_directory() {
        // GIVEN an old run whose directory was already removed
        let storage = InMemoryStorage::new();
        let dir = TempDir::new().unwrap();
        let id = stored_run(&storage, dir.path(), RunStatus::Completed, days(31));
        std::fs::remove_dir(dir.path().join(id.to_string())).unwrap();

        // WHEN
        let deleted = cull_expired_runs(&storage, dir.path(), days(30), Utc::now());

        // THEN
        assert_eq!(deleted, vec![id]);
        assert!(stored_ids(&storage).is_empty());
    }

    #[test]
    fn deletes_orphaned_run_dirs_last_modified_before_cutoff() {
        // GIVEN a run dir with no stored run, plus entries otter never creates
        let storage = InMemoryStorage::new();
        let dir = TempDir::new().unwrap();
        let orphan = dir.path().join(Uuid::new_v4().to_string());
        let unrelated = dir.path().join("not-a-run");
        let simple_uuid = dir.path().join(Uuid::new_v4().simple().to_string());
        let uuid_file = dir.path().join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(orphan.join("trigger-context")).unwrap();
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::create_dir_all(&simple_uuid).unwrap();
        std::fs::write(&uuid_file, "").unwrap();

        // WHEN culling as if a year had passed
        cull_expired_runs(&storage, dir.path(), days(30), Utc::now() + days(365));

        // THEN only the orphaned run dir is removed
        assert!(!orphan.exists());
        assert!(unrelated.exists());
        assert!(simple_uuid.exists());
        assert!(uuid_file.exists());
    }

    #[test]
    fn keeps_recently_modified_orphaned_run_dirs() {
        // GIVEN a freshly pre-allocated run dir not yet stored
        let storage = InMemoryStorage::new();
        let dir = TempDir::new().unwrap();
        let pending = dir.path().join(Uuid::new_v4().to_string());
        std::fs::create_dir_all(&pending).unwrap();

        // WHEN
        cull_expired_runs(&storage, dir.path(), days(30), Utc::now());

        // THEN
        assert!(pending.exists());
    }

    #[test]
    fn missing_runs_dir_is_not_an_error() {
        // GIVEN
        let storage = InMemoryStorage::new();
        let dir = TempDir::new().unwrap();
        let id = stored_run(&storage, dir.path(), RunStatus::Completed, days(31));

        // WHEN culling against a runs dir that does not exist
        let deleted = cull_expired_runs(&storage, &dir.path().join("absent"), days(30), Utc::now());

        // THEN the stored run is still deleted
        assert_eq!(deleted, vec![id]);
    }
}
