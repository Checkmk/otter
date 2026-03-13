use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use clap::{ArgAction, Parser, Subcommand};
use tracing::info;

use orchestr8r_core::agent_runner::ClaudeCodeRunner;
use orchestr8r_core::engine::Engine;
use orchestr8r_core::types::{CheckpointResponse, EngineEvent, WorkflowDef};
use orchestr8r_storage::SqliteStorage;

#[derive(Parser)]
#[command(name = "orchestr8r", about = "Workflow automation service")]
struct Cli {
    /// Path to a workflow TOML file (shorthand for `run <workflow>`)
    workflow: Option<PathBuf>,

    /// Increase log verbosity (-v = debug, -vv = trace)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    /// Suppress all log output
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Skip TUI and use stdin for checkpoints
    #[arg(long, global = true)]
    no_tui: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a workflow from a TOML file
    Run {
        /// Path to the workflow TOML file
        workflow: PathBuf,
    },
    /// Send a manual trigger signal to a running workflow
    Trigger {
        /// Name of the workflow to trigger
        workflow: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let level = if cli.quiet {
        tracing::Level::ERROR
    } else {
        match cli.verbose {
            0 => tracing::Level::INFO,
            1 => tracing::Level::DEBUG,
            _ => tracing::Level::TRACE,
        }
    };

    // When TUI is enabled, redirect logs to a file to avoid terminal corruption.
    // When TUI is disabled or --quiet is set, logs go to stderr.
    if !cli.no_tui && !cli.quiet {
        let data_dir = dirs_data_dir();
        std::fs::create_dir_all(&data_dir).ok();
        let log_file = std::fs::File::create(data_dir.join("orchestr8r.log"))
            .context("Failed to create log file")?;
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env().add_directive(level.into()),
            )
            .with_writer(log_file)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env().add_directive(level.into()),
            )
            .init();
    }

    match (cli.command, cli.workflow) {
        (Some(Commands::Trigger { workflow }), _) => trigger_workflow(workflow),
        (Some(Commands::Run { workflow }), _) | (None, Some(workflow)) => {
            run_single_workflow(workflow, cli.no_tui).await
        }
        (None, None) => run_all_workflows(cli.no_tui).await,
    }
}

fn trigger_workflow(workflow_name: String) -> anyhow::Result<()> {
    let trigger_dir = dirs_data_dir().join("triggers");
    std::fs::create_dir_all(&trigger_dir)
        .context("Failed to create trigger directory")?;
    let signal_path = trigger_dir.join(&workflow_name);
    std::fs::write(&signal_path, "")
        .with_context(|| format!("Failed to write trigger signal for '{}'", workflow_name))?;
    println!("Trigger signal sent for workflow: {}", workflow_name);
    Ok(())
}

async fn run_all_workflows(no_tui: bool) -> anyhow::Result<()> {
    let workflows_dir = dirs_config_dir().join("workflows");
    if !workflows_dir.exists() {
        eprintln!(
            "No workflows directory found at {:?}. Create it and add workflow TOML files.",
            workflows_dir
        );
        std::process::exit(1);
    }

    let mut workflow_files: Vec<PathBuf> = std::fs::read_dir(&workflows_dir)
        .with_context(|| format!("Failed to read workflows directory: {:?}", workflows_dir))?
        .filter_map(|entry| entry.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("toml"))
        .collect();

    workflow_files.sort();

    if workflow_files.is_empty() {
        eprintln!("No workflow TOML files found in {:?}", workflows_dir);
        std::process::exit(1);
    }

    let mut workflows = Vec::new();
    for path in &workflow_files {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read workflow file: {:?}", path))?;
        let def: WorkflowDef = toml::from_str(&content)
            .with_context(|| format!("Failed to parse workflow TOML: {:?}", path))?;
        info!(workflow = %def.name, "Loaded workflow definition");
        workflows.push(def);
    }

    run_workflows(workflows, no_tui).await
}

