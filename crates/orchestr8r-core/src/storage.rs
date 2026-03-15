use std::sync::Mutex;
use uuid::Uuid;

use crate::types::{LogEntry, StorageBackend, WorkflowRun};

/// In-memory storage backend. Useful for testing without touching the filesystem.
pub struct InMemoryStorage {
    runs: Mutex<Vec<WorkflowRun>>,
    logs: Mutex<Vec<LogEntry>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self {
            runs: Mutex::new(Vec::new()),
            logs: Mutex::new(Vec::new()),
        }
    }

    pub fn runs(&self) -> Vec<WorkflowRun> {
        self.runs.lock().unwrap().clone()
    }

    pub fn logs(&self) -> Vec<LogEntry> {
        self.logs.lock().unwrap().clone()
    }
}

impl Default for InMemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl StorageBackend for InMemoryStorage {
    fn save_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()> {
        self.runs.lock().unwrap().push(run.clone());
        Ok(())
    }

    fn update_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()> {
        let mut runs = self.runs.lock().unwrap();
        if let Some(existing) = runs.iter_mut().find(|r| r.id == run.id) {
            *existing = run.clone();
        }
        Ok(())
    }

    fn append_log(&self, entry: LogEntry) -> anyhow::Result<()> {
        self.logs.lock().unwrap().push(entry);
        Ok(())
    }

    fn load_latest_run(&self, workflow_name: &str) -> anyhow::Result<Option<WorkflowRun>> {
        let runs = self.runs.lock().unwrap();
        let latest = runs
            .iter()
            .filter(|r| r.workflow_name == workflow_name)
            .max_by_key(|r| r.started_at);
        Ok(latest.cloned())
    }

