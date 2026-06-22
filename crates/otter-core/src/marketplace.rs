//! Workflow marketplaces: registries of installable workflow packages hosted
//! in a git repo, browsed and installed via the `otter marketplace` and
//! `otter workflow install <name>@<marketplace>` commands.

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::types::WorkflowDef;

// ─── Registry (~/.config/otter/marketplaces.toml) ──────────────────────────

/// A registered marketplace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Marketplace {
    pub name: String,
    pub url: String,
    pub added_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    marketplace: Vec<Marketplace>,
}

/// Returns the path to the marketplace registry under `config_dir`.
pub fn registry_path(config_dir: &Path) -> PathBuf {
    config_dir.join("marketplaces.toml")
}

pub fn load_registry(config_dir: &Path) -> anyhow::Result<Vec<Marketplace>> {
    let path = registry_path(config_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let parsed: RegistryFile =
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(parsed.marketplace)
}

pub fn save_registry(config_dir: &Path, marketplaces: &[Marketplace]) -> anyhow::Result<()> {
    let path = registry_path(config_dir);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let file = RegistryFile {
        marketplace: marketplaces.to_vec(),
    };
    let raw = toml::to_string_pretty(&file)?;
    std::fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

// ─── Per-marketplace state (data_dir/marketplaces/<name>.state.json) ───────

/// Fetch metadata + last-known upstream `version` per workflow path.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MarketplaceState {
    pub last_fetched_at: Option<DateTime<Utc>>,
    /// Map: workflow package path (relative to marketplace root) → version string.
    /// `None` value means the workflow had no `version` field.
    #[serde(default)]
    pub known_versions: HashMap<String, Option<String>>,
}

pub fn marketplaces_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("marketplaces")
}

pub fn clone_dir(data_dir: &Path, name: &str) -> PathBuf {
    marketplaces_dir(data_dir).join(name)
}

pub fn state_path(data_dir: &Path, name: &str) -> PathBuf {
    marketplaces_dir(data_dir).join(format!("{name}.state.json"))
}

pub fn load_state(data_dir: &Path, name: &str) -> anyhow::Result<MarketplaceState> {
    let path = state_path(data_dir, name);
    if !path.exists() {
        return Ok(MarketplaceState::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let state: MarketplaceState = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(state)
}

pub fn save_state(data_dir: &Path, name: &str, state: &MarketplaceState) -> anyhow::Result<()> {
    let path = state_path(data_dir, name);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let raw = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

// ─── Marketplace index file (.otter-marketplace.toml at repo root) ──────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct IndexEntry {
    pub path: String,
    #[serde(default)]
    pub wip: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MarketplaceIndex {
    pub schema: u32,
    pub name: String,
    #[serde(default, rename = "workflow")]
    pub workflows: Vec<IndexEntry>,
}

/// Validates a marketplace name: non-empty, ASCII alphanumeric / `-` / `_`,
/// and not starting with a dash. Mirrors the shape accepted by
/// `parse_marketplace_ref` in the CLI.
pub fn validate_marketplace_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!name.is_empty(), "marketplace name must not be empty");
    anyhow::ensure!(
        !name.starts_with('-'),
        "marketplace name '{name}' must not start with '-'"
    );
    anyhow::ensure!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "marketplace name '{name}' may only contain ASCII letters, digits, '-' and '_'"
    );
    Ok(())
}

/// Parse `.otter-marketplace.toml` from a marketplace clone.
pub fn load_index(clone_dir: &Path) -> anyhow::Result<MarketplaceIndex> {
    let path = clone_dir.join(".otter-marketplace.toml");
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "marketplace index not found at {} — not a valid marketplace",
            path.display()
        )
    })?;
    let parsed: MarketplaceIndex =
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
    if parsed.schema != 1 {
        anyhow::bail!(
            "marketplace at {} uses unsupported schema {}; this otter supports schema 1",
            clone_dir.display(),
            parsed.schema
        );
    }
    validate_marketplace_name(&parsed.name)
        .with_context(|| format!("invalid marketplace name in {}", path.display()))?;
    Ok(parsed)
}

