use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "agentbox", about = "Sandboxed agent execution using Podman")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a command inside a sandboxed container
    Run {
        /// Container image to use
        #[arg(long, default_value = agentbox::DEFAULT_IMAGE)]
        image: String,

        /// Workspace directory to bind-mount (required)
        #[arg(long)]
        workspace: PathBuf,

        /// Network mode: bridge or none
        #[arg(long, default_value = "bridge")]
        network: String,

        /// CPU limit (e.g. "2.0")
        #[arg(long)]
        cpus: Option<String>,

        /// Additional bind mount (HOST:CONTAINER[:ro|:rw], default ro), repeatable
        #[arg(long = "mount", value_name = "HOST:CONTAINER[:ro|:rw]")]
        mounts: Vec<String>,

        /// Environment variable (KEY=VALUE), repeatable
        #[arg(short = 'e', value_name = "KEY=VALUE")]
        env: Vec<String>,

        /// Disable TTY allocation (for piped/non-interactive use)
        #[arg(long)]
        no_tty: bool,

        /// Do not auto-mount ~/.claude and ~/.claude.json when running claude
        #[arg(long)]
        no_claude_config: bool,

        /// Command to run inside the sandbox
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Build the default agentbox container image
    Build {
        /// Tag for the built image
        #[arg(long)]
        tag: Option<String>,
    },
    /// Export the Containerfile template for customization
    ExportTemplate {
        /// Output path for the Containerfile
        #[arg(long, default_value = "Containerfile")]
        output: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run {
            image,
            workspace,
            network,
            cpus,
            mounts,
            env,
            no_tty,
            no_claude_config,
            command,
        } => {
            if !workspace.exists() {
                std::fs::create_dir_all(&workspace)
                    .map_err(|e| format!("cannot create workspace {}: {e}", workspace.display()))?;
            }
            let workspace = std::fs::canonicalize(&workspace)
                .map_err(|e| format!("workspace {}: {e}", workspace.display()))?;

            let mut extra_mounts: Vec<agentbox::Mount> = mounts
                .iter()
                .map(|m| {
                    let parts: Vec<&str> = m.splitn(3, ':').collect();
                    if parts.len() < 2 {
                        eprintln!("invalid mount format: {m} (expected HOST:CONTAINER[:ro|:rw])");
                        std::process::exit(1);
                    }
                    let read_only = parts.get(2).map(|&s| s != "rw").unwrap_or(true);
                    agentbox::Mount {
                        host: PathBuf::from(parts[0]),
                        container: PathBuf::from(parts[1]),
                        read_only,
                    }
                })
                .collect();

            // Auto-mount ~/.claude and ~/.claude.json when running claude
            let is_claude = command.first().map(|c| c == "claude").unwrap_or(false);
            if is_claude && !no_claude_config {
                let home = std::env::var("HOME").unwrap_or_default();
                let claude_dir = PathBuf::from(&home).join(".claude");
                let claude_json = PathBuf::from(&home).join(".claude.json");
                if claude_dir.exists() {
                    extra_mounts.push(agentbox::Mount {
                        host: claude_dir,
                        container: PathBuf::from("/home/sandbox/.claude"),
                        read_only: false,
                    });
                }
                if claude_json.exists() {
                    extra_mounts.push(agentbox::Mount {
                        host: claude_json,
                        container: PathBuf::from("/home/sandbox/.claude.json"),
                        read_only: false,
                    });
                }
            }

            // Start with safe system vars so tools like claude have a usable environment
            let mut env_vars: Vec<(String, String)> = Vec::new();
            for key in &["TERM", "LANG", "LC_ALL", "USER", "COLORTERM"] {
                if let Ok(val) = std::env::var(key) {
                    env_vars.push((key.to_string(), val));
                }
            }
            env_vars.push(("HOME".to_string(), "/home/sandbox".to_string()));

            // User-supplied -e flags (can override the defaults above)
            for e in &env {
                let parts: Vec<&str> = e.splitn(2, '=').collect();
                if parts.len() != 2 {
                    eprintln!("invalid env format: {e} (expected KEY=VALUE)");
                    std::process::exit(1);
                }
                env_vars.push((parts[0].to_string(), parts[1].to_string()));
            }

            let tty = !no_tty && std::io::stdin().is_terminal();

            let config = agentbox::SandboxConfig {
                image: Some(image),
                workspace_dir: workspace,
                extra_mounts,
                network: agentbox::parse_network_mode(&network),
                cpus,
                env_vars,
                tty,
            };

            let podman_args = agentbox::wrap_command(&command, &config);

            #[cfg(unix)]
            if tty {
                // Replace this process with podman so the shell's terminal state
                // is inherited directly — necessary for interactive PTY sessions.
                use std::os::unix::process::CommandExt;
                let err = std::process::Command::new(&podman_args[0])
                    .args(&podman_args[1..])
                    .exec();
                return Err(err.into());
            }

            let status = std::process::Command::new(&podman_args[0])
                .args(&podman_args[1..])
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status()?;

            std::process::exit(status.code().unwrap_or(1));
        }
        Commands::Build { tag } => {
            agentbox::build_image(tag.as_deref()).await?;
            let tag = tag.as_deref().unwrap_or(agentbox::DEFAULT_IMAGE);
            println!("Image built: {tag}");
        }
        Commands::ExportTemplate { output } => {
            agentbox::export_template(&output)?;
            println!("Containerfile template exported to: {}", output.display());
        }
    }

    Ok(())
}

