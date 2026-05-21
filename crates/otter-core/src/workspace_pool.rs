//! Reusable, locked git-worktree slots for the `git` workspace type.
//!
//! A "pool" is a directory under which slot worktrees and lock dirs live:
//!
//! ```text
//! <pool_dir>/
//!   slot-0/              # worktree (checked out)
//!   slot-0.lock/         # lock dir (atomic mkdir-based locking)
//!     timestamp          # epoch seconds; used for stale-lock detection
//!   slot-1/
//!   slot-1.lock/
//!   ...
//! ```
//!
//! `mkdir(lock_dir)` is the lock primitive — atomic on both POSIX
//! (`mkdir(2)` returns `EEXIST`) and Windows NTFS (`CreateDirectoryW` returns
//! `ERROR_ALREADY_EXISTS`). `std::fs::create_dir` maps both to
//! `io::ErrorKind::AlreadyExists`, so the same code is cross-platform.
//!
//! Slots are grown on demand: an acquire scans `slot-0..slot-N-1`; if all are
//! locked and non-stale, it creates `slot-N`. Stale locks (older than
//! [`STALE_LOCK_SECS`]) are broken and reclaimed.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

use crate::process::inject_isolated_env;

const STALE_LOCK_SECS: u64 = 24 * 60 * 60;
/// Safety cap on pool growth — protects against unbounded mkdir loops.
const MAX_SLOTS: usize = 1024;

/// Acquires a worktree slot from the pool and prepares it at `git_ref`.
///
/// Locking is per-slot via `mkdir <slot>.lock`. The slot's worktree is either
/// created (`git worktree add`) on first use or reset (`checkout --detach`,
/// `reset --hard`, `clean -fd`) on reuse.
///
/// On any error after the lock is acquired, the lock is released before
/// returning so a transient failure doesn't permanently consume a slot.
pub async fn acquire_pool_slot(
    pool_dir: &Path,
    base_repo: &Path,
    git_ref: &str,
) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(pool_dir)
        .map_err(|e| anyhow::anyhow!("cannot create pool dir '{}': {}", pool_dir.display(), e))?;

    let (slot_path, lock_path) = lock_a_slot(pool_dir)?;
    info!(slot = %slot_path.display(), "Acquired git pool slot");

    // From here on, release the lock on any failure so the slot is recoverable.
    match prepare_slot(&slot_path, base_repo, git_ref).await {
        Ok(()) => Ok(slot_path),
        Err(e) => {
            // Best-effort release; log but surface the original error.
            if let Err(rel_err) = std::fs::remove_dir_all(&lock_path) {
                warn!(
                    lock = %lock_path.display(),
                    error = %rel_err,
                    "Failed to release lock after prepare_slot failure"
                );
            }
            Err(e)
        }
    }
}

/// Releases a slot's lock. The slot's worktree is left in place for reuse.
pub async fn release_pool_slot(slot_path: &Path) -> anyhow::Result<()> {
    let lock_path = lock_path_for(slot_path)?;
    std::fs::remove_dir_all(&lock_path)
        .map_err(|e| anyhow::anyhow!("failed to release lock '{}': {}", lock_path.display(), e))?;
    info!(slot = %slot_path.display(), "Released git pool slot");
    Ok(())
}

fn lock_path_for(slot_path: &Path) -> anyhow::Result<PathBuf> {
    let file_name = slot_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("slot path has no filename: {}", slot_path.display()))?
        .to_string_lossy()
        .into_owned();
    let parent = slot_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("slot path has no parent: {}", slot_path.display()))?;
    Ok(parent.join(format!("{}.lock", file_name)))
}

/// Scans `slot-0`, `slot-1`, ... and atomically claims the first free one (or
/// creates a new slot beyond the highest seen). Returns `(slot_dir, lock_dir)`.
fn lock_a_slot(pool_dir: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    let mut n = 0usize;
    let mut already_broke_stale = false;
    loop {
        if n >= MAX_SLOTS {
            return Err(anyhow::anyhow!(
                "pool '{}' exhausted (>={} slots)",
                pool_dir.display(),
                MAX_SLOTS
            ));
        }
        let slot_path = pool_dir.join(format!("slot-{n}"));
        let lock_path = pool_dir.join(format!("slot-{n}.lock"));
        match std::fs::create_dir(&lock_path) {
            Ok(()) => {
                write_lock_timestamp(&lock_path)?;
                return Ok((slot_path, lock_path));
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                if !already_broke_stale && is_stale_lock(&lock_path) {
                    warn!(lock = %lock_path.display(), "Breaking stale git pool lock");
                    if std::fs::remove_dir_all(&lock_path).is_ok() {
                        already_broke_stale = true;
                        continue;
                    }
                }
                n += 1;
                already_broke_stale = false;
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "failed to create lock '{}': {}",
                    lock_path.display(),
                    e
                ));
            }
        }
    }
}

fn write_lock_timestamp(lock_path: &Path) -> anyhow::Result<()> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {}", e))?
        .as_secs();
    std::fs::write(lock_path.join("timestamp"), ts.to_string()).map_err(|e| {
        anyhow::anyhow!(
            "failed to write lock timestamp '{}': {}",
            lock_path.display(),
            e
        )
    })
}

