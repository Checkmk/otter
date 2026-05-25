//! End-to-end marketplace test against a real local git repo. Skipped silently
//! when `git` isn't on PATH so the suite still runs in stripped CI environments.

use otter_core::marketplace;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        // Pin author identity so commits work in CI without global config.
        .env("GIT_AUTHOR_NAME", "Otter Test")
        .env("GIT_AUTHOR_EMAIL", "otter@example.com")
        .env("GIT_COMMITTER_NAME", "Otter Test")
        .env("GIT_COMMITTER_EMAIL", "otter@example.com")
        .current_dir(cwd)
        .output()
        .expect("git command spawnable");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

fn workflow_toml(name: &str, version: &str) -> String {
    format!(
        r#"name = "{name}"
type = "looping"
schema = 1
version = "{version}"
description = "Test workflow {name}"

[[steps]]
type = "shell"
command = ["echo", "hi"]
"#
    )
}

/// Verifies the full flow: register a fake marketplace, refresh state, bump
/// upstream version, fetch, observe `compute_updates` reports the bump.
#[tokio::test]
async fn marketplace_end_to_end() {
    if !git_available() {
        eprintln!("git not available — skipping marketplace_end_to_end");
        return;
    }

    // GIVEN a local git repo acting as a marketplace, two packages,
    // and an otter "host" (data_dir + workflows_dir) checked into a tempdir.
    let host = TempDir::new().unwrap();
    let data_dir = host.path().join("data");
    let workflows_dir = host.path().join("workflows");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&workflows_dir).unwrap();

    let upstream = TempDir::new().unwrap();
    let upstream_path = upstream.path();
    git(upstream_path, &["init", "-q", "-b", "main"]);
    write(
        &upstream_path.join(".otter-marketplace.toml"),
        r#"
schema = 1
name = "shop"
[[workflow]]
path = "workflows/alpha"
[[workflow]]
path = "workflows/beta"
"#,
    );
    write(
        &upstream_path.join("workflows/alpha/workflow.toml"),
        &workflow_toml("alpha", "1.0.0"),
    );
    write(
        &upstream_path.join("workflows/beta/workflow.toml"),
        &workflow_toml("beta", "0.1.0"),
    );
    git(upstream_path, &["add", "."]);
    git(upstream_path, &["commit", "-q", "-m", "initial"]);

    // WHEN we clone the marketplace and refresh state
    let marketplace_name = "shop";
    let clone = marketplace::clone_dir(&data_dir, marketplace_name);
    let url = format!("file://{}", upstream_path.display());
    marketplace::clone_marketplace(&url, &clone).await.unwrap();
    let state = marketplace::refresh_state_from_clone(&data_dir, marketplace_name).unwrap();

    // THEN both upstream versions are recorded
    assert_eq!(
        state.known_versions.get("workflows/alpha"),
        Some(&Some("1.0.0".to_string()))
    );
    assert_eq!(
        state.known_versions.get("workflows/beta"),
        Some(&Some("0.1.0".to_string()))
    );

    // GIVEN the user resolves and "installs" alpha (we mimic the install
    // step by copying the package + writing origin.toml — the CLI's exact
    // flow is exercised by handle_workflow_install which would do the same).
    let pkg =
        marketplace::resolve_workflow_in_marketplace(&data_dir, marketplace_name, "alpha").unwrap();
    let installed = workflows_dir.join("alpha");
    std::fs::create_dir_all(&installed).unwrap();
    std::fs::copy(pkg.join("workflow.toml"), installed.join("workflow.toml")).unwrap();
    marketplace::save_origin(
        &installed,
        &marketplace::Origin {
            marketplace: marketplace_name.to_string(),
            path: "workflows/alpha".to_string(),
            installed_version: Some("1.0.0".to_string()),
        },
    )
    .unwrap();

    // With identical versions, no updates are reported yet.
    assert!(marketplace::compute_updates(&workflows_dir, &data_dir).is_empty());

    // WHEN upstream bumps alpha to 2.0.0 and the daemon fetches
    write(
        &upstream_path.join("workflows/alpha/workflow.toml"),
        &workflow_toml("alpha", "2.0.0"),
    );
    git(upstream_path, &["add", "workflows/alpha/workflow.toml"]);
    git(upstream_path, &["commit", "-q", "-m", "bump alpha"]);

    marketplace::fetch_marketplace(&clone).await.unwrap();
    marketplace::refresh_state_from_clone(&data_dir, marketplace_name).unwrap();

    // THEN compute_updates surfaces the bump
    let updates = marketplace::compute_updates(&workflows_dir, &data_dir);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].workflow_name, "alpha");
    assert_eq!(updates[0].installed.as_deref(), Some("1.0.0"));
    assert_eq!(updates[0].latest.as_deref(), Some("2.0.0"));

    // AND dangling_origins is empty while the marketplace is still registered
    assert!(
        marketplace::dangling_origins(&workflows_dir, &[marketplace_name.to_string()]).is_empty()
    );
    // ...but reports alpha as dangling once the marketplace is "removed"
    let dangling = marketplace::dangling_origins(&workflows_dir, &[]);
    assert_eq!(dangling.len(), 1);
    assert_eq!(dangling[0].0, "alpha");
}
