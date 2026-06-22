use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use otter_core::types::{
    DaemonCommand, DaemonEvent, DaemonResponse, MarketplaceStatus, WorkflowStatus,
};

use crate::config::load_theme_config;
use crate::{dirs_config_dir, dirs_data_dir, socket_path};
use otter_tui::theme_loader;

pub async fn run_ui() -> anyhow::Result<()> {
    let stream = match connect_to_daemon().await {
        Ok(s) => s,
        Err(_) => {
            eprintln!("The otter service is not running.\n");
            eprintln!("Start it first:\n");
            eprintln!("    otter service start\n");
            eprintln!("Then run `otter` again to open the dashboard.");
            std::process::exit(1);
        }
    };

    let (sub_reader, mut sub_writer) = tokio::io::split(stream);
    let subscribe_line = serde_json::to_string(&DaemonCommand::Subscribe)? + "\n";
    sub_writer.write_all(subscribe_line.as_bytes()).await?;

    let (event_tx, event_rx) = mpsc::channel::<DaemonEvent>(256);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<DaemonCommand>(32);

    // Clone before event_tx is moved into the subscription task below
    let event_tx_cmd = event_tx.clone();

    // Read event stream from the subscription connection
    tokio::spawn(async move {
        let _keep_writer = sub_writer; // hold write half open so connection stays alive
        let mut reader = BufReader::new(sub_reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(ev) = serde_json::from_str::<DaemonEvent>(line.trim()) {
                        if event_tx.send(ev).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    // Forward outbound commands via one-shot connections; pipe trigger list responses back as events
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            let workflow_hint = if let DaemonCommand::ListConsumedTriggers { ref workflow } = cmd {
                Some(workflow.clone())
            } else {
                None
            };
            match send_command_once(cmd).await {
                Ok(DaemonResponse::ConsumedTriggersResponse { triggers }) => {
                    if let Some(workflow) = workflow_hint {
                        let _ = event_tx_cmd
                            .send(DaemonEvent::ConsumedTriggersChanged { workflow, triggers })
                            .await;
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("command to daemon failed: {}", e),
            }
        }
    });

    let shutdown = Arc::new(AtomicBool::new(false));

    let theme_cfg = load_theme_config(&dirs_config_dir());
    let theme = theme_loader::resolve(&theme_cfg);

    let data_dir = dirs_data_dir();
    let config_dir = dirs_config_dir();
    tokio::task::spawn_blocking(move || {
        otter_tui::run(event_rx, cmd_tx, shutdown, theme, data_dir, config_dir)
    })
    .await??;

    Ok(())
}

pub async fn send_command_print(cmd: DaemonCommand) -> anyhow::Result<()> {
    let resp = send_command_once(cmd).await?;
    match resp {
        DaemonResponse::Ok | DaemonResponse::Pong => println!("OK"),
        DaemonResponse::Error { message } => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        DaemonResponse::StatusResponse {
            workflows,
            marketplaces,
        } => {
            print_workflows(&workflows);
            if !marketplaces.is_empty() {
                println!();
                print_marketplaces(&marketplaces, &workflows);
            }
        }
        DaemonResponse::ConsumedTriggersResponse { .. } => {}
    }
    Ok(())
}

pub async fn print_status(service_enabled: bool) -> anyhow::Result<()> {
    match send_command_once(DaemonCommand::Status).await {
        Ok(DaemonResponse::StatusResponse {
            workflows,
            marketplaces,
        }) => {
            let mode = if service_enabled {
                "systemd (auto-start)"
            } else {
                "session only"
            };
            println!("Service: running ({mode})");
            print_update_line();
            println!();
            print_workflows(&workflows);
            if !marketplaces.is_empty() {
                println!();
                print_marketplaces(&marketplaces, &workflows);
            }
        }
        Ok(DaemonResponse::Error { message }) => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        Ok(_) => {}
        Err(_) => {
            println!("Service: stopped");
            print_update_line();
            println!("Run `otter service start` to start the service.");
        }
    }
    Ok(())
}

fn print_update_line() {
    if let Some(u) = crate::updater::read_cache(&crate::dirs_data_dir()) {
        println!(
            "Update: v{} → v{} available (run `otter update`)",
            u.current, u.latest
        );
    }
}

fn print_workflows(workflows: &[WorkflowStatus]) {
    // Compute per-row strings up-front so column widths can size to content.
    struct Row {
        name: String,
        kind: String,
        state: String,
        notes: String,
    }
    let rows: Vec<Row> = workflows
        .iter()
        .map(|wf| {
            let name = match &wf.origin {
                Some(o) => format!("{}@{}", wf.name, o.marketplace),
                None => wf.name.clone(),
            };
            let mut notes: Vec<String> = Vec::new();
            if let Some(o) = &wf.origin {
                if o.dangling {
                    notes.push(format!("'{}' removed", o.marketplace));
                }
            }
            if let Some(v) = &wf.update_available {
                notes.push(format!("update available: {v}"));
            }
            Row {
                name,
                kind: format!("{:?}", wf.kind),
                state: format!("{:?}", wf.state),
                notes: notes.join(", "),
            }
        })
        .collect();

    let col = |header: &str, get: fn(&Row) -> &str| {
        rows.iter()
            .map(|r| get(r).chars().count())
            .max()
            .unwrap_or(0)
            .max(header.chars().count())
    };
    let name_w = col("NAME", |r| &r.name);
    let kind_w = col("KIND", |r| &r.kind);
    let state_w = col("STATE", |r| &r.state);
    let total_w = name_w + kind_w + state_w + "NOTES".len() + 3; // 3 inter-column spaces

    println!(
        "{:<name_w$} {:<kind_w$} {:<state_w$} NOTES",
        "NAME", "KIND", "STATE"
    );
    println!("{}", "-".repeat(total_w));
    for r in &rows {
        println!(
            "{:<name_w$} {:<kind_w$} {:<state_w$} {}",
            r.name, r.kind, r.state, r.notes
        );
    }
}

fn print_marketplaces(marketplaces: &[MarketplaceStatus], workflows: &[WorkflowStatus]) {
    println!("marketplaces:");
    for m in marketplaces {
        let (installed, updates) = installed_and_update_counts(m, workflows);
        let fetched = match m.last_fetched_at {
            None => "never".to_string(),
            Some(t) => format_relative(t),
        };
        let mut bits = vec![format!(
            "{} workflow{}",
            m.workflows.len(),
            if m.workflows.len() == 1 { "" } else { "s" },
        )];
        if installed > 0 {
            bits.push(format!("{installed} installed"));
        }
        if updates > 0 {
            bits.push(format!(
                "{updates} update{} available",
                if updates == 1 { "" } else { "s" },
            ));
        }
        println!("  {}: {} · fetched {}", m.name, bits.join(", "), fetched,);
    }
    println!();
    println!("Run `otter workflow list` to browse available workflows.");
}

fn installed_and_update_counts(
    m: &MarketplaceStatus,
    workflows: &[WorkflowStatus],
) -> (usize, usize) {
    let installed_names: std::collections::HashSet<&str> = workflows
        .iter()
        .filter_map(|w| {
            w.origin
                .as_ref()
                .filter(|o| !o.dangling && o.marketplace == m.name)
                .map(|_| w.name.as_str())
        })
        .collect();
    let updates = workflows
        .iter()
        .filter(|w| {
            w.origin
                .as_ref()
                .is_some_and(|o| !o.dangling && o.marketplace == m.name)
                && w.update_available.is_some()
        })
        .count();
    (installed_names.len(), updates)
}

pub async fn fetch_status() -> Option<(Vec<WorkflowStatus>, Vec<MarketplaceStatus>)> {
    match send_command_once(DaemonCommand::Status).await {
        Ok(DaemonResponse::StatusResponse {
            workflows,
            marketplaces,
        }) => Some((workflows, marketplaces)),
        _ => None,
    }
}

pub async fn print_marketplace_list() -> anyhow::Result<()> {
    let Some((workflows, marketplaces)) = fetch_status().await else {
        eprintln!("The otter service is not running. Run `otter service start` first.");
        std::process::exit(1);
    };

    if marketplaces.is_empty() {
        println!("No marketplaces registered. Add one with `otter marketplace add <git-url>`.");
        return Ok(());
    }

    for m in &marketplaces {
        let (installed, updates) = installed_and_update_counts(m, &workflows);
        let fetched = match m.last_fetched_at {
            None => "never".to_string(),
            Some(t) => format_relative(t),
        };
        let mut bits = vec![format!(
            "{} workflow{}",
            m.workflows.len(),
            if m.workflows.len() == 1 { "" } else { "s" },
        )];
        if installed > 0 {
            bits.push(format!("{installed} installed"));
        }
        if updates > 0 {
            bits.push(format!(
                "{updates} update{} available",
                if updates == 1 { "" } else { "s" },
            ));
        }
        println!("{} — {} · fetched {}", m.name, m.url, fetched);
        println!("  {}", bits.join(", "));
    }

    println!();
    println!("Run `otter workflow list` to browse available workflows.");
    Ok(())
}

pub fn print_available_workflows(marketplaces: &[MarketplaceStatus], workflows: &[WorkflowStatus]) {
    print!("{}", render_available_workflows(marketplaces, workflows));
}

fn render_available_workflows(
    marketplaces: &[MarketplaceStatus],
    workflows: &[WorkflowStatus],
) -> String {
    let mut out = String::new();
    out.push_str("Available\n");

    if marketplaces.is_empty() {
        out.push_str(
            "  No marketplaces registered. Add one with `otter marketplace add <git-url>`.\n",
        );
        return out;
    }

    struct Row {
        name: String,
        version: String,
        description: String,
    }

    let mut any = false;
    for m in marketplaces {
        let installed: std::collections::HashSet<&str> = workflows
            .iter()
            .filter(|w| {
                w.origin
                    .as_ref()
                    .is_some_and(|o| !o.dangling && o.marketplace == m.name)
            })
            .map(|w| w.name.as_str())
            .collect();

        let rows: Vec<Row> = m
            .workflows
            .iter()
            .filter(|wf| !installed.contains(wf.name.as_str()))
            .map(|wf| Row {
                name: wf.name.clone(),
                version: wf.version.clone().unwrap_or_else(|| "-".to_string()),
                description: truncate(wf.description.as_deref().unwrap_or(""), 60),
            })
            .collect();
        if rows.is_empty() {
            continue;
        }
        any = true;

        let col = |header: &str, get: fn(&Row) -> &str| {
            rows.iter()
                .map(|r| get(r).chars().count())
                .max()
                .unwrap_or(0)
                .max(header.chars().count())
        };
        let name_w = col("NAME", |r| &r.name);
        let version_w = col("VERSION", |r| &r.version);

        out.push_str(&format!("  {}:\n", m.name));
        out.push_str(&format!(
            "    {:<name_w$}  {:<version_w$}  DESCRIPTION\n",
            "NAME", "VERSION"
        ));
        for r in &rows {
            out.push_str(&format!(
                "    {:<name_w$}  {:<version_w$}  {}\n",
                r.name, r.version, r.description
            ));
        }
    }

    if any {
        out.push_str("\n  Run `otter workflow install <name>@<marketplace>` to install one.\n");
    } else {
        out.push_str("  All published workflows are installed.\n");
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn format_relative(t: chrono::DateTime<chrono::Utc>) -> String {
    let now = chrono::Utc::now();
    let dur = now.signed_duration_since(t);
    let secs = dur.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

pub async fn send_command_once(cmd: DaemonCommand) -> anyhow::Result<DaemonResponse> {
    let stream = connect_to_daemon().await?;
    let (reader, mut writer) = tokio::io::split(stream);
    let cmd_line = serde_json::to_string(&cmd)? + "\n";
    writer.write_all(cmd_line.as_bytes()).await?;
    drop(writer);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    if line.is_empty() {
        anyhow::bail!("service closed connection without a response");
    }
    Ok(serde_json::from_str(line.trim())?)
}

#[cfg(not(target_os = "windows"))]
async fn connect_to_daemon() -> anyhow::Result<tokio::net::UnixStream> {
    let path = socket_path();
    tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("could not connect to service socket at {path:?}"))
}

#[cfg(target_os = "windows")]
async fn connect_to_daemon() -> anyhow::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let path = socket_path();
    tokio::net::windows::named_pipe::ClientOptions::new()
        .open(&path)
        .with_context(|| format!("could not connect to service pipe at {path:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_core::types::{
        MarketplaceOrigin, MarketplaceWorkflowEntry, WorkflowState, WorkflowType,
    };

    fn wf(
        name: &str,
        marketplace: Option<&str>,
        update: Option<&str>,
        dangling: bool,
    ) -> WorkflowStatus {
        WorkflowStatus {
            name: name.to_string(),
            kind: WorkflowType::Triggered,
            state: WorkflowState::Dormant,
            trigger: None,
            toml_content: None,
            enabled: false,
            update_available: update.map(str::to_string),
            origin: marketplace.map(|m| MarketplaceOrigin {
                marketplace: m.to_string(),
                dangling,
            }),
        }
    }

    fn mp(name: &str, workflows: &[&str]) -> MarketplaceStatus {
        MarketplaceStatus {
            name: name.to_string(),
            url: "test://".to_string(),
            workflow_count: workflows.len(),
            last_fetched_at: None,
            workflows: workflows
                .iter()
                .map(|n| MarketplaceWorkflowEntry {
                    name: n.to_string(),
                    version: None,
                    description: None,
                    path: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn installed_and_update_counts_counts_only_matching_marketplace() {
        // GIVEN a marketplace with three workflows, two installed (one with update),
        // plus an installed workflow from a different marketplace and a dangling install.
        let m = mp("acme", &["a", "b", "c"]);
        let workflows = vec![
            wf("a", Some("acme"), None, false),
            wf("b", Some("acme"), Some("0.2.0"), false),
            wf("c", Some("acme"), None, true), // dangling — does not count
            wf("x", Some("other"), Some("1.0"), false), // different marketplace
        ];

        // WHEN counting installs and updates
        let (installed, updates) = installed_and_update_counts(&m, &workflows);

        // THEN only non-dangling workflows from this marketplace are counted,
        // and only those with an available update contribute to the update count.
        assert_eq!(installed, 2);
        assert_eq!(updates, 1);
    }

    #[test]
    fn available_lists_only_not_installed_workflows() {
        // GIVEN a marketplace with three workflows, one already installed
        let m = mp("acme", &["a", "b", "c"]);
        let workflows = vec![wf("b", Some("acme"), None, false)];

        // WHEN rendering the available section
        let out = render_available_workflows(&[m], &workflows);

        // THEN the installed workflow is omitted and the others are offered
        assert!(out.contains("acme:"));
        assert!(out.contains("a"));
        assert!(out.contains("c"));
        assert!(!out.lines().any(|l| l.trim_start().starts_with("b ")));
        assert!(out.contains("otter workflow install <name>@<marketplace>"));
    }

    #[test]
    fn available_reports_when_everything_is_installed() {
        // GIVEN a marketplace whose every workflow is installed
        let m = mp("acme", &["a", "b"]);
        let workflows = vec![
            wf("a", Some("acme"), None, false),
            wf("b", Some("acme"), None, false),
        ];

        // WHEN rendering the available section
        let out = render_available_workflows(&[m], &workflows);

        // THEN it states there is nothing left to install
        assert!(out.contains("All published workflows are installed."));
    }

    #[test]
    fn available_reports_when_no_marketplaces_registered() {
        // GIVEN no registered marketplaces
        // WHEN rendering the available section
        let out = render_available_workflows(&[], &[]);

        // THEN it guides the user to add one
        assert!(out.contains("No marketplaces registered."));
    }
}