fn is_stale_lock(lock_path: &Path) -> bool {
    let ts_path = lock_path.join("timestamp");
    let Ok(content) = std::fs::read_to_string(&ts_path) else {
        // Missing or unreadable timestamp → treat as stale (matches existing shell script).
        return true;
    };
    let Ok(ts) = content.trim().parse::<u64>() else {
        return true;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    now.as_secs().saturating_sub(ts) > STALE_LOCK_SECS
}

async fn prepare_slot(slot_path: &Path, base_repo: &Path, git_ref: &str) -> anyhow::Result<()> {
    if slot_path.is_dir() {
        reset_existing_worktree(slot_path, git_ref).await
    } else {
        super::workspace::add_worktree(base_repo, slot_path, git_ref).await
    }
}

async fn reset_existing_worktree(slot_path: &Path, git_ref: &str) -> anyhow::Result<()> {
    run_git(slot_path, &["checkout", "--detach", git_ref]).await?;
    run_git(slot_path, &["reset", "--hard"]).await?;
    run_git(slot_path, &["clean", "-fd"]).await?;
    Ok(())
}

async fn run_git(cwd: &Path, args: &[&str]) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(cwd);
    for a in args {
        cmd.arg(a);
    }
    inject_isolated_env(&mut cmd, &[], true);
    let output = cmd
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("failed to spawn 'git {}': {}", args.join(" "), e))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "'git {}' failed in {} (status {}): {}",
            args.join(" "),
            cwd.display(),
            output.status,
            stderr.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Builds a minimal git repo with one commit. Returns the repo path.
    fn init_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        run_sync(&path, &["init", "--initial-branch=main"]);
        run_sync(&path, &["config", "user.email", "t@t"]);
        run_sync(&path, &["config", "user.name", "t"]);
        std::fs::write(path.join("README.md"), "hi").unwrap();
        run_sync(&path, &["add", "."]);
        run_sync(&path, &["commit", "-m", "init"]);
        (dir, path)
    }

    fn run_sync(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn acquire_creates_first_slot() {
        // GIVEN a pool dir and a base repo
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        // WHEN
        let slot = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN
        assert_eq!(slot, pool.path().join("slot-0"));
        assert!(slot.join("README.md").is_file());
        assert!(pool.path().join("slot-0.lock").is_dir());
    }

    #[tokio::test]
    async fn second_acquire_grows_pool() {
        // GIVEN one slot already locked
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let first = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // WHEN
        let second = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN
        assert_eq!(first, pool.path().join("slot-0"));
        assert_eq!(second, pool.path().join("slot-1"));
        assert!(pool.path().join("slot-0.lock").is_dir());
        assert!(pool.path().join("slot-1.lock").is_dir());
    }

    #[tokio::test]
    async fn release_frees_slot_for_reuse() {
        // GIVEN
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let first = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // WHEN
        release_pool_slot(&first).await.unwrap();
        let second = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN — same slot reused (slot-0)
        assert_eq!(first, second);
        assert!(pool.path().join("slot-0.lock").is_dir());
    }

    #[tokio::test]
    async fn stale_lock_is_broken() {
        // GIVEN a lock with an ancient timestamp
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let stale_lock = pool.path().join("slot-0.lock");
        std::fs::create_dir_all(&stale_lock).unwrap();
        std::fs::write(stale_lock.join("timestamp"), "0").unwrap();
        // WHEN
        let slot = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN — slot-0 was reclaimed
        assert_eq!(slot, pool.path().join("slot-0"));
    }

    #[tokio::test]
    async fn missing_timestamp_treated_as_stale() {
        // GIVEN a lock dir with no timestamp file
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(pool.path().join("slot-0.lock")).unwrap();
        // WHEN
        let slot = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN
        assert_eq!(slot, pool.path().join("slot-0"));
    }

    #[tokio::test]
    async fn reuse_resets_to_ref() {
        // GIVEN a slot with an extra commit applied (dirty/diverged)
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let slot = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // pollute the worktree
        std::fs::write(slot.join("README.md"), "polluted").unwrap();
        std::fs::write(slot.join("junk.txt"), "trash").unwrap();
        release_pool_slot(&slot).await.unwrap();
        // WHEN — acquire again
        let slot2 = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN — content is reset
        assert_eq!(slot, slot2);
        assert_eq!(
            std::fs::read_to_string(slot2.join("README.md")).unwrap(),
            "hi"
        );
        assert!(
            !slot2.join("junk.txt").exists(),
            "untracked files should be cleaned"
        );
    }

    #[tokio::test]
    async fn concurrent_acquires_get_distinct_slots() {
        // GIVEN
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let n = 8;
        // WHEN — spawn N concurrent acquires
        let mut handles = Vec::new();
        for _ in 0..n {
            let pool_path = pool.path().to_path_buf();
            let repo_path = repo.clone();
            handles.push(tokio::spawn(async move {
                acquire_pool_slot(&pool_path, &repo_path, "HEAD")
                    .await
                    .unwrap()
            }));
        }
        let mut slots = Vec::new();
        for h in handles {
            slots.push(h.await.unwrap());
        }
        // THEN — all distinct
        slots.sort();
        slots.dedup();
        assert_eq!(
            slots.len(),
            n,
            "concurrent acquires must return distinct slots"
        );
    }
}
