//! `otter update` subcommand: probe → stop daemon → swap binary → restart daemon.

use std::time::{Duration, Instant};

use anyhow::Context;

use crate::dirs_data_dir;
use crate::service::{is_service_running, platform_service_manager};

use super::{check_latest, perform_update, write_cache, GithubReleaseFetcher};

pub async fn run(check_only: bool, force: bool) -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let fetcher = GithubReleaseFetcher::new();
    let info = check_latest(&fetcher, current)
        .await
        .context("check for newer otter release")?;

    if check_only {
        match info {
            Some(u) => println!(
                "Update available: v{} → v{}\n{}",
                u.current, u.latest, u.release_url
            ),
            None => println!("Up to date (v{current})."),
        }
        return Ok(());
    }

    if info.is_none() && !force {
        println!("Already up to date (v{current}).");
        return Ok(());
    }
    let latest_label = info
        .as_ref()
        .map(|u| u.latest.clone())
        .unwrap_or_else(|| "latest".to_string());

    let was_running = is_service_running();
    let mgr = platform_service_manager();

    if was_running {
        println!("Stopping daemon …");
        mgr.stop().context("stop daemon before swap")?;
        wait_until(Duration::from_secs(5), || !is_service_running());
    }

    println!("Downloading otter v{latest_label} …");
    // `self_update` is blocking (uses ureq / std::fs). Run it on a blocking
    // thread so we don't stall the tokio reactor with progress-bar writes.
    let current_owned = current.to_string();
    let result = tokio::task::spawn_blocking(move || perform_update(&current_owned))
        .await
        .context("spawn self_update worker")??;

    let installed_version = match result {
        self_update::Status::UpToDate(v) => v,
        self_update::Status::Updated(v) => v,
    };
    let _ = write_cache(&dirs_data_dir(), None);
    println!("Updated to v{installed_version}.");

    if was_running {
        println!("Restarting daemon …");
        mgr.start().context("restart daemon after swap")?;
    }
    println!("Restart any open TUI to pick up the new client.");
    Ok(())
}

fn wait_until<F: Fn() -> bool>(timeout: Duration, cond: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    cond()
}