/// Parse the `workflow.toml` of a package inside a marketplace clone, returning
/// the lightweight metadata used for listing & resolution. Doesn't validate
/// the workflow (callers do that via `validate_workflow` when installing).
pub fn read_package_def(clone_dir: &Path, rel_path: &str) -> anyhow::Result<WorkflowDef> {
    let wf_path = clone_dir.join(rel_path).join("workflow.toml");
    let raw = std::fs::read_to_string(&wf_path)
        .with_context(|| format!("failed to read {}", wf_path.display()))?;
    let def: WorkflowDef =
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", wf_path.display()))?;
    Ok(def)
}

/// Resolve `<name>@<marketplace>` to a package directory inside the marketplace
/// clone. Errors if the workflow is unknown, marked `wip`, or has no matching
/// `name`.
pub fn resolve_workflow_in_marketplace(
    data_dir: &Path,
    marketplace_name: &str,
    workflow_name: &str,
) -> anyhow::Result<PathBuf> {
    let clone = clone_dir(data_dir, marketplace_name);
    if !clone.is_dir() {
        anyhow::bail!(
            "marketplace '{marketplace_name}' is not registered (no clone at {})",
            clone.display()
        );
    }
    let index = load_index(&clone)?;
    for entry in &index.workflows {
        if entry.wip {
            continue;
        }
        let Ok(def) = read_package_def(&clone, &entry.path) else {
            continue;
        };
        if def.name == workflow_name {
            return Ok(clone.join(&entry.path));
        }
    }
    anyhow::bail!("workflow '{workflow_name}' not found in marketplace '{marketplace_name}'")
}

// ─── Origin sidecar (.otter-state/origin.toml) ─────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    pub marketplace: String,
    pub path: String,
    #[serde(default)]
    pub installed_version: Option<String>,
}

pub fn origin_path(workflow_pkg_dir: &Path) -> PathBuf {
    workflow_pkg_dir.join(".otter-state").join("origin.toml")
}

pub fn load_origin(workflow_pkg_dir: &Path) -> anyhow::Result<Option<Origin>> {
    let path = origin_path(workflow_pkg_dir);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let origin: Origin =
        toml::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(origin))
}

pub fn save_origin(workflow_pkg_dir: &Path, origin: &Origin) -> anyhow::Result<()> {
    let path = origin_path(workflow_pkg_dir);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let raw = toml::to_string_pretty(origin)?;
    std::fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

// ─── git shell-outs ─────────────────────────────────────────────────────────

/// `git clone <url> <dest>`.
pub async fn clone_marketplace(url: &str, dest: &Path) -> anyhow::Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("clone").arg(url).arg(dest);
    let output = cmd
        .output()
        .await
        .with_context(|| format!("failed to spawn 'git clone {url}'"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "'git clone' failed (status {}): {}",
            output.status,
            stderr.trim()
        );
    }
    Ok(())
}

/// `git -C <clone> fetch && git -C <clone> reset --hard @{u}`.
///
/// Idempotent: aligns the clone to the upstream tip on its current branch.
/// Designed for the daemon's periodic fetch task — local edits in the clone
/// are intentionally clobbered (the clone is purely a cache, not a workspace).
pub async fn fetch_marketplace(clone: &Path) -> anyhow::Result<()> {
    let fetch = Command::new("git")
        .arg("-C")
        .arg(clone)
        .arg("fetch")
        .arg("--quiet")
        .output()
        .await
        .with_context(|| format!("failed to spawn 'git fetch' in {}", clone.display()))?;
    if !fetch.status.success() {
        let stderr = String::from_utf8_lossy(&fetch.stderr);
        anyhow::bail!("'git fetch' failed: {}", stderr.trim());
    }
    let reset = Command::new("git")
        .arg("-C")
        .arg(clone)
        .arg("reset")
        .arg("--hard")
        .arg("@{u}")
        .output()
        .await
        .with_context(|| format!("failed to spawn 'git reset' in {}", clone.display()))?;
    if !reset.status.success() {
        let stderr = String::from_utf8_lossy(&reset.stderr);
        anyhow::bail!("'git reset --hard @{{u}}' failed: {}", stderr.trim());
    }
    Ok(())
}

