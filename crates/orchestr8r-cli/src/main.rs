use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use clap::{Parser, Subcommand, ArgAction};
use tracing::info;

use orchestr8r_core::engine::Engine;
use orchestr8r_core::types::WorkflowDef;
use orchestr8r_storage::SqliteStorage;

#[derive(Parser)]
#[command(name = "orchestr8r", about = "Workflow automation service")]
struct Cli {
    /// Path to the workflow TOML file (shorthand for `run <workflow>`)
    workflow: Option<PathBuf>,

    /// Increase log verbosity (-v = debug, -vv = trace)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    verbose: u8,

    /// Suppress all log output
    #[arg(short, long, global = true)]
    quiet: bool,

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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(level.into()),
        )
        .init();

    let workflow_path = match (cli.command, cli.workflow) {
        (Some(Commands::Run { workflow }), _) => workflow,
        (None, Some(workflow)) => workflow,
        (None, None) => {
            eprintln!("Usage: orchestr8r <workflow.toml>  or  orchestr8r run <workflow.toml>");
            std::process::exit(1);
        }
    };

    run_workflow(workflow_path).await
}

async fn run_workflow(workflow_path: PathBuf) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(&workflow_path)
        .with_context(|| format!("Failed to read workflow file: {:?}", workflow_path))?;

    let workflow_def: WorkflowDef = toml::from_str(&content)
        .with_context(|| format!("Failed to parse workflow TOML: {:?}", workflow_path))?;

    info!(workflow = %workflow_def.name, "Loaded workflow definition");

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
        tokio::signal::ctrl_c().await.expect("Failed to listen for ctrl_c");
        info!("Ctrl+C received, shutting down after current step...");
        shutdown_clone.store(true, Ordering::Relaxed);
    });

    let engine = Engine::new(storage, scratch_base);
    engine.run(&workflow_def, shutdown).await?;

    info!("Workflow stopped cleanly");
    Ok(())
}

fn dirs_data_dir() -> PathBuf {
    // XDG data dir on Linux/Mac, %APPDATA% equivalent on Windows
    #[cfg(target_os = "windows")]
    {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("orchestr8r")
    }
    #[cfg(not(target_os = "windows"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".local").join("share").join("orchestr8r")
    }
}
