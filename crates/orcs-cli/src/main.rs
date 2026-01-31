//! ORCS CLI - Hackable Agentic Shell

use anyhow::Result;
use clap::Parser;
use orcs_app::OrcsApp;
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

    /// Resume a previous session by ID
    #[arg(long)]
    resume: Option<String>,

    /// Command to execute (optional)
    #[arg(trailing_var_arg = true)]
    command: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup logging
    let filter = if args.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("info")
    };

    fmt().with_env_filter(filter).with_target(false).init();

    info!("ORCS CLI v{}", env!("CARGO_PKG_VERSION"));

    // Build application
    let mut builder = OrcsApp::builder();

    if args.debug {
        builder = builder.verbose();
    }

    if let Some(session_id) = args.resume {
        builder = builder.resume(session_id);
    }

    let mut app = builder.build().await?;

    info!("Application initialized");

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
