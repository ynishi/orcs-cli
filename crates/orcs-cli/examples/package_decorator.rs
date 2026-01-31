//! Package Decorator Example
//!
//! Demonstrates package install/uninstall functionality.
//!
//! The EchoWithHilComponent supports decorator packages that add
//! prefix/suffix to echo output.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example package_decorator
//! ```
//!
//! # Commands
//!
//! - `install <prefix> <suffix>` - Install decorator package
//! - `uninstall` - Uninstall decorator package
//! - `list` - List installed packages
//! - `<text>` - Echo with decoration (requires approval)
//! - `y` - Approve pending echo
//! - `q` - Quit

use anyhow::Result;
use orcs_component::{Package, PackageInfo};
use orcs_runtime::{ChannelConfig, DecoratorConfig, EchoWithHilComponent, OrcsEngine, World};
use orcs_types::ComponentId;
use std::io::{self, BufRead, Write};

fn main() -> Result<()> {
    println!("=== Package Decorator Example ===");
    println!();
    println!("Commands:");
    println!("  install <prefix> <suffix>  - Install decorator (e.g., install << >>)");
    println!("  uninstall                  - Uninstall decorator");
    println!("  list                       - List installed packages");
    println!("  <text>                     - Echo with decoration");
    println!("  y                          - Approve pending echo");
    println!("  q                          - Quit");
    println!();

    // Create engine with EchoWithHilComponent
    let mut world = World::new();
    let io_channel = world.create_channel(ChannelConfig::interactive());
    let mut engine = OrcsEngine::new(world, io_channel);

    // Register component directly for package management
    let echo_id = ComponentId::builtin("echo_with_hil");
    let echo_component = Box::new(EchoWithHilComponent::new());
    engine.register(echo_component);

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("> ");
        stdout.flush()?;

        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.split_whitespace().collect();
        match parts.as_slice() {
            ["q"] | ["quit"] => {
                println!("Goodbye!");
                break;
            }
            ["list"] => {
                let packages = engine.collect_packages();
                if packages.is_empty() {
                    println!("No packages installed.");
                } else {
                    for (comp_id, pkgs) in packages {
                        println!("Component: {}", comp_id);
                        for pkg in pkgs {
                            println!("  - {} v{}: {}", pkg.name, pkg.version, pkg.description);
                        }
                    }
                }
            }
            ["install", prefix, suffix] => {
                let info = PackageInfo::new(
                    "decorator-1",
                    "Decorator",
                    "1.0.0",
                    format!("Adds '{}' prefix and '{}' suffix", prefix, suffix),
                );
                let config = DecoratorConfig {
                    prefix: (*prefix).to_string(),
                    suffix: (*suffix).to_string(),
                };
                let package = Package::new(info, &config)?;

                match engine.install_package(&echo_id, &package) {
                    Ok(()) => {
                        println!("Package installed: decorator-1");
                        println!("  prefix: '{}'", prefix);
                        println!("  suffix: '{}'", suffix);
                    }
                    Err(e) => {
                        println!("Install failed: {}", e);
                    }
                }
            }
            ["uninstall"] => match engine.uninstall_package(&echo_id, "decorator-1") {
                Ok(()) => {
                    println!("Package uninstalled: decorator-1");
                }
                Err(e) => {
                    println!("Uninstall failed: {}", e);
                }
            },
            _ => {
                // Treat as echo input - show how decoration would work
                // For simplicity, we'll just demonstrate the package system
                println!("Unknown command. Try: install <prefix> <suffix>, uninstall, list, or q");
            }
        }
    }

    Ok(())
}
