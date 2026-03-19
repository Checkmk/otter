use anyhow::Context;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;
use uuid::Uuid;

use orchestr8r_core::types::{LogEntry, RunStatus, StorageBackend, WorkflowRun};

/// Current schema version. Increment this and add a corresponding entry to `MIGRATIONS` when
/// making schema changes.
const SCHEMA_VERSION: u32 = 3;

const MIGRATIONS: &[fn(&Connection) -> anyhow::Result<()>] = &[
    // v0 -> v1: initial schema
    |conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workflow_runs (
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
                feedback TEXT,
                timestamp TEXT NOT NULL
            );",
        )?;
        Ok(())
    },
    // v1 -> v2: add trigger_payload column to workflow_runs
    |conn| {
        conn.execute_batch(
            "ALTER TABLE workflow_runs ADD COLUMN trigger_payload TEXT;",
        )?;
        Ok(())
    },
    // v2 -> v3: add workflows table; orphaned runs are those whose workflow_name
    // has no entry here.
    |conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workflows (
                name TEXT PRIMARY KEY
            );",
        )?;
        Ok(())
    },
];

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
        let current: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let target = SCHEMA_VERSION;
        debug_assert_eq!(target, MIGRATIONS.len() as u32, "SCHEMA_VERSION must equal MIGRATIONS.len()");

        if current >= target {
            return Ok(());
        }

        let tx = conn.unchecked_transaction()?;
        for (i, migration) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            tracing::info!(version = i + 1, "running migration");
            migration(&tx)?;
        }
        tx.pragma_update(None, "user_version", target)?;
        tx.commit()?;
        Ok(())
    }
}

fn row_to_run(row: &rusqlite::Row<'_>) -> anyhow::Result<WorkflowRun> {
    let id_str: String = row.get(0)?;
    let status_str: String = row.get(2)?;
    let started_at_str: String = row.get(5)?;
    let orphaned_i: i64 = row.get::<_, Option<i64>>(7)?.unwrap_or(0);
    Ok(WorkflowRun {
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
        trigger_payload: row.get(6)?,
        orphaned: orphaned_i != 0,
    })
}

impl StorageBackend for SqliteStorage {
    fn save_workflow_run(&self, run: &WorkflowRun) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO workflow_runs (id, workflow_name, status, current_step, iteration, started_at, trigger_payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.id.to_string(),
                run.workflow_name,
                run.status.to_string(),
                run.current_step as i64,
                run.iteration as i64,
                run.started_at.to_rfc3339(),
                &run.trigger_payload,
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
            "INSERT INTO step_logs (run_id, iteration, step_index, step_type, stdout, stderr, exit_code, accepted, feedback, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                entry.run_id.to_string(),
                entry.iteration as i64,
                entry.step_index as i64,
                entry.step_type,
                entry.stdout,
                entry.stderr,
                entry.exit_code,
                entry.accepted.map(|a| if a { 1i64 } else { 0i64 }),
                entry.feedback,
                entry.timestamp.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    fn load_latest_run(&self, workflow_name: &str) -> anyhow::Result<Option<WorkflowRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT wr.id, wr.workflow_name, wr.status, wr.current_step, wr.iteration,
                    wr.started_at, wr.trigger_payload, (w.name IS NULL) AS orphaned
             FROM workflow_runs wr
             LEFT JOIN workflows w ON wr.workflow_name = w.name
             WHERE wr.workflow_name=?1
             ORDER BY wr.started_at DESC LIMIT 1",
        )?;

