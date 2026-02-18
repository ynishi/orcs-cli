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
//! - `ORCS_BUILTINS_DIR`: Override builtin components directory
//! - `ORCS_EXPERIMENTAL`: Enable experimental components (`true`/`false`)

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

    /// Override builtin components directory (also: ORCS_BUILTINS_DIR)
    #[arg(long)]
    builtins_dir: Option<PathBuf>,

    /// Install/refresh builtin components and exit
    #[arg(long)]
    install_builtins: bool,

    /// Force overwrite when used with --install-builtins
    #[arg(long, requires = "install_builtins")]
    force: bool,

    /// Activate a profile (overrides ORCS_PROFILE env var)
    #[arg(long)]
    profile: Option<String>,

    /// Enable experimental components (also: ORCS_EXPERIMENTAL)
    #[arg(long)]
    experimental: bool,

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
    experimental: bool,
    session_path: Option<PathBuf>,
    builtins_dir: Option<PathBuf>,
    profile: Option<String>,
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
            experimental: args.experimental,
            session_path: args.session_path.clone(),
            builtins_dir: args.builtins_dir.clone(),
            profile: args.profile.clone(),
        }
    }
}

impl ConfigResolver for CliConfigResolver {
    fn resolve(&self) -> Result<OrcsConfig, ConfigError> {
        let mut loader = ConfigLoader::new().with_project_root(&self.project_root);

        if let Some(ref profile) = self.profile {
            loader = loader.with_profile(profile);
        }

        let mut config = loader.load()?;

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
        if let Some(ref p) = self.builtins_dir {
            config.components.builtins_dir = p.clone();
        }
        if self.experimental {
            config.components.activate_experimental();
        }

        Ok(config)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Setup logging: --debug > --verbose > RUST_LOG env > default "warn"
    let filter = if args.debug {
        EnvFilter::new("debug")
    } else if args.verbose {
        EnvFilter::new("info")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
    };

    fmt().with_env_filter(filter).with_target(false).init();

    println!("ORCS CLI v{}", env!("CARGO_PKG_VERSION"));

    let resolver = CliConfigResolver::from_args(&args);
    info!(
        path = %resolver.project_root.display(),
        "Project root"
    );

    // Handle --install-builtins before full app startup
    if args.install_builtins {
        let config = resolver
            .resolve()
            .map_err(|e| anyhow::anyhow!("Config error: {e}"))?;
        let builtins_base = config.components.resolved_builtins_dir();
        let target = orcs_app::builtins::versioned_dir(&builtins_base);

        if args.force {
            info!(path = %target.display(), "Force-installing builtins");
            let written = orcs_app::builtins::force_expand(&builtins_base)
                .map_err(|e| anyhow::anyhow!("Failed to install builtins: {e}"))?;
            info!(
                "Installed {} file(s) to {}",
                written.len(),
                target.display()
            );
        } else {
            info!(path = %target.display(), "Installing builtins (skip if exists)");
            let written = orcs_app::builtins::ensure_expanded(&builtins_base)
                .map_err(|e| anyhow::anyhow!("Failed to install builtins: {e}"))?;
            if written.is_empty() {
                info!("Builtins already installed at {}", target.display());
            } else {
                info!(
                    "Installed {} file(s) to {}",
                    written.len(),
                    target.display()
                );
            }
        }
        return Ok(());
    }

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
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp).expect("should create temp dir");
        CliConfigResolver {
            project_root: temp,
            debug,
            verbose,
            experimental: false,
            session_path,
            builtins_dir: None,
            profile: None,
        }
    }

    #[test]
    fn resolve_defaults_no_overrides() {
        let resolver = resolver_with(false, false, None);
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(!config.debug);
        assert!(!config.ui.verbose);
        assert!(config.paths.session_dir.is_none());
    }

    #[test]
    fn resolve_debug_override() {
        let resolver = resolver_with(true, false, None);
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(config.debug);
        assert!(!config.ui.verbose);
    }

    #[test]
    fn resolve_verbose_override() {
        let resolver = resolver_with(false, true, None);
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(!config.debug);
        assert!(config.ui.verbose);
    }

    #[test]
    fn resolve_session_path_override() {
        let path = PathBuf::from("/custom/sessions");
        let resolver = resolver_with(false, false, Some(path.clone()));
        let config = resolver.resolve().expect("resolve should succeed");

        assert_eq!(config.paths.session_dir, Some(path));
    }

    #[test]
    fn resolve_all_overrides() {
        let path = PathBuf::from("/all/overrides");
        let resolver = resolver_with(true, true, Some(path.clone()));
        let config = resolver.resolve().expect("resolve should succeed");

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
        let config = resolver.resolve().expect("resolve should succeed");

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
            builtins_dir: None,
            install_builtins: false,
            force: false,
            profile: None,
            experimental: false,
            command: vec![],
        };
        let resolver = CliConfigResolver::from_args(&args);

        assert!(!resolver.debug);
        assert!(!resolver.verbose);
        assert!(!resolver.experimental);
        assert!(resolver.session_path.is_none());
        assert!(resolver.builtins_dir.is_none());
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
            builtins_dir: Some(PathBuf::from("/custom/builtins")),
            install_builtins: false,
            force: false,
            profile: Some("rust-dev".into()),
            experimental: true,
            command: vec!["run".into()],
        };
        let resolver = CliConfigResolver::from_args(&args);

        assert!(resolver.debug);
        assert!(resolver.verbose);
        assert!(resolver.experimental);
        assert_eq!(resolver.project_root, PathBuf::from("/tmp"));
        assert_eq!(resolver.session_path, Some(PathBuf::from("/sessions")));
        assert_eq!(
            resolver.builtins_dir,
            Some(PathBuf::from("/custom/builtins"))
        );
    }

    #[test]
    fn resolve_builtins_dir_override() {
        let mut resolver = resolver_with(false, false, None);
        let custom = PathBuf::from("/custom/builtins");
        resolver.builtins_dir = Some(custom.clone());

        let config = resolver.resolve().expect("resolve should succeed");
        assert_eq!(config.components.builtins_dir, custom);
    }

    #[test]
    fn resolve_builtins_dir_default_when_unset() {
        let resolver = resolver_with(false, false, None);
        let config = resolver.resolve().expect("resolve should succeed");

        let default = OrcsConfig::default();
        assert_eq!(
            config.components.builtins_dir,
            default.components.builtins_dir
        );
    }

    #[test]
    fn resolve_experimental_adds_components() {
        let mut resolver = resolver_with(false, false, None);
        resolver.experimental = true;
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(config.components.load.contains(&"life_game".to_string()));
    }

    #[test]
    fn resolve_no_experimental_by_default() {
        let resolver = resolver_with(false, false, None);
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(!config.components.load.contains(&"life_game".to_string()));
    }
}
