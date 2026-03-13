use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use orchestr8r_core::types::{
    CheckpointAction, DaemonCommand, DaemonEvent, DaemonResponse, WorkflowStatus,
};

use crate::socket_path;

pub async fn run_ui(no_tui: bool) -> anyhow::Result<()> {
    let stream = connect_to_daemon()
        .await
        .context("Failed to connect to daemon — is it running?")?;

    let (sub_reader, mut sub_writer) = stream.into_split();
    let subscribe_line = serde_json::to_string(&DaemonCommand::Subscribe)? + "\n";
    sub_writer.write_all(subscribe_line.as_bytes()).await?;

    let (event_tx, event_rx) = mpsc::channel::<DaemonEvent>(256);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<DaemonCommand>(32);

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

    // Forward outbound commands via one-shot connections
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            if let Err(e) = send_command_once(cmd).await {
                tracing::warn!("command to daemon failed: {}", e);
            }
        }
    });

    let shutdown = Arc::new(AtomicBool::new(false));

    if no_tui {
        tokio::task::spawn_blocking(move || run_stdin_event_handler(event_rx, cmd_tx)).await??;
    } else {
        let shutdown_clone = shutdown.clone();
        tokio::task::spawn_blocking(move || {
            orchestr8r_tui::run(event_rx, cmd_tx, shutdown_clone)
        })
        .await??;
    }

    Ok(())
}

/// Blocking stdin-based event handler for `orchestr8r ui --no-tui`.
fn run_stdin_event_handler(
    mut event_rx: mpsc::Receiver<DaemonEvent>,
    cmd_tx: mpsc::Sender<DaemonCommand>,
) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
    use crossterm::terminal;
    use std::io::{self, BufRead, Write};

    let handle = tokio::runtime::Handle::current();

    loop {
        let ev = match handle.block_on(event_rx.recv()) {
            Some(ev) => ev,
            None => break,
        };

        match ev {
            DaemonEvent::LogAppended(entry) => {
                if !entry.stdout.is_empty() {
                    print!("{}", entry.stdout);
                }
                if !entry.stderr.is_empty() {
                    eprint!("{}", entry.stderr);
                }
            }
            DaemonEvent::CheckpointPending {
                run_id,
                message,
                feedback_available,
                ..
            } => {
                println!("\n[CHECKPOINT] {message}");
                if feedback_available {
                    print!("(c)ontinue, (s)top, or give (f)eedback: ");
                } else {
                    print!("(c)ontinue or (s)top: ");
                }
                io::stdout().flush()?;

                terminal::enable_raw_mode().map_err(|e| io::Error::other(e))?;
                while event::poll(std::time::Duration::ZERO).unwrap_or(false) {
                    let _ = event::read();
                }
                let action_char = loop {
                    if let Ok(Event::Key(KeyEvent { code, modifiers, .. })) = event::read() {
                        if modifiers.contains(KeyModifiers::CONTROL)
                            && code == KeyCode::Char('c')
                        {
                            break None;
                        }
                        match code {
                            KeyCode::Char('c') => break Some('c'),
                            KeyCode::Char('s') => break Some('s'),
                            KeyCode::Char('f') if feedback_available => break Some('f'),
                            _ => {}
                        }
                    }
                };
                terminal::disable_raw_mode().map_err(|e| io::Error::other(e))?;
                println!();

                let action = match action_char {
                    Some('c') => CheckpointAction::Continue,
                    Some('f') => {
                        print!("Feedback: ");
                        io::stdout().flush()?;
                        let mut line = String::new();
                        io::stdin().lock().read_line(&mut line)?;
                        CheckpointAction::Feedback(line.trim().to_string())
                    }
                    _ => CheckpointAction::Stop,
                };
                let _ = cmd_tx.try_send(DaemonCommand::CheckpointRespond { run_id, action });
            }
            _ => {}
        }
    }

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
    let (reader, mut writer) = stream.into_split();
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

async fn connect_to_daemon() -> anyhow::Result<tokio::net::UnixStream> {
    let path = socket_path();
    tokio::net::UnixStream::connect(&path)
        .await
        .with_context(|| format!("could not connect to daemon socket at {path:?}"))
}