async fn run_single_workflow(workflow_path: PathBuf, no_tui: bool) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(&workflow_path)
        .with_context(|| format!("Failed to read workflow file: {:?}", workflow_path))?;
    let workflow_def: WorkflowDef = toml::from_str(&content)
        .with_context(|| format!("Failed to parse workflow TOML: {:?}", workflow_path))?;
    info!(workflow = %workflow_def.name, "Loaded workflow definition");
    run_workflows(vec![workflow_def], no_tui).await
}

async fn run_workflows(workflows: Vec<WorkflowDef>, no_tui: bool) -> anyhow::Result<()> {
    let data_dir = dirs_data_dir();
    let db_path = data_dir.join("state.db");
    let scratch_base = data_dir.join("runs");

    let storage = Arc::new(
        SqliteStorage::open(&db_path)
            .with_context(|| format!("Failed to open storage at {:?}", db_path))?,
    );

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for ctrl_c");
        info!("Ctrl+C received, shutting down after current step...");
        shutdown_clone.store(true, Ordering::Relaxed);
    });

    let (ui_tx, ui_rx) = tokio::sync::mpsc::channel(256);

    // Spawn one engine task per workflow
    let mut engine_handles = Vec::new();
    for workflow_def in workflows {
        let engine = Engine::new(storage.clone(), scratch_base.clone(), Arc::new(ClaudeCodeRunner));
        let shutdown_for_engine = shutdown.clone();
        let ui_tx_for_engine = ui_tx.clone();

        let handle = tokio::spawn(async move {
            engine
                .run(&workflow_def, shutdown_for_engine, Some(ui_tx_for_engine))
                .await
        });
        engine_handles.push(handle);
    }
    drop(ui_tx); // drop the original so the channel closes when all engines finish

    if no_tui {
        let handler = tokio::task::spawn_blocking(move || run_stdin_checkpoint_handler(ui_rx));
        for handle in engine_handles {
            handle.await??;
        }
        handler.await??;
    } else {
        let shutdown_for_tui = shutdown.clone();
        tokio::task::spawn_blocking(move || orchestr8r_tui::run(ui_rx, shutdown_for_tui))
            .await??;

        for handle in engine_handles {
            handle.await??;
        }
    }

    info!("All workflows stopped cleanly");
    Ok(())
}

fn run_stdin_checkpoint_handler(
    mut rx: tokio::sync::mpsc::Receiver<EngineEvent>,
) -> anyhow::Result<()> {
    use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
    use crossterm::terminal;
    use std::io::{self, BufRead, Write};

    let handle = tokio::runtime::Handle::current();

    loop {
        let ev = match handle.block_on(rx.recv()) {
            Some(ev) => ev,
            None => break,
        };

        match ev {
            EngineEvent::LogAppended(entry) => {
                if !entry.stdout.is_empty() {
                    print!("{}", entry.stdout);
                }
                if !entry.stderr.is_empty() {
                    eprint!("{}", entry.stderr);
                }
                continue;
            }
            EngineEvent::CheckpointPending {
                message,
                feedback_available,
                response_tx,
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
        let action = loop {
            if let Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) = event::read()
            {
                if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
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

        let response = match action {
            Some('c') => CheckpointResponse::Continue,
            Some('s') | None => CheckpointResponse::Stop,
            Some('f') => {
                print!("Feedback: ");
                io::stdout().flush()?;
                let mut line = String::new();
                io::stdin().lock().read_line(&mut line)?;
                CheckpointResponse::Feedback(line.trim().to_string())
            }
            _ => CheckpointResponse::Stop,
        };
        let _ = response_tx.send(response);
            }
            _ => {}
        }
    }

    Ok(())
}

fn dirs_data_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("orchestr8r")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("orchestr8r")
    }
}

fn dirs_config_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("orchestr8r")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("orchestr8r")
    }
}
