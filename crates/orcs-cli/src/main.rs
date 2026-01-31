//! ORCS CLI - Hackable Agentic Shell

use anyhow::Result;
use clap::Parser;
use orcs_app::{OrcsEngine, World};
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

    // Create World with primary channel
    let mut world = World::new();
    world.create_primary()?;

    // Create engine with injected World
    let mut engine = OrcsEngine::new(world);

    info!("Engine initialized with primary channel");

    // For now, just run a simple loop
    if args.command.is_empty() {
        info!("Interactive mode (not yet implemented)");
        info!("Press Ctrl+C to exit");

        // Run engine
        engine.run().await;
    } else {
        let cmd = args.command.join(" ");
        info!("Command mode: {}", cmd);
        // TODO: Execute command
    }

    Ok(())
}
