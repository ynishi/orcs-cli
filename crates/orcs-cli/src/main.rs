//! ORCS CLI - Hackable Agentic Shell
//!
//! # Configuration
//!
//! Configuration is loaded from multiple sources with priority:
//!
//! 1. CLI arguments (highest priority)
//! 2. Environment variables (`ORCS_*`)
//! 3. Project config (`.orcs/config.toml` in current directory)
//! 4. Global config (`~/.orcs/config.toml`)
//! 5. Default values (lowest priority)
//!
//! # Environment Variables
//!
//! - `ORCS_DEBUG`: Enable debug mode (`true`/`false`)
//! - `ORCS_VERBOSE`: Enable verbose output
//! - `ORCS_MODEL`: Default model name
//! - `ORCS_AUTO_APPROVE`: Auto-approve all requests (dangerous)
//! - `ORCS_SESSION_PATH`: Custom session storage path

use anyhow::Result;
use clap::Parser;
use orcs_app::OrcsApp;
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

/// ORCS CLI - Hackable Agentic Shell
#[derive(Parser, Debug)]
#[command(name = "orcs")]
#[command(version, about, long_about = None)]
struct Args {
    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Enable verbose output
    #[arg(short, long)]
    verbose: bool,

    /// Project root directory (defaults to current directory)
    #[arg(short = 'C', long)]
    project: Option<PathBuf>,

    /// Resume a previous session by ID
    #[arg(long)]
    resume: Option<String>,

    /// Custom session storage path
    #[arg(long)]
    session_path: Option<PathBuf>,

    /// Command to execute (optional)
    #[arg(trailing_var_arg = true)]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup logging based on CLI args
    let filter = if args.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };

    fmt().with_env_filter(filter).with_target(false).init();

    info!("ORCS CLI v{}", env!("CARGO_PKG_VERSION"));

    // Build application with configuration
    let mut builder = OrcsApp::builder();

    // Set project root (defaults to current directory)
    let project_root = match args.project {
        Some(path) => {
            info!(path = %path.display(), "Using specified project root");
            path
        }
        None => {
            let cwd = std::env::current_dir().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to get current directory, using '.'");
                PathBuf::from(".")
            });
            info!(path = %cwd.display(), "Using current directory as project root");
            cwd
        }
    };
    builder = builder.with_project_root(project_root);

    // Apply CLI overrides
    if args.debug {
        builder = builder.debug();
    }
    if args.verbose {
        builder = builder.verbose();
    }
    if let Some(path) = args.session_path {
        builder = builder.with_session_path(path);
    }
    if let Some(session_id) = args.resume {
        builder = builder.resume(session_id);
    }

    let mut app = builder.build().await?;

    info!(
        "Application initialized (debug={}, verbose={})",
        app.config().debug,
        app.config().ui.verbose
    );

    // Run in interactive or command mode
    if args.command.is_empty() {
        app.run_interactive().await?;
    } else {
        let cmd = args.command.join(" ");
        info!("Command mode: {}", cmd);
        // TODO: Execute command
    }

    Ok(())
}