pub async fn refresh_marketplace(data_dir: &Path, name: &str) -> anyhow::Result<()> {
    let clone = clone_dir(data_dir, name);
    fetch_marketplace(&clone).await?;
    refresh_state_from_clone(data_dir, name)?;
    Ok(())
}

/// Refresh the per-marketplace `known_versions` map by reading the current
/// state of every (non-wip) workflow package in the clone. Updates `last_fetched_at`.
pub fn refresh_state_from_clone(data_dir: &Path, name: &str) -> anyhow::Result<MarketplaceState> {
    let clone = clone_dir(data_dir, name);
    let mut state = load_state(data_dir, name).unwrap_or_default();
    let index = load_index(&clone)?;
    let mut known: HashMap<String, Option<String>> = HashMap::new();
    for entry in &index.workflows {
        if entry.wip {
            continue;
        }
        match read_package_def(&clone, &entry.path) {
            Ok(def) => {
                known.insert(entry.path.clone(), def.version);
            }
            Err(e) => {
                tracing::warn!(
                    marketplace = %name,
                    path = %entry.path,
                    error = %e,
                    "Skipping unreadable workflow package"
                );
            }
        }
    }
    state.known_versions = known;
    state.last_fetched_at = Some(Utc::now());
    save_state(data_dir, name, &state)?;
    Ok(state)
}

// ─── Update detection ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAvailable {
    pub workflow_name: String,
    pub marketplace: String,
    pub installed: Option<String>,
    pub latest: Option<String>,
}

/// Scan every installed workflow under `workflows_dir`. For each one with an
/// `origin.toml` pointing at a registered marketplace, compare its
/// `installed_version` against the marketplace's last-known upstream version.
/// Returns one entry per workflow whose versions differ.
pub fn compute_updates(workflows_dir: &Path, data_dir: &Path) -> Vec<UpdateAvailable> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(workflows_dir) else {
        return out;
    };
    // Cache state per marketplace to avoid re-reading the JSON for every workflow.
    let mut state_cache: HashMap<String, Option<MarketplaceState>> = HashMap::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(Some(origin)) = load_origin(&path) else {
            continue;
        };
        let state = state_cache
            .entry(origin.marketplace.clone())
            .or_insert_with(|| load_state(data_dir, &origin.marketplace).ok());
        let Some(state) = state else { continue };
        let latest = match state.known_versions.get(&origin.path) {
            Some(v) => v.clone(),
            None => continue, // marketplace no longer lists this workflow
        };
        if latest != origin.installed_version {
            let workflow_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            out.push(UpdateAvailable {
                workflow_name,
                marketplace: origin.marketplace,
                installed: origin.installed_version,
                latest,
            });
        }
    }
    out
}

