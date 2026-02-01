//! Console I/O Example
//!
//! Demonstrates the IO abstraction layer:
//! - IOPort for bidirectional communication
//! - HumanChannel as bridge between View and Model
//! - Console for terminal I/O
//!
//! # Usage
//!
//! ```bash
//! cargo run --example console_io
//! ```
//!
//! Then type commands:
//! - `y` or `y <id>` - Approve (shows as Signal)
//! - `n` or `n <id>` - Reject (shows as Signal)
//! - `veto` - Emergency stop
//! - `q` or `quit` - Exit
//! - Any other text - Shows as Unknown command

use orcs_runtime::components::HumanChannel;
use orcs_runtime::io::{setup_ctrlc_handler, Console, IOPort};
use orcs_types::{ChannelId, Principal, PrincipalId};

#[tokio::main]
async fn main() {
    // Initialize tracing for output
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .init();

    println!("=== Console I/O Example ===\n");
    println!("This demonstrates the IO abstraction layer.\n");
    println!("Commands:");
    println!("  y [id]     - Approve (converts to Signal)");
    println!("  n [id]     - Reject (converts to Signal)");
    println!("  veto       - Emergency stop (Veto Signal)");
    println!("  q / quit   - Exit");
    println!("  Ctrl+C     - Send Veto signal");
    println!("\nType a command and press Enter:\n");

    // Create channel and principal
    let channel_id = ChannelId::new();
    let principal = Principal::User(PrincipalId::new());

    // Create IO port (connects View and Bridge)
    let (port, input_handle, output_handle) = IOPort::with_defaults(channel_id);

    // Setup Ctrl+C handler
    setup_ctrlc_handler(input_handle.clone());

    // Create HumanChannel (Bridge layer)
    let mut human_channel = HumanChannel::new(port, principal);

    // Set a default approval ID for demo
    human_channel.set_default_approval_id(Some("demo-request-1".to_string()));

    // Create Console (View layer)
    let console = Console::new(input_handle, output_handle);

    // Split console for manual control
    let (input_reader, renderer, output_handle) = console.split();

    // Spawn renderer task
    let renderer_task = tokio::spawn(async move {
        renderer.run(output_handle).await;
    });

    // Spawn input reader task
    let input_task = tokio::spawn(async move {
        input_reader.run().await;
    });

    // Main loop: process input from HumanChannel
    println!("Waiting for input...\n");

    loop {
        // Wait for input
        match human_channel.recv_input().await {
            Some(Ok(signal)) => {
                // Input converted to Signal
                println!("\n[Signal received]");
                println!("  Kind: {:?}", signal.kind);
                println!("  Scope: {:?}", signal.scope);
                println!("  Source: {:?}", signal.source);

                if signal.is_veto() {
                    println!("\nVeto signal received, exiting...");
                    break;
                }

                println!("\nType another command:");
            }
            Some(Err(cmd)) => {
                // Input is a command that doesn't map to Signal
                println!("\n[Command received (not a Signal)]");
                println!("  {:?}", cmd);

                if matches!(cmd, orcs_runtime::io::InputCommand::Quit) {
                    println!("\nQuit command received, exiting...");
                    break;
                }

                println!("\nType another command:");
            }
            None => {
                // Channel closed
                println!("\nInput channel closed, exiting...");
                break;
            }
        }
    }

    // Cleanup
    println!("\nShutting down...");

    // Abort tasks (they'll exit when channel closes)
    input_task.abort();
    renderer_task.abort();

    println!("=== Example Complete ===");
}
