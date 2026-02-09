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
use orcs_app::{ConfigError, ConfigLoader, ConfigResolver, OrcsApp, OrcsConfig, ProjectSandbox};
use std::path::PathBuf;
use std::sync::Arc;
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

/// CLI-based configuration resolver.
///
/// Merges file/env config via [`ConfigLoader`] and applies CLI argument
/// overrides as the highest-priority layer.
struct CliConfigResolver {
    project_root: PathBuf,
    debug: bool,
    verbose: bool,
    session_path: Option<PathBuf>,
}

impl CliConfigResolver {
    fn from_args(args: &Args) -> Self {
        let project_root = args.project.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to get current directory, using '.'");
                PathBuf::from(".")
            })
        });

        Self {
            project_root,
            debug: args.debug,
            verbose: args.verbose,
            session_path: args.session_path.clone(),
        }
    }
}

impl ConfigResolver for CliConfigResolver {
    fn resolve(&self) -> Result<OrcsConfig, ConfigError> {
        let mut config = ConfigLoader::new()
            .with_project_root(&self.project_root)
            .load()?;

        // CLI args override (highest priority)
        if self.debug {
            config.debug = true;
        }
        if self.verbose {
            config.ui.verbose = true;
        }
        if let Some(ref p) = self.session_path {
            config.paths.session_dir = Some(p.clone());
        }

        Ok(config)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup logging: --debug flag > RUST_LOG env > default "info"
    let filter = if args.debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    fmt().with_env_filter(filter).with_target(false).init();

    info!("ORCS CLI v{}", env!("CARGO_PKG_VERSION"));

    let resolver = CliConfigResolver::from_args(&args);
    info!(
        path = %resolver.project_root.display(),
        "Project root"
    );

    // Create sandbox from project root
    let sandbox = Arc::new(
        ProjectSandbox::new(&resolver.project_root)
            .map_err(|e| anyhow::anyhow!("Failed to create sandbox: {e}"))?,
    );

    let mut builder = OrcsApp::builder(resolver).with_sandbox(sandbox);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: creates a CliConfigResolver with a temp dir (no config files).
    fn resolver_with(
        debug: bool,
        verbose: bool,
        session_path: Option<PathBuf>,
    ) -> CliConfigResolver {
        let temp = std::env::temp_dir().join(format!(
            "orcs-cli-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp).unwrap();
        CliConfigResolver {
            project_root: temp,
            debug,
            verbose,
            session_path,
        }
    }

    #[test]
    fn resolve_defaults_no_overrides() {
        let resolver = resolver_with(false, false, None);
        let config = resolver.resolve().unwrap();

        assert!(!config.debug);
        assert!(!config.ui.verbose);
        assert!(config.paths.session_dir.is_none());
    }

    #[test]
    fn resolve_debug_override() {
        let resolver = resolver_with(true, false, None);
        let config = resolver.resolve().unwrap();

        assert!(config.debug);
        assert!(!config.ui.verbose);
    }

    #[test]
    fn resolve_verbose_override() {
        let resolver = resolver_with(false, true, None);
        let config = resolver.resolve().unwrap();

        assert!(!config.debug);
        assert!(config.ui.verbose);
    }

    #[test]
    fn resolve_session_path_override() {
        let path = PathBuf::from("/custom/sessions");
        let resolver = resolver_with(false, false, Some(path.clone()));
        let config = resolver.resolve().unwrap();

        assert_eq!(config.paths.session_dir, Some(path));
    }

    #[test]
    fn resolve_all_overrides() {
        let path = PathBuf::from("/all/overrides");
        let resolver = resolver_with(true, true, Some(path.clone()));
        let config = resolver.resolve().unwrap();

        assert!(config.debug);
        assert!(config.ui.verbose);
        assert_eq!(config.paths.session_dir, Some(path));
    }

    /// #4: CLI flag=false does NOT override file config values.
    ///
    /// When debug/verbose are false (CLI default), the loader's
    /// values must be preserved — `if self.debug` guard skips override.
    #[test]
    fn false_flags_preserve_loader_values() {
        let resolver = resolver_with(false, false, None);
        let config = resolver.resolve().unwrap();

        // Loader returns defaults (false) and CLI doesn't override.
        // Key point: no accidental `config.debug = false` forced write.
        let baseline = OrcsConfig::default();
        assert_eq!(config.debug, baseline.debug);
        assert_eq!(config.ui.verbose, baseline.ui.verbose);
    }

    #[test]
    fn from_args_defaults() {
        let args = Args {
            debug: false,
            verbose: false,
            project: None,
            resume: None,
            session_path: None,
            command: vec![],
        };
        let resolver = CliConfigResolver::from_args(&args);

        assert!(!resolver.debug);
        assert!(!resolver.verbose);
        assert!(resolver.session_path.is_none());
        // project defaults to cwd
        assert!(resolver.project_root.exists());
    }

    #[test]
    fn from_args_with_all_flags() {
        let args = Args {
            debug: true,
            verbose: true,
            project: Some(PathBuf::from("/tmp")),
            resume: Some("sess-123".into()),
            session_path: Some(PathBuf::from("/sessions")),
            command: vec!["run".into()],
        };
        let resolver = CliConfigResolver::from_args(&args);

        assert!(resolver.debug);
        assert!(resolver.verbose);
        assert_eq!(resolver.project_root, PathBuf::from("/tmp"));
        assert_eq!(resolver.session_path, Some(PathBuf::from("/sessions")));
    }
}
