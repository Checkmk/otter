use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use orchestr8r_core::types::{
    DaemonCommand, DaemonEvent, DaemonResponse, WorkflowStatus,
};

use crate::socket_path;

pub async fn run_ui() -> anyhow::Result<()> {
    let stream = match connect_to_daemon().await {
        Ok(s) => s,
        Err(_) => {
            eprintln!("The orchestr8r daemon is not running.\n");
            eprintln!("Start it first (e.g. in a separate terminal):\n");
            eprintln!("    orchestr8r daemon\n");
            eprintln!("Then run `orchestr8r` again to open the dashboard.");
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
                        let _ = event_tx_cmd.send(DaemonEvent::ConsumedTriggersChanged { workflow, triggers }).await;
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("command to daemon failed: {}", e),
            }
        }
    });

    let shutdown = Arc::new(AtomicBool::new(false));

    tokio::task::spawn_blocking(move || orchestr8r_tui::run(event_rx, cmd_tx, shutdown))
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
        DaemonResponse::StatusResponse { workflows } => print_workflows(&workflows),
        DaemonResponse::ConsumedTriggersResponse { .. } => {}
    }
    Ok(())
}

pub async fn print_status() -> anyhow::Result<()> {
    let resp = send_command_once(DaemonCommand::Status).await?;
    match resp {
        DaemonResponse::StatusResponse { workflows } => print_workflows(&workflows),
        DaemonResponse::Error { message } => {
            eprintln!("Error: {message}");
            std::process::exit(1);
        }
        _ => {}
    }
    Ok(())
}

fn print_workflows(workflows: &[WorkflowStatus]) {
    println!("{:<30} {:<12} {}", "NAME", "KIND", "STATE");
    println!("{}", "-".repeat(54));
    for wf in workflows {
        println!(
            "{:<30} {:<12} {:?}",
            wf.name,
            format!("{:?}", wf.kind),
            wf.state,
        );
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
        anyhow::bail!("daemon closed connection without a response");
    }
    Ok(serde_json::from_str(line.trim())?)
}

#[cfg(not(target_os = "windows"))]
async fn connect_to_daemon() -> anyhow::Result<tokio::net::UnixStream> {
    let path = socket_path();
    tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("could not connect to daemon socket at {path:?}"))
}

#[cfg(target_os = "windows")]
async fn connect_to_daemon() -> anyhow::Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    let path = socket_path();
    tokio::net::windows::named_pipe::ClientOptions::new()
        .open(&path)
        .with_context(|| format!("could not connect to daemon pipe at {path:?}"))
}
