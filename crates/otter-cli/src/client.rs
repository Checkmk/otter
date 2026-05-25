use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use otter_core::types::{
    DaemonCommand, DaemonEvent, DaemonResponse, MarketplaceStatus, WorkflowStatus,
};

use crate::config::load_theme_config;
use crate::{dirs_config_dir, socket_path};
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

    tokio::task::spawn_blocking(move || otter_tui::run(event_rx, cmd_tx, shutdown, theme))
        .await??;

    Ok(())
}

pub async fn send_command_print(cmd: DaemonCommand) -> anyhow::Result<()> {
    let resp = send_command_once(cmd).await?;
    match resp {
        DaemonResponse::Ok => println!("OK"),
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
                print_marketplaces(&marketplaces);
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
            println!("Service: running ({mode})\n");
            print_workflows(&workflows);
            if !marketplaces.is_empty() {
                println!();
                print_marketplaces(&marketplaces);
            }
        }
        Ok(DaemonResponse::Error { message }) => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        Ok(_) => {}
        Err(_) => {
            println!("Service: stopped");
            println!("Run `otter service start` to start the service.");
        }
    }
    Ok(())
}

fn print_workflows(workflows: &[WorkflowStatus]) {
    println!("{:<30} {:<12} {:<14} NOTES", "NAME", "KIND", "STATE");
    println!("{}", "-".repeat(74));
    for wf in workflows {
        let mut notes: Vec<String> = Vec::new();
        if let Some(v) = &wf.update_available {
            notes.push(format!("update available: {v}"));
        }
        if wf.origin_dangling {
            notes.push("origin marketplace removed".to_string());
        }
        println!(
            "{:<30} {:<12} {:<14} {}",
            wf.name,
            format!("{:?}", wf.kind),
            format!("{:?}", wf.state),
            notes.join(", "),
        );
    }
}

fn print_marketplaces(marketplaces: &[MarketplaceStatus]) {
    println!("marketplaces:");
    println!("  {:<20} {:<5} {:<20} URL", "NAME", "WFS", "LAST FETCH");
    println!("  {}", "-".repeat(80));
    for m in marketplaces {
        let fetched = match m.last_fetched_at {
            None => "never".to_string(),
            Some(t) => format_relative(t),
        };
        println!(
            "  {:<20} {:<5} {:<20} {}",
            m.name, m.workflow_count, fetched, m.url
        );
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
