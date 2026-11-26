use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use tracing_appender::rolling::{RollingFileAppender, Rotation};

use crate::config::Retention;

const PREFIX: &str = "daemon";
const SUFFIX: &str = "log";
const LEGACY_LOG: &str = "daemon.log";

fn logs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

fn daily_log_name(date: DateTime<Utc>) -> String {
    format!("{PREFIX}.{}.{SUFFIX}", date.format("%Y-%m-%d"))
}

pub fn appender(data_dir: &Path, retention: Retention) -> anyhow::Result<RollingFileAppender> {
    let logs_dir = logs_dir(data_dir);
    std::fs::create_dir_all(&logs_dir).context("create daemon log dir")?;
    let mut builder = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(PREFIX)
        .filename_suffix(SUFFIX);
    if let Retention::Days(days) = retention {
        let todays_log = 1;
        builder = builder.max_log_files(days as usize + todays_log);
    }
    builder.build(&logs_dir).context("create daemon log")
}

pub fn adopt_legacy_log(data_dir: &Path) -> anyhow::Result<()> {
    let legacy = data_dir.join(LEGACY_LOG);
    let Ok(modified) = legacy.metadata().and_then(|m| m.modified()) else {
        return Ok(());
    };
    let logs_dir = logs_dir(data_dir);
    std::fs::create_dir_all(&logs_dir).context("create daemon log dir")?;
    let target = logs_dir.join(daily_log_name(modified.into()));
    if target.exists() {
        return Ok(());
    }
    std::fs::rename(&legacy, &target).with_context(|| {
        format!(
            "Failed to move {} to {}",
            legacy.display(),
            target.display()
        )
    })
}

pub fn latest(data_dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(logs_dir(data_dir))
        .ok()?
        .flatten()
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .max()
        .map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::io::Write;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn days_ago(days: u64) -> SystemTime {
        SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60)
    }

    fn name_days_ago(days: u64) -> String {
        daily_log_name(days_ago(days).into())
    }

    fn write_file(path: &Path, content: &str, modified: SystemTime) {
        std::fs::write(path, content).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    fn create_daily_logs_oldest_first(logs_dir: &Path, ages_in_days: &[u64]) {
        std::fs::create_dir_all(logs_dir).unwrap();
        for &age in ages_in_days {
            write_file(&logs_dir.join(name_days_ago(age)), "old\n", days_ago(age));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn file_names(dir: &Path) -> HashSet<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect()
    }

    #[test]
    fn latest_picks_most_recently_written_daemon_log() {
        // GIVEN
        let dir = TempDir::new().unwrap();
        create_daily_logs_oldest_first(&logs_dir(dir.path()), &[2, 1]);

        // WHEN / THEN
        assert_eq!(
            latest(dir.path()),
            Some(logs_dir(dir.path()).join(name_days_ago(1)))
        );
    }

    #[test]
    fn latest_is_none_without_logs() {
        // GIVEN
        let dir = TempDir::new().unwrap();

        // WHEN / THEN
        assert_eq!(latest(dir.path()), None);
    }

    #[test]
    fn appender_writes_to_latest_log() {
        // GIVEN
        let dir = TempDir::new().unwrap();
        create_daily_logs_oldest_first(&logs_dir(dir.path()), &[1]);

        // WHEN
        let mut writer = appender(dir.path(), Retention::Days(30)).unwrap();
        writer.write_all(b"fresh\n").unwrap();

        // THEN
        let latest = latest(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(latest).unwrap(), "fresh\n");
    }

    #[test]
    fn legacy_log_moves_into_daily_logs() {
        // GIVEN a log written before daily rotation existed
        let dir = TempDir::new().unwrap();
        write_file(&dir.path().join(LEGACY_LOG), "legacy\n", days_ago(3));

        // WHEN
        adopt_legacy_log(dir.path()).unwrap();

        // THEN it is kept under the date it was last written
        assert!(!dir.path().join(LEGACY_LOG).exists());
        let adopted = logs_dir(dir.path()).join(name_days_ago(3));
        assert_eq!(std::fs::read_to_string(adopted).unwrap(), "legacy\n");
    }

    #[test]
    fn appender_keeps_todays_log_and_one_earlier_log_per_retention_day() {
        // GIVEN
        let dir = TempDir::new().unwrap();
        create_daily_logs_oldest_first(&logs_dir(dir.path()), &[4, 3, 2, 1]);

        // WHEN
        appender(dir.path(), Retention::Days(2)).unwrap();

        // THEN
        assert_eq!(
            file_names(&logs_dir(dir.path())),
            HashSet::from([name_days_ago(2), name_days_ago(1), name_days_ago(0)])
        );
    }

    #[test]
    fn appender_keeps_todays_log_across_restarts() {
        // GIVEN the log written before a same-day restart
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(logs_dir(dir.path())).unwrap();
        let today = logs_dir(dir.path()).join(name_days_ago(0));
        std::fs::write(&today, "before restart\n").unwrap();

        // WHEN
        appender(dir.path(), Retention::Days(1)).unwrap();

        // THEN
        assert_eq!(std::fs::read_to_string(today).unwrap(), "before restart\n");
    }

    #[test]
    fn appender_keeps_all_logs_without_retention() {
        // GIVEN
        let dir = TempDir::new().unwrap();
        create_daily_logs_oldest_first(&logs_dir(dir.path()), &[365, 1]);

        // WHEN
        appender(dir.path(), Retention::KeepForever).unwrap();

        // THEN
        assert_eq!(
            file_names(&logs_dir(dir.path())),
            HashSet::from([name_days_ago(365), name_days_ago(1), name_days_ago(0)])
        );
    }
}