/// Returns workflows whose `origin.toml` references a marketplace that
/// no longer exists in the registry (orphaned by `marketplace remove`).
pub fn dangling_origins(
    workflows_dir: &Path,
    registered_names: &[String],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(workflows_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(Some(origin)) = load_origin(&path) else {
            continue;
        };
        if !registered_names.iter().any(|s| s == &origin.marketplace) {
            let workflow_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            out.push((workflow_name, origin.marketplace));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_workflow(dir: &Path, name: &str, version: Option<&str>) {
        std::fs::create_dir_all(dir).unwrap();
        let mut toml = format!("name = \"{name}\"\ntype = \"looping\"\nschema = 1\n");
        if let Some(v) = version {
            toml.push_str(&format!("version = \"{v}\"\n"));
        }
        toml.push_str(
            "[[steps]]\n\
             type = \"shell\"\n\
             command = [\"echo\", \"hi\"]\n",
        );
        std::fs::write(dir.join("workflow.toml"), toml).unwrap();
    }

    #[test]
    fn load_index_rejects_missing_name() {
        let dir = TempDir::new().unwrap();
        let clone = dir.path();
        std::fs::write(
            clone.join(".otter-marketplace.toml"),
            "schema = 1\n[[workflow]]\npath = \"workflows/a\"\n",
        )
        .unwrap();
        let err = load_index(clone).unwrap_err();
        assert!(err.to_string().contains(".otter-marketplace.toml"));
    }

    #[test]
    fn load_index_rejects_invalid_name() {
        let dir = TempDir::new().unwrap();
        let clone = dir.path();
        std::fs::write(
            clone.join(".otter-marketplace.toml"),
            "schema = 1\nname = \"has space\"\n[[workflow]]\npath = \"workflows/a\"\n",
        )
        .unwrap();
        let err = load_index(clone).unwrap_err();
        assert!(
            err.chain()
                .any(|e| e.to_string().contains("may only contain")),
            "unexpected error chain: {err:?}"
        );
    }

    #[test]
    fn index_parsing_with_wip_filter() {
        // GIVEN a marketplace clone with two workflows, one wip
        let dir = TempDir::new().unwrap();
        let clone = dir.path();
        std::fs::write(
            clone.join(".otter-marketplace.toml"),
            r#"
schema = 1
name = "test-shop"
[[workflow]]
path = "workflows/a"
[[workflow]]
path = "workflows/b"
wip = true
"#,
        )
        .unwrap();
        write_workflow(&clone.join("workflows/a"), "wf-a", Some("1.0.0"));
        write_workflow(&clone.join("workflows/b"), "wf-b", Some("0.1.0"));

        // WHEN parsing the index
        let index = load_index(clone).unwrap();

        // THEN both entries are present but consumers can filter on `wip`
        assert_eq!(index.workflows.len(), 2);
        assert!(!index.workflows[0].wip);
        assert!(index.workflows[1].wip);
    }

    #[test]
    fn origin_file_round_trip() {
        // GIVEN
        let dir = TempDir::new().unwrap();
        let origin = Origin {
            marketplace: "official".to_string(),
            path: "workflows/x".to_string(),
            installed_version: Some("1.2.3".to_string()),
        };
        // WHEN
        save_origin(dir.path(), &origin).unwrap();
        let loaded = load_origin(dir.path()).unwrap().unwrap();
        // THEN
        assert_eq!(loaded, origin);
    }

    #[test]
    fn compute_updates_reports_version_bump() {
        // GIVEN one installed workflow at v1.0.0 with marketplace state at v2.0.0
        let dir = TempDir::new().unwrap();
        let workflows_dir = dir.path().join("workflows");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        let pkg = workflows_dir.join("my-wf");
        write_workflow(&pkg, "my-wf", Some("1.0.0"));
        save_origin(
            &pkg,
            &Origin {
                marketplace: "shop".to_string(),
                path: "workflows/my".to_string(),
                installed_version: Some("1.0.0".to_string()),
            },
        )
        .unwrap();

        let mut state = MarketplaceState::default();
        state
            .known_versions
            .insert("workflows/my".to_string(), Some("2.0.0".to_string()));
        save_state(&data_dir, "shop", &state).unwrap();

        // WHEN
        let updates = compute_updates(&workflows_dir, &data_dir);

        // THEN
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].workflow_name, "my-wf");
        assert_eq!(updates[0].installed.as_deref(), Some("1.0.0"));
        assert_eq!(updates[0].latest.as_deref(), Some("2.0.0"));
    }

    #[test]
    fn compute_updates_silent_when_versions_match() {
        // GIVEN matching versions
        let dir = TempDir::new().unwrap();
        let workflows_dir = dir.path().join("workflows");
        let data_dir = dir.path().join("data");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        let pkg = workflows_dir.join("my-wf");
        write_workflow(&pkg, "my-wf", Some("1.0.0"));
        save_origin(
            &pkg,
            &Origin {
                marketplace: "shop".to_string(),
                path: "workflows/my".to_string(),
                installed_version: Some("1.0.0".to_string()),
            },
        )
        .unwrap();
        let mut state = MarketplaceState::default();
        state
            .known_versions
            .insert("workflows/my".to_string(), Some("1.0.0".to_string()));
        save_state(&data_dir, "shop", &state).unwrap();

        // WHEN / THEN
        assert!(compute_updates(&workflows_dir, &data_dir).is_empty());
    }

    #[test]
    fn dangling_origin_is_reported_when_marketplace_unregistered() {
        // GIVEN
        let dir = TempDir::new().unwrap();
        let workflows_dir = dir.path().join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        let pkg = workflows_dir.join("my-wf");
        write_workflow(&pkg, "my-wf", Some("1.0.0"));
        save_origin(
            &pkg,
            &Origin {
                marketplace: "gone".to_string(),
                path: "workflows/my".to_string(),
                installed_version: Some("1.0.0".to_string()),
            },
        )
        .unwrap();

        // WHEN no marketplaces are registered
        let dangling = dangling_origins(&workflows_dir, &[]);

        // THEN
        assert_eq!(dangling, vec![("my-wf".to_string(), "gone".to_string())]);
    }

    #[test]
    fn resolve_workflow_finds_by_name() {
        // GIVEN a clone with two workflows
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let clone = clone_dir(data_dir, "shop");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(
            clone.join(".otter-marketplace.toml"),
            r#"
schema = 1
name = "shop"
[[workflow]]
path = "workflows/a"
[[workflow]]
path = "workflows/b"
"#,
        )
        .unwrap();
        write_workflow(&clone.join("workflows/a"), "alpha", Some("1.0.0"));
        write_workflow(&clone.join("workflows/b"), "beta", Some("0.1.0"));

        // WHEN
        let path = resolve_workflow_in_marketplace(data_dir, "shop", "beta").unwrap();

        // THEN
        assert_eq!(path, clone.join("workflows/b"));
    }

    #[test]
    fn resolve_workflow_skips_wip() {
        // GIVEN beta is wip
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let clone = clone_dir(data_dir, "shop");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(
            clone.join(".otter-marketplace.toml"),
            r#"
schema = 1
name = "shop"
[[workflow]]
path = "workflows/a"
[[workflow]]
path = "workflows/b"
wip = true
"#,
        )
        .unwrap();
        write_workflow(&clone.join("workflows/a"), "alpha", Some("1.0.0"));
        write_workflow(&clone.join("workflows/b"), "beta", Some("0.1.0"));

        // WHEN / THEN
        let err = resolve_workflow_in_marketplace(data_dir, "shop", "beta").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn registry_round_trip() {
        // GIVEN
        let dir = TempDir::new().unwrap();
        let mps = vec![Marketplace {
            name: "official".to_string(),
            url: "https://example.com/repo.git".to_string(),
            added_at: Utc::now(),
        }];
        // WHEN
        save_registry(dir.path(), &mps).unwrap();
        let loaded = load_registry(dir.path()).unwrap();
        // THEN
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "official");
        assert_eq!(loaded[0].url, "https://example.com/repo.git");
    }

    #[test]
    fn refresh_state_records_versions_from_clone() {
        // GIVEN a clone with two workflows on disk
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path();
        let clone = clone_dir(data_dir, "shop");
        std::fs::create_dir_all(&clone).unwrap();
        std::fs::write(
            clone.join(".otter-marketplace.toml"),
            r#"
schema = 1
name = "shop"
[[workflow]]
path = "workflows/a"
[[workflow]]
path = "workflows/b"
"#,
        )
        .unwrap();
        write_workflow(&clone.join("workflows/a"), "alpha", Some("1.0.0"));
        write_workflow(&clone.join("workflows/b"), "beta", None);

        // WHEN
        let state = refresh_state_from_clone(data_dir, "shop").unwrap();

        // THEN
        assert_eq!(
            state.known_versions.get("workflows/a"),
            Some(&Some("1.0.0".to_string()))
        );
        assert_eq!(state.known_versions.get("workflows/b"), Some(&None));
        assert!(state.last_fetched_at.is_some());
    }
}
