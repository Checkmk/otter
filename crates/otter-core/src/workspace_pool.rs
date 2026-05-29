//! Reusable, locked git-worktree slots for the `git` workspace type.
//!
//! A "pool" is a directory under which slot worktrees and lock dirs live.
//! Slots are namespaced by base repo so multiple workflows pointing at
//! different repos can share a single `pool.dir` without colliding:
//!
//! ```text
//! <pool_dir>/
//!   <base-repo-ns>/      # one subdir per distinct canonical base_repo path
//!     slot-0/            # worktree (checked out)
//!     slot-0.lock/       # lock dir (atomic mkdir-based locking)
//!       timestamp        # epoch seconds; used for stale-lock detection
//!     slot-1/
//!     slot-1.lock/
//!     ...
//!   <other-repo-ns>/
//!     slot-0/
//!     ...
//! ```
//!
//! `<base-repo-ns>` is `<basename>-<fnv1a-hex>` derived from the canonical
//! base_repo path — stable across otter rebuilds and unambiguous across
//! repos that happen to share a basename.
//!
//! `mkdir(lock_dir)` is the lock primitive — atomic on both POSIX
//! (`mkdir(2)` returns `EEXIST`) and Windows NTFS (`CreateDirectoryW` returns
//! `ERROR_ALREADY_EXISTS`). `std::fs::create_dir` maps both to
//! `io::ErrorKind::AlreadyExists`, so the same code is cross-platform.
//!
//! Slots are grown on demand within a namespace: an acquire scans
//! `slot-0..slot-N-1`; if all are locked and non-stale, it creates `slot-N`.
//! Stale locks (older than [`STALE_LOCK_SECS`]) are broken and reclaimed.

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
pub async fn acquire_pool_slot(
    pool_dir: &Path,
    base_repo: &Path,
    git_ref: &str,
) -> anyhow::Result<PathBuf> {
    let ns_dir = pool_dir.join(repo_namespace(base_repo));
    std::fs::create_dir_all(&ns_dir)
        .map_err(|e| anyhow::anyhow!("cannot create pool dir '{}': {}", ns_dir.display(), e))?;

    let (slot_path, lock_path) = lock_a_slot(&ns_dir)?;
    info!(slot = %slot_path.display(), "Acquired git pool slot");

    // From here on, release the lock on any failure so the slot is recoverable.
    let guard = SlotLockGuard::new(lock_path);
    prepare_slot(&slot_path, base_repo, git_ref).await?;
    guard.defuse();
    Ok(slot_path)
}

/// RAII guard that removes a lock dir on drop unless `defuse()` is called first.
struct SlotLockGuard {
    lock_path: Option<PathBuf>,
}

impl SlotLockGuard {
    fn new(lock_path: PathBuf) -> Self {
        Self {
            lock_path: Some(lock_path),
        }
    }

    /// Disarm the guard — the lock will not be released on drop.
    fn defuse(mut self) {
        self.lock_path.take();
    }
}

impl Drop for SlotLockGuard {
    fn drop(&mut self) {
        if let Some(lock_path) = self.lock_path.take() {
            match std::fs::remove_dir_all(&lock_path) {
                Ok(()) => {
                    info!(lock = %lock_path.display(), "Released git pool slot lock (drop-guard)")
                }
                Err(e) => warn!(
                    lock = %lock_path.display(),
                    error = %e,
                    "Failed to release lock on drop"
                ),
            }
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

pub(crate) fn repo_namespace(base_repo: &Path) -> String {
    let basename = base_repo
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut sanitized = String::with_capacity(basename.len().min(32));
    for c in basename.chars().take(32) {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
        }
    }
    let hash = fnv1a_64(base_repo.as_os_str().to_string_lossy().as_bytes());
    if sanitized.is_empty() {
        format!("{hash:016x}")
    } else {
        format!("{sanitized}-{hash:016x}")
    }
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
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
        return reset_existing_worktree(slot_path, git_ref).await;
    }
    match super::workspace::add_worktree(base_repo, slot_path, git_ref, false).await {
        Err(e) if is_stale_registration(&e) => {
            warn!(slot = %slot_path.display(), "Re-adding worktree over stale registration");
            super::workspace::add_worktree(base_repo, slot_path, git_ref, true).await
        }
        result => result,
    }
}

fn is_stale_registration(error: &anyhow::Error) -> bool {
    error.to_string().contains("already registered")
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
        let ns = pool.path().join(repo_namespace(&repo));
        // WHEN
        let slot = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN
        assert_eq!(slot, ns.join("slot-0"));
        assert!(slot.join("README.md").is_file());
        assert!(ns.join("slot-0.lock").is_dir());
    }

    #[tokio::test]
    async fn distinct_base_repos_get_distinct_namespaces() {
        // GIVEN two different base repos sharing one pool dir
        let (_g1, repo_a) = init_repo();
        let (_g2, repo_b) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        // WHEN
        let slot_a = acquire_pool_slot(pool.path(), &repo_a, "HEAD")
            .await
            .unwrap();
        let slot_b = acquire_pool_slot(pool.path(), &repo_b, "HEAD")
            .await
            .unwrap();
        // THEN — both got slot-0 under their own namespace subdir
        assert_ne!(slot_a.parent().unwrap(), slot_b.parent().unwrap());
        assert_eq!(slot_a.file_name().unwrap(), "slot-0");
        assert_eq!(slot_b.file_name().unwrap(), "slot-0");
        assert_eq!(slot_a.parent().unwrap().parent().unwrap(), pool.path());
        assert_eq!(slot_b.parent().unwrap().parent().unwrap(), pool.path());
    }

