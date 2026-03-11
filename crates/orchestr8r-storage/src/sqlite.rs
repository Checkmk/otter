use anyhow::Context;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

use orchestr8r_core::types::{LogEntry, RunStatus, StorageBackend, WorkflowRun};

pub struct SqliteStorage {
    pub(crate) conn: Mutex<Connection>,
}

impl SqliteStorage {
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(db_path)
            .with_context(|| format!("Failed to open SQLite DB at {:?}", db_path))?;

        let storage = Self {
            conn: Mutex::new(conn),
        };
        storage.migrate()?;
        Ok(storage)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS workflow_runs (
                id TEXT PRIMARY KEY,
                workflow_name TEXT NOT NULL,
                status TEXT NOT NULL,
                current_step INTEGER NOT NULL,
                iteration INTEGER NOT NULL,
                started_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS step_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                iteration INTEGER NOT NULL,
                step_index INTEGER NOT NULL,
                step_type TEXT NOT NULL,
                stdout TEXT NOT NULL,
                stderr TEXT NOT NULL,
                exit_code INTEGER,
                accepted INTEGER,
                timestamp TEXT NOT NULL
            );
        ",
        )?;
        Ok(())
    }
}

impl StorageBackend for SqliteStorage {
    fn save_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO workflow_runs (id, workflow_name, status, current_step, iteration, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run.id.to_string(),
                run.workflow_name,
                run.status.to_string(),
                run.current_step as i64,
                run.iteration as i64,
                run.started_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn update_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE workflow_runs SET status=?2, current_step=?3, iteration=?4 WHERE id=?1",
            params![
                run.id.to_string(),
                run.status.to_string(),
                run.current_step as i64,
                run.iteration as i64,
            ],
        )?;
        Ok(())
    }

    fn append_log(&self, entry: LogEntry) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO step_logs (run_id, iteration, step_index, step_type, stdout, stderr, exit_code, accepted, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                entry.run_id.to_string(),
                entry.iteration as i64,
                entry.step_index as i64,
                entry.step_type,
                entry.stdout,
                entry.stderr,
                entry.exit_code,
                entry.accepted.map(|a| if a { 1i64 } else { 0i64 }),
                entry.timestamp.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn load_latest_run(&self, workflow_name: &str) -> anyhow::Result<Option<WorkflowRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, workflow_name, status, current_step, iteration, started_at
             FROM workflow_runs WHERE workflow_name=?1
             ORDER BY started_at DESC LIMIT 1",
        )?;

        let mut rows = stmt.query(params![workflow_name])?;
        if let Some(row) = rows.next()? {
            let id_str: String = row.get(0)?;
            let status_str: String = row.get(2)?;
            let started_at_str: String = row.get(5)?;

            let run = WorkflowRun {
                id: id_str.parse().context("invalid UUID in DB")?,
                workflow_name: row.get(1)?,
                status: match status_str.as_str() {
                    "running" => RunStatus::Running,
                    "waiting_checkpoint" => RunStatus::WaitingCheckpoint,
                    "completed" => RunStatus::Completed,
                    _ => RunStatus::Failed,
                },
                current_step: row.get::<_, i64>(3)? as usize,
                iteration: row.get::<_, i64>(4)? as u64,
                started_at: started_at_str.parse().context("invalid datetime in DB")?,
            };
            Ok(Some(run))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use orchestr8r_core::types::{LogEntry, RunStatus, WorkflowRun};

    fn in_memory_storage() -> SqliteStorage {
        // Use an in-process SQLite memory database for fast, isolated tests.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let storage = SqliteStorage {
            conn: Mutex::new(conn),
        };
        storage.migrate().unwrap();
        storage
    }

    fn make_run(name: &str) -> WorkflowRun {
        WorkflowRun::new(name.to_string())
    }

    #[test]
    fn save_and_load_latest_run() {
        // GIVEN
        let storage = in_memory_storage();
        let run = make_run("wf");
        storage.save_workflow_run(&run).unwrap();

        // WHEN
        let loaded = storage.load_latest_run("wf").unwrap().unwrap();

        // THEN
        assert_eq!(loaded.id, run.id);
        assert_eq!(loaded.workflow_name, "wf");
        assert_eq!(loaded.status, RunStatus::Running);
    }

    #[test]
    fn load_latest_returns_none_when_empty() {
        // GIVEN
        let storage = in_memory_storage();

        // WHEN / THEN
        assert!(storage.load_latest_run("nope").unwrap().is_none());
    }

    #[test]
    fn update_workflow_run_persists_changes() {
        // GIVEN
        let storage = in_memory_storage();
        let mut run = make_run("wf");
        storage.save_workflow_run(&run).unwrap();

        // WHEN
        run.status = RunStatus::Failed;
        run.iteration = 5;
        storage.update_workflow_run(&run).unwrap();

        // THEN
        let loaded = storage.load_latest_run("wf").unwrap().unwrap();
        assert_eq!(loaded.status, RunStatus::Failed);
        assert_eq!(loaded.iteration, 5);
    }

    #[test]
    fn load_latest_returns_most_recent_of_multiple_runs() {
        // GIVEN two runs where run2 started later
        let storage = in_memory_storage();
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
    fn append_log_and_count() {
        // GIVEN
        let storage = in_memory_storage();
        let run = make_run("wf");
        storage.save_workflow_run(&run).unwrap();
        for i in 0..3 {
            storage
                .append_log(LogEntry {
                    run_id: run.id,
                    iteration: 0,
                    step_index: i,
                    step_type: "shell".to_string(),
                    stdout: format!("out {i}"),
                    stderr: String::new(),
                    exit_code: Some(0),
                    accepted: None,
                    timestamp: Utc::now(),
                })
                .unwrap();
        }

        // THEN
        let conn = storage.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM step_logs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn append_log_stores_accepted_flag() {
        // GIVEN a checkpoint log entry with accepted = true
        let storage = in_memory_storage();
        let run = make_run("wf");
        storage.save_workflow_run(&run).unwrap();
        storage
            .append_log(LogEntry {
                run_id: run.id,
                iteration: 0,
                step_index: 0,
                step_type: "checkpoint".to_string(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
                accepted: Some(true),
                timestamp: Utc::now(),
            })
            .unwrap();

        // WHEN
        let conn = storage.conn.lock().unwrap();
        let accepted: i64 = conn
            .query_row("SELECT accepted FROM step_logs LIMIT 1", [], |r| r.get(0))
            .unwrap();

        // THEN
        assert_eq!(accepted, 1);
    }
}