    fn load_workflow_runs(&self, workflow_name: &str) -> anyhow::Result<Vec<WorkflowRun>> {
        let mut runs: Vec<WorkflowRun> = self
            .runs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.workflow_name == workflow_name)
            .cloned()
            .collect();
        runs.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(runs)
    }

    fn load_run_logs(&self, run_id: Uuid) -> anyhow::Result<Vec<LogEntry>> {
        let logs: Vec<LogEntry> = self
            .logs
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.run_id == run_id)
            .cloned()
            .collect();
        Ok(logs)
    }

    fn delete_run(&self, run_id: Uuid) -> anyhow::Result<()> {
        let mut runs = self.runs.lock().unwrap();
        runs.retain(|r| r.id != run_id);
        let mut logs = self.logs.lock().unwrap();
        logs.retain(|l| l.run_id != run_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RunStatus, WorkflowRun};

    fn make_run(name: &str) -> WorkflowRun {
        WorkflowRun::new(name.to_string())
    }

    #[test]
    fn save_and_load_latest_run() {
        // GIVEN
        let storage = InMemoryStorage::new();
        let run = make_run("test-workflow");
        storage.save_workflow_run(&run).unwrap();

        // WHEN
        let loaded = storage.load_latest_run("test-workflow").unwrap();

        // THEN
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().id, run.id);
    }

    #[test]
    fn load_latest_returns_none_for_unknown_workflow() {
        // GIVEN
        let storage = InMemoryStorage::new();

        // WHEN / THEN
        assert!(storage.load_latest_run("nonexistent").unwrap().is_none());
    }

    #[test]
    fn load_latest_returns_most_recent() {
        // GIVEN two runs where run2 started later
        let storage = InMemoryStorage::new();
        let run1 = make_run("wf");
        let mut run2 = make_run("wf");
        run2.started_at = run1.started_at + chrono::Duration::seconds(1);
        storage.save_workflow_run(&run1).unwrap();
        storage.save_workflow_run(&run2).unwrap();

        // WHEN
        let loaded = storage.load_latest_run("wf").unwrap().unwrap();

        // THEN
        assert_eq!(loaded.id, run2.id);
    }

    #[test]
    fn update_workflow_run_mutates_existing() {
        // GIVEN
        let storage = InMemoryStorage::new();
        let mut run = make_run("wf");
        storage.save_workflow_run(&run).unwrap();

        // WHEN
        run.status = RunStatus::Failed;
        run.iteration = 3;
        storage.update_workflow_run(&run).unwrap();

        // THEN
        let stored = storage.runs();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].status, RunStatus::Failed);
        assert_eq!(stored[0].iteration, 3);
    }

    #[test]
    fn append_log_stores_entries() {
        use chrono::Utc;
        use uuid::Uuid;

        // GIVEN
        let storage = InMemoryStorage::new();
        let run_id = Uuid::new_v4();
        for i in 0..3 {
            storage
                .append_log(LogEntry {
                    run_id,
                    iteration: 0,
                    step_index: i,
                    step_type: "shell".to_string(),
                    stdout: format!("out {i}"),
                    stderr: String::new(),
                    exit_code: Some(0),
                    accepted: None,
                    feedback: None,
                    timestamp: Utc::now(),
                })
                .unwrap();
        }

        // THEN
        assert_eq!(storage.logs().len(), 3);
    }

    #[test]
    fn load_workflow_runs_returns_all_runs_ordered_newest_first() {
        // GIVEN three runs for the same workflow, with different started_at times
        let storage = InMemoryStorage::new();
        let mut run1 = make_run("test-wf");
        let mut run2 = make_run("test-wf");
        let mut run3 = make_run("test-wf");

        run1.started_at = chrono::Utc::now();
        run2.started_at = run1.started_at + chrono::Duration::seconds(1);
        run3.started_at = run2.started_at + chrono::Duration::seconds(1);

        storage.save_workflow_run(&run1).unwrap();
        storage.save_workflow_run(&run2).unwrap();
        storage.save_workflow_run(&run3).unwrap();

        // Add one run for a different workflow
        let other_run = make_run("other-wf");
        storage.save_workflow_run(&other_run).unwrap();

        // WHEN
        let runs = storage.load_workflow_runs("test-wf").unwrap();

        // THEN
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].id, run3.id); // newest first
        assert_eq!(runs[1].id, run2.id);
        assert_eq!(runs[2].id, run1.id); // oldest last
    }

    #[test]
    fn load_run_logs_returns_only_logs_for_specified_run() {
        use chrono::Utc;
        use uuid::Uuid;

        // GIVEN
        let storage = InMemoryStorage::new();
        let run1_id = Uuid::new_v4();
        let run2_id = Uuid::new_v4();

        // Add logs for run1
        for i in 0..2 {
            storage
                .append_log(LogEntry {
                    run_id: run1_id,
                    iteration: 0,
                    step_index: i,
                    step_type: "shell".to_string(),
                    stdout: format!("run1-out{i}"),
                    stderr: String::new(),
                    exit_code: Some(0),
                    accepted: None,
                    feedback: None,
                    timestamp: Utc::now(),
                })
                .unwrap();
        }

        // Add logs for run2
        for i in 0..3 {
            storage
                .append_log(LogEntry {
                    run_id: run2_id,
                    iteration: 0,
                    step_index: i,
                    step_type: "shell".to_string(),
                    stdout: format!("run2-out{i}"),
                    stderr: String::new(),
                    exit_code: Some(0),
                    accepted: None,
                    feedback: None,
                    timestamp: Utc::now(),
                })
                .unwrap();
        }

        // WHEN
        let run1_logs = storage.load_run_logs(run1_id).unwrap();
        let run2_logs = storage.load_run_logs(run2_id).unwrap();

        // THEN
        assert_eq!(run1_logs.len(), 2);
        assert_eq!(run2_logs.len(), 3);
        assert!(run1_logs.iter().all(|l| l.run_id == run1_id));
        assert!(run2_logs.iter().all(|l| l.run_id == run2_id));
    }

    #[test]
    fn delete_run_removes_run_and_its_logs() {
        use chrono::Utc;

        // GIVEN
        let storage = InMemoryStorage::new();
        let run1 = make_run("wf");
        let run2 = make_run("wf");

        storage.save_workflow_run(&run1).unwrap();
        storage.save_workflow_run(&run2).unwrap();

        // Add logs for both runs
        for run_id in [run1.id, run2.id] {
            for i in 0..2 {
                storage
                    .append_log(LogEntry {
                        run_id,
                        iteration: 0,
                        step_index: i,
                        step_type: "shell".to_string(),
                        stdout: "test".to_string(),
                        stderr: String::new(),
                        exit_code: Some(0),
                        accepted: None,
                        feedback: None,
                        timestamp: Utc::now(),
                    })
                    .unwrap();
            }
        }

        // WHEN
        storage.delete_run(run1.id).unwrap();

        // THEN
        let runs = storage.runs();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, run2.id);

        // Logs for run1 should be deleted, but run2 logs should remain
        let remaining_logs = storage.logs();
        assert!(remaining_logs.iter().all(|l| l.run_id == run2.id));
        assert_eq!(remaining_logs.len(), 2);
    }
}
