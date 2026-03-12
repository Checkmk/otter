use std::sync::Mutex;

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
}
