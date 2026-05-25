//! Self-update logic: probe GitHub Releases, cache the result, perform the
//! atomic binary swap.
//!
//! The probe runs once at daemon startup (fire-and-forget) and writes its
//! result to `<data-dir>/update.json` so the CLI's `otter status` and the TUI
//! status bar can read it without re-hitting GitHub. Failure modes (network,
//! parse errors) are silent — the daemon must never block on this.

pub mod cli;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::atomic_write;

pub const REPO_OWNER: &str = "Checkmk";
pub const REPO_NAME: &str = "otter";
pub const TARGET_TRIPLE: &str = "x86_64-unknown-linux-gnu";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str = concat!("otter/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub release_url: String,
}

/// Path of the cache file inside the otter data dir.
pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("update.json")
}

/// Read the cache. Returns `None` if the file is missing, empty, or unparsable
/// (a corrupt cache must not break the CLI).
pub fn read_cache(data_dir: &Path) -> Option<UpdateInfo> {
    let bytes = std::fs::read(cache_path(data_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write or clear the cache. `Some` records an update; `None` removes the file
/// so a stale "update available" never lingers after a successful upgrade.
pub fn write_cache(data_dir: &Path, info: Option<&UpdateInfo>) -> anyhow::Result<()> {
    let path = cache_path(data_dir);
    match info {
        Some(info) => {
            std::fs::create_dir_all(data_dir).context("create data dir")?;
            atomic_write(&path, &serde_json::to_vec(info)?)
        }
        None => {
            if path.exists() {
                std::fs::remove_file(&path).context("remove update cache")?;
            }
            Ok(())
        }
    }
}

/// Minimal subset of the GitHub Releases API response.
#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseJson {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
}

/// Source of release metadata. Behind a trait so tests can inject canned JSON
/// without standing up an HTTP server.
#[async_trait::async_trait]
pub trait ReleaseFetcher: Send + Sync {
    async fn fetch_latest(&self) -> anyhow::Result<ReleaseJson>;
}

pub struct GithubReleaseFetcher {
    base_url: String,
}

impl GithubReleaseFetcher {
    pub fn new() -> Self {
        let base = std::env::var("OTTER_UPDATE_REPO_URL")
            .unwrap_or_else(|_| "https://api.github.com".to_string());
        Self { base_url: base }
    }
}

impl Default for GithubReleaseFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ReleaseFetcher for GithubReleaseFetcher {
    async fn fetch_latest(&self) -> anyhow::Result<ReleaseJson> {
        let url = format!(
            "{}/repos/{}/{}/releases/latest",
            self.base_url, REPO_OWNER, REPO_NAME
        );
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        let resp = client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json::<ReleaseJson>().await?)
    }
}

/// Compare two versions; return the latest tag if it is strictly greater than
/// `current`. Stable installs skip pre-releases (e.g. `0.2.0-rc.1`).
pub fn is_newer(current: &str, latest_tag: &str) -> Option<String> {
    let current_v = semver::Version::parse(current.trim_start_matches('v')).ok()?;
    let latest_v = semver::Version::parse(latest_tag.trim_start_matches('v')).ok()?;
    if !latest_v.pre.is_empty() && current_v.pre.is_empty() {
        return None;
    }
    if latest_v > current_v {
        Some(latest_v.to_string())
    } else {
        None
    }
}

/// Returns `Some(UpdateInfo)` only when a strictly-newer stable release exists.
pub async fn check_latest(
    fetcher: &dyn ReleaseFetcher,
    current: &str,
) -> anyhow::Result<Option<UpdateInfo>> {
    let rel = fetcher.fetch_latest().await?;
    if rel.draft || rel.prerelease {
        return Ok(None);
    }
    let Some(latest) = is_newer(current, &rel.tag_name) else {
        return Ok(None);
    };
    Ok(Some(UpdateInfo {
        current: current.to_string(),
        latest,
        release_url: rel.html_url,
    }))
}

/// Download the latest release tarball, atomically swap the running binary.
/// Blocking — the caller must run it inside `spawn_blocking` to avoid stalling
/// the tokio reactor.
pub fn perform_update(current_version: &str) -> anyhow::Result<self_update::Status> {
    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("otter")
        .target(TARGET_TRIPLE)
        .show_download_progress(true)
        .current_version(current_version)
        .no_confirm(true)
        .build()
        .context("build self_update updater")?
        .update()
        .context("perform self-update")?;
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_strict_greater() {
        assert_eq!(is_newer("0.1.0", "v0.2.0").as_deref(), Some("0.2.0"));
        assert_eq!(is_newer("0.1.0", "v0.1.1").as_deref(), Some("0.1.1"));
    }

    #[test]
    fn is_newer_returns_none_for_equal_or_older() {
        assert!(is_newer("0.1.0", "v0.1.0").is_none());
        assert!(is_newer("0.2.0", "v0.1.5").is_none());
    }

    #[test]
    fn stable_install_skips_prereleases() {
        assert!(is_newer("0.1.0", "v0.2.0-rc.1").is_none());
        assert!(is_newer("0.1.0", "v0.2.0-alpha").is_none());
    }

    #[test]
    fn prerelease_install_accepts_newer_prereleases() {
        assert_eq!(
            is_newer("0.1.0-alpha", "v0.2.0-rc.1").as_deref(),
            Some("0.2.0-rc.1")
        );
    }

    #[test]
    fn is_newer_handles_unprefixed_tags() {
        assert_eq!(is_newer("0.1.0", "0.2.0").as_deref(), Some("0.2.0"));
    }

    #[test]
    fn is_newer_rejects_garbage() {
        assert!(is_newer("not-semver", "v0.2.0").is_none());
        assert!(is_newer("0.1.0", "not-semver").is_none());
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let info = UpdateInfo {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            release_url: "https://github.com/Checkmk/otter/releases/tag/v0.2.0".into(),
        };
        write_cache(dir.path(), Some(&info)).unwrap();
        assert_eq!(read_cache(dir.path()), Some(info));
    }

    #[test]
    fn cache_clear_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let info = UpdateInfo {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            release_url: "u".into(),
        };
        write_cache(dir.path(), Some(&info)).unwrap();
        write_cache(dir.path(), None).unwrap();
        assert_eq!(read_cache(dir.path()), None);
    }

    #[test]
    fn cache_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_cache(dir.path()), None);
    }

    #[test]
    fn cache_returns_none_for_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(cache_path(dir.path()), b"{ not json").unwrap();
        assert_eq!(read_cache(dir.path()), None);
    }

    struct StubFetcher(ReleaseJson);

    #[async_trait::async_trait]
    impl ReleaseFetcher for StubFetcher {
        async fn fetch_latest(&self) -> anyhow::Result<ReleaseJson> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn check_latest_reports_newer_stable() {
        let stub = StubFetcher(ReleaseJson {
            tag_name: "v0.2.0".into(),
            html_url: "https://github.com/Checkmk/otter/releases/tag/v0.2.0".into(),
            prerelease: false,
            draft: false,
        });
        let info = check_latest(&stub, "0.1.0").await.unwrap();
        assert_eq!(info.unwrap().latest, "0.2.0");
    }

    #[tokio::test]
    async fn check_latest_skips_drafts_and_prereleases() {
        let prerelease = StubFetcher(ReleaseJson {
            tag_name: "v0.2.0".into(),
            html_url: "u".into(),
            prerelease: true,
            draft: false,
        });
        assert!(check_latest(&prerelease, "0.1.0").await.unwrap().is_none());

        let draft = StubFetcher(ReleaseJson {
            tag_name: "v0.2.0".into(),
            html_url: "u".into(),
            prerelease: false,
            draft: true,
        });
        assert!(check_latest(&draft, "0.1.0").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn check_latest_returns_none_when_up_to_date() {
        let stub = StubFetcher(ReleaseJson {
            tag_name: "v0.1.0".into(),
            html_url: "u".into(),
            prerelease: false,
            draft: false,
        });
        assert!(check_latest(&stub, "0.1.0").await.unwrap().is_none());
    }
}