    #[tokio::test]
    async fn same_base_repo_shares_namespace() {
        // GIVEN one base repo and two acquires
        let (_g, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        // WHEN
        let s0 = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        let s1 = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN — same namespace dir, distinct slot indices
        assert_eq!(s0.parent(), s1.parent());
        assert_eq!(s0.file_name().unwrap(), "slot-0");
        assert_eq!(s1.file_name().unwrap(), "slot-1");
    }

    #[test]
    fn repo_namespace_is_stable_and_unique() {
        let a = PathBuf::from("/home/user/check_mk");
        let b = PathBuf::from("/home/user/other/check_mk");
        // Same input → same output
        assert_eq!(repo_namespace(&a), repo_namespace(&a));
        // Same basename, different parent → different namespace
        assert_ne!(repo_namespace(&a), repo_namespace(&b));
        // Namespace embeds the basename for debuggability
        assert!(repo_namespace(&a).starts_with("check_mk-"));
    }

    #[tokio::test]
    async fn second_acquire_grows_pool() {
        // GIVEN one slot already locked
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let ns = pool.path().join(repo_namespace(&repo));
        let first = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // WHEN
        let second = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN
        assert_eq!(first, ns.join("slot-0"));
        assert_eq!(second, ns.join("slot-1"));
        assert!(ns.join("slot-0.lock").is_dir());
        assert!(ns.join("slot-1.lock").is_dir());
    }

    #[tokio::test]
    async fn release_frees_slot_for_reuse() {
        // GIVEN
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let ns = pool.path().join(repo_namespace(&repo));
        let first = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // WHEN
        release_pool_slot(&first).await.unwrap();
        let second = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN — same slot reused (slot-0)
        assert_eq!(first, second);
        assert!(ns.join("slot-0.lock").is_dir());
    }

    #[tokio::test]
    async fn stale_lock_is_broken() {
        // GIVEN a lock with an ancient timestamp
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let ns = pool.path().join(repo_namespace(&repo));
        let stale_lock = ns.join("slot-0.lock");
        std::fs::create_dir_all(&stale_lock).unwrap();
        std::fs::write(stale_lock.join("timestamp"), "0").unwrap();
        // WHEN
        let slot = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN — slot-0 was reclaimed
        assert_eq!(slot, ns.join("slot-0"));
    }

    #[tokio::test]
    async fn missing_timestamp_treated_as_stale() {
        // GIVEN a lock dir with no timestamp file
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let ns = pool.path().join(repo_namespace(&repo));
        std::fs::create_dir_all(ns.join("slot-0.lock")).unwrap();
        // WHEN
        let slot = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        // THEN
        assert_eq!(slot, ns.join("slot-0"));
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

    #[test]
    fn slot_lock_guard_releases_on_drop() {
        // GIVEN — a lock dir exists
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("slot-0.lock");
        std::fs::create_dir(&lock).unwrap();
        // WHEN — a guard wraps it and is dropped without defuse
        {
            let _g = SlotLockGuard::new(lock.clone());
        }
        // THEN — the lock dir is gone
        assert!(!lock.exists());
    }

    #[test]
    fn slot_lock_guard_defuse_keeps_lock() {
        // GIVEN — a lock dir exists and is wrapped in a guard
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("slot-0.lock");
        std::fs::create_dir(&lock).unwrap();
        let g = SlotLockGuard::new(lock.clone());
        // WHEN — the guard is defused
        g.defuse();
        // THEN — the lock dir is untouched
        assert!(lock.is_dir());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_acquire_releases_lock() {
        // GIVEN — a base repo and an empty pool
        let (_repo_guard, repo) = init_repo();
        let pool = tempfile::tempdir().unwrap();
        let ns = pool.path().join(repo_namespace(&repo));
        let lock_dir = ns.join("slot-0.lock");

        // WHEN — acquire is cancelled (future dropped) right after the lock is
        // taken but before prepare_slot finishes
        {
            let acquire_fut = acquire_pool_slot(pool.path(), &repo, "HEAD");
            tokio::pin!(acquire_fut);
            tokio::select! {
                biased;
                _ = &mut acquire_fut => {
                    // If prepare_slot is fast enough to win the race, the lock
                    // remains held on success — release it so the assertion below
                    // still exercises drop-on-cancel behavior on retries.
                    release_pool_slot(&ns.join("slot-0")).await.unwrap();
                }
                _ = async {
                    while !lock_dir.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                } => {
                    // Lock is now held; drop acquire_fut by leaving this select.
                }
            }
            // acquire_fut drops at end of this scope → SlotLockGuard::drop runs.
        }

        // THEN — the lock dir is gone (released either by drop-guard or by the
        // explicit release_pool_slot above)
        assert!(
            !lock_dir.exists(),
            "cancelled acquire must release the slot lock"
        );
        // And a fresh acquire reuses slot-0 rather than growing the pool.
        let next = acquire_pool_slot(pool.path(), &repo, "HEAD").await.unwrap();
        assert_eq!(next, ns.join("slot-0"));
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
