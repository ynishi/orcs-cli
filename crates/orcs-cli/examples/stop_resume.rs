//! Stop/Resume Example (M5 E2E Test)
//!
//! Demonstrates session persistence:
//! 1. Start application
//! 2. Type 'p' or 'pause' to save session and exit
//! 3. Resume with: `cargo run --example stop_resume -- --resume <session_id>`
//!
//! # Usage
//!
//! ```bash
//! # Start new session
//! cargo run --example stop_resume
//!
//! # Type some commands, then 'p' to pause
//! # Note the session ID printed
//!
//! # Resume the session
//! cargo run --example stop_resume -- --resume <session_id>
//! ```

use anyhow::Result;
use clap::Parser;
use orcs_app::OrcsApp;
use tracing_subscriber::{fmt, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "stop_resume")]
#[command(about = "Stop/Resume example for ORCS")]
struct Args {
    /// Resume a previous session by ID
    #[arg(long)]
    resume: Option<String>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,
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

    println!("=== ORCS Stop/Resume Example ===");
    println!();

    // Build application
    let mut builder = OrcsApp::builder();

    if let Some(ref session_id) = args.resume {
        println!("Resuming session: {}", session_id);
        builder = builder.resume(session_id);
    } else {
        println!("Starting new session...");
    }

    let mut app = builder.build().await?;

    println!("Session ID: {}", app.session_id());
    println!();
    println!("Commands:");
    println!("  p / pause  - Save session and exit");
    println!("  q / quit   - Exit without saving");
    println!("  <text>     - Echo with HIL approval");
    println!();

    // Run interactive mode
    app.run_interactive().await?;

    Ok(())
}