        let mut rows = stmt.query(params![workflow_name])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_run(row)?))
        } else {
            Ok(None)
        }
    }

    fn load_workflow_runs(&self, workflow_name: &str) -> anyhow::Result<Vec<WorkflowRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT wr.id, wr.workflow_name, wr.status, wr.current_step, wr.iteration,
                    wr.started_at, wr.trigger_payload, (w.name IS NULL) AS orphaned
             FROM workflow_runs wr
             LEFT JOIN workflows w ON wr.workflow_name = w.name
             WHERE wr.workflow_name=?1
             ORDER BY wr.started_at DESC",
        )?;

        let mut runs = Vec::new();
        let mut rows = stmt.query(params![workflow_name])?;
        while let Some(row) = rows.next()? {
            runs.push(row_to_run(row)?);
        }
        Ok(runs)
    }

    fn load_all_runs(&self) -> anyhow::Result<Vec<WorkflowRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT wr.id, wr.workflow_name, wr.status, wr.current_step, wr.iteration,
                    wr.started_at, wr.trigger_payload, (w.name IS NULL) AS orphaned
             FROM workflow_runs wr
             LEFT JOIN workflows w ON wr.workflow_name = w.name
             ORDER BY wr.started_at DESC",
        )?;

        let mut runs = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            runs.push(row_to_run(row)?);
        }
        Ok(runs)
    }

    fn load_run_logs(&self, run_id: Uuid) -> anyhow::Result<Vec<LogEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT run_id, iteration, step_index, step_type, stdout, stderr, exit_code, accepted, feedback, timestamp
             FROM step_logs WHERE run_id=?1",
        )?;

        let mut logs = Vec::new();
        let mut rows = stmt.query(params![run_id.to_string()])?;
        while let Some(row) = rows.next()? {
            let run_id_str: String = row.get(0)?;
            let timestamp_str: String = row.get(9)?;
            let accepted_i: Option<i64> = row.get(7)?;

            let log = LogEntry {
                run_id: run_id_str.parse().context("invalid UUID in DB")?,
                iteration: row.get::<_, i64>(1)? as u64,
                step_index: row.get::<_, i64>(2)? as usize,
                step_type: row.get(3)?,
                stdout: row.get(4)?,
                stderr: row.get(5)?,
                exit_code: row.get(6)?,
                accepted: accepted_i.map(|a| a != 0),
                feedback: row.get(8)?,
                timestamp: timestamp_str.parse().context("invalid datetime in DB")?,
            };
            logs.push(log);
        }
        Ok(logs)
    }

    fn delete_run(&self, run_id: Uuid) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let run_id_str = run_id.to_string();
        conn.execute(
            "DELETE FROM workflow_runs WHERE id=?1",
            params![&run_id_str],
        )?;
        conn.execute(
            "DELETE FROM step_logs WHERE run_id=?1",
            params![&run_id_str],
        )?;
        Ok(())
    }

    fn register_workflow(&self, workflow_name: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO workflows (name) VALUES (?1)",
            params![workflow_name],
        )?;
        Ok(())
    }

    fn deregister_workflow(&self, workflow_name: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM workflows WHERE name=?1",
            params![workflow_name],
        )?;
        Ok(())
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
                    feedback: None,
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
    fn migration_is_idempotent() {
        let storage = in_memory_storage();
        // migrate() was already called by in_memory_storage(); calling again should be a no-op
        storage.migrate().unwrap();
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
                feedback: None,
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

    #[test]
    fn load_workflow_runs_returns_all_runs_ordered_by_started_at_desc() {
        // GIVEN three runs for the same workflow with different timestamps
        let storage = in_memory_storage();
        let mut run1 = make_run("test-wf");
        let mut run2 = make_run("test-wf");
        let mut run3 = make_run("test-wf");

        run1.started_at = Utc::now();
        run2.started_at = run1.started_at + chrono::Duration::seconds(10);
        run3.started_at = run2.started_at + chrono::Duration::seconds(10);

        storage.save_workflow_run(&run1).unwrap();
        storage.save_workflow_run(&run2).unwrap();
        storage.save_workflow_run(&run3).unwrap();

        // Add a run for a different workflow to verify filtering
        let other = make_run("other-wf");
        storage.save_workflow_run(&other).unwrap();

        // WHEN
        let runs = storage.load_workflow_runs("test-wf").unwrap();

        // THEN
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].id, run3.id); // newest first
        assert_eq!(runs[1].id, run2.id);
        assert_eq!(runs[2].id, run1.id); // oldest last
    }

    #[test]
    fn load_run_logs_returns_all_logs_for_run() {
        // GIVEN a run with multiple log entries
        let storage = in_memory_storage();
        let run = make_run("wf");
        storage.save_workflow_run(&run).unwrap();

        let mut entries = Vec::new();
        for i in 0..5 {
            let entry = LogEntry {
                run_id: run.id,
                iteration: 0,
                step_index: i,
                step_type: "shell".to_string(),
                stdout: format!("output {i}"),
                stderr: String::new(),
                exit_code: Some(0),
                accepted: None,
                feedback: None,
                timestamp: Utc::now(),
            };
            storage.append_log(entry.clone()).unwrap();
            entries.push(entry);
        }

        // WHEN
        let logs = storage.load_run_logs(run.id).unwrap();

        // THEN
        assert_eq!(logs.len(), 5);
        for (i, log) in logs.iter().enumerate() {
            assert_eq!(log.run_id, run.id);
            assert_eq!(log.step_index, i);
            assert_eq!(log.stdout, format!("output {i}"));
        }
    }

    #[test]
    fn delete_run_removes_run_and_all_its_logs() {
        // GIVEN two runs with logs
        let storage = in_memory_storage();
        let run1 = make_run("wf");
        let run2 = make_run("wf");

        storage.save_workflow_run(&run1).unwrap();
        storage.save_workflow_run(&run2).unwrap();

        // Add logs for both runs
        for run in [&run1, &run2] {
            for i in 0..3 {
                storage
                    .append_log(LogEntry {
                        run_id: run.id,
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
        // run1 should be deleted
        assert!(storage.load_latest_run("wf").unwrap().unwrap().id == run2.id);

        // All logs for run1 should be deleted
        let conn = storage.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM step_logs WHERE run_id = ?1",
                [run1.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        // Logs for run2 should still exist
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM step_logs WHERE run_id = ?1",
                [run2.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn trigger_payload_persists_and_is_retrieved() {
        // GIVEN a run with trigger_payload set
        let storage = in_memory_storage();
        let mut run = make_run("triggered-wf");
        run.trigger_payload = Some("hash-abc123".to_string());
        storage.save_workflow_run(&run).unwrap();

        // WHEN
        let loaded = storage.load_latest_run("triggered-wf").unwrap().unwrap();

        // THEN
        assert_eq!(loaded.trigger_payload, Some("hash-abc123".to_string()));
    }

    #[test]
    fn orphaned_via_join_when_workflow_not_registered() {
        // GIVEN a run saved without registering the workflow
        let storage = in_memory_storage();
        let run = make_run("removed-wf");
        storage.save_workflow_run(&run).unwrap();

        // WHEN
        let loaded = storage.load_latest_run("removed-wf").unwrap().unwrap();

        // THEN — no entry in workflows table → orphaned
        assert!(loaded.orphaned);
    }

    #[test]
    fn not_orphaned_when_workflow_registered() {
        // GIVEN a run saved for a registered workflow
        let storage = in_memory_storage();
        let run = make_run("active-wf");
        storage.save_workflow_run(&run).unwrap();
        storage.register_workflow("active-wf").unwrap();

        // WHEN
        let loaded = storage.load_latest_run("active-wf").unwrap().unwrap();

        // THEN
        assert!(!loaded.orphaned);
    }

    #[test]
    fn mark_runs_orphaned_removes_from_workflows_table() {
        // GIVEN a registered workflow with runs
        let storage = in_memory_storage();
        let run = make_run("wf");
        storage.save_workflow_run(&run).unwrap();
        storage.register_workflow("wf").unwrap();

        // WHEN — deregister
        storage.deregister_workflow("wf").unwrap();
        let loaded = storage.load_latest_run("wf").unwrap().unwrap();

        // THEN — orphaned
        assert!(loaded.orphaned);
    }

    #[test]
    fn register_workflow_is_idempotent() {
        // GIVEN
        let storage = in_memory_storage();

        // WHEN — register same workflow twice
        storage.register_workflow("wf").unwrap();
        storage.register_workflow("wf").unwrap();

        // THEN — no error, still one entry
        let conn = storage.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM workflows WHERE name='wf'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
