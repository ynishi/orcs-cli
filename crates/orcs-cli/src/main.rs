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

mod tracing_writer;

use anyhow::Result;
use clap::Parser;
use orcs_app::{
    ConfigError, ConfigLoader, ConfigResolver, OrcsApp, OrcsConfig, ProjectSandbox,
    SharedPrinterSlot, WorkDir,
};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

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

    /// Run in sandbox mode with isolated state (sessions, builtins, history, logs).
    ///
    /// Uses a temporary directory if DIR is omitted. Global config is skipped.
    /// Useful for clean-install testing and demos.
    #[arg(long, value_name = "DIR")]
    sandbox: Option<Option<PathBuf>>,

    /// Override log file directory path (also: ORCS_LOG_FILE)
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,

    /// Override file log level (also: ORCS_LOG_LEVEL, default: debug)
    #[arg(long, value_name = "LEVEL")]
    log_level: Option<String>,

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
    log_file: Option<PathBuf>,
    log_level: Option<String>,
    /// Sandbox root directory. When set, all `~/.orcs/` paths are redirected
    /// here and global config is skipped (clean-install simulation).
    pub(crate) sandbox_dir: Option<PathBuf>,
}

impl CliConfigResolver {
    fn from_args(args: &Args) -> Self {
        let project_root = args.project.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to get current directory, using '.'");
                PathBuf::from(".")
            })
        });

        // --sandbox: resolve to explicit dir (handled by caller via WorkDir)
        let sandbox_dir = args.sandbox.as_ref().and_then(|opt_path| opt_path.clone());

        Self {
            project_root,
            debug: args.debug,
            verbose: args.verbose,
            experimental: args.experimental,
            session_path: args.session_path.clone(),
            builtins_dir: args.builtins_dir.clone(),
            profile: args.profile.clone(),
            log_file: args.log_file.clone(),
            log_level: args.log_level.clone(),
            sandbox_dir,
        }
    }
}

impl ConfigResolver for CliConfigResolver {
    fn resolve(&self) -> Result<OrcsConfig, ConfigError> {
        let mut loader = ConfigLoader::new().with_project_root(&self.project_root);

        // Sandbox: skip global config (clean-install simulation)
        if self.sandbox_dir.is_some() {
            loader = loader.skip_global_config();
        }

        if let Some(ref profile) = self.profile {
            loader = loader.with_profile(profile);
        }

        let mut config = loader.load()?;

        // Sandbox: redirect all ~/.orcs/ paths to sandbox dir
        if let Some(ref sandbox) = self.sandbox_dir {
            config.paths.session_dir = Some(sandbox.join("sessions"));
            config.paths.history_file = Some(sandbox.join("history"));
            config.components.builtins_dir = sandbox.join("builtins");
            config.components.paths = vec![sandbox.join("components")];
            config.scripts.dirs = vec![sandbox.join("scripts")];
            config.logging.file_path = Some(sandbox.join("logs"));
        }

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
        if let Some(ref p) = self.log_file {
            config.logging.file_path = Some(p.clone());
        }
        if let Some(ref level) = self.log_level {
            config.logging.file_level.clone_from(level);
        }

        Ok(config)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Shared printer slot: links tracing output to rustyline's ExternalPrinter
    let printer_slot = SharedPrinterSlot::new();

    // Create WorkDir for sandbox lifecycle management.
    // WorkDir is owned by main() — Temporary variant auto-cleans on process exit.
    let sandbox_work_dir: Option<WorkDir> = match &args.sandbox {
        Some(Some(path)) => Some(WorkDir::persistent(path.clone()).unwrap_or_else(|e| {
            eprintln!(
                "Error: cannot create sandbox directory {}: {e}",
                path.display()
            );
            std::process::exit(1);
        })),
        Some(None) => Some(WorkDir::temporary().unwrap_or_else(|e| {
            eprintln!("Error: cannot create temporary sandbox directory: {e}");
            std::process::exit(1);
        })),
        None => None,
    };

    let mut resolver = CliConfigResolver::from_args(&args);

    // Inject WorkDir path into resolver (resolver borrows the path, WorkDir owns the lifecycle)
    if let Some(ref wd) = sandbox_work_dir {
        resolver.sandbox_dir = Some(wd.path().to_path_buf());
        println!("Sandbox: {}", wd.path().display());
    }

    // Resolve config early to access logging settings for tracing initialization.
    // The resolved config is also used later to build the app.
    let resolved_config = resolver
        .resolve()
        .map_err(|e| anyhow::anyhow!("Config error: {e}"))?;

    // --- Tracing setup: independent terminal and file filters ---
    //
    // Terminal filter: --debug > --verbose > RUST_LOG env > default "warn"
    // File filter:     config.logging.file_level (default "debug") — always independent
    //
    // External crates (hyper, h2, reqwest, tokio, rustls) are suppressed at WARN
    // on both layers to avoid flooding output with HTTP/TLS debug noise.
    let terminal_filter = if args.debug {
        EnvFilter::new(
            "debug,hyper=warn,h2=warn,reqwest=warn,rustls=warn,tokio=warn,tungstenite=warn,rustyline=warn",
        )
    } else if args.verbose {
        EnvFilter::new("info")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
    };

    let terminal_writer = tracing_writer::TerminalMakeWriter::new(&printer_slot);
    let terminal_layer = fmt::layer().with_target(false).with_writer(terminal_writer);

    // File layer: independent filter, ANSI disabled (clean log)
    let log_file = if resolved_config.logging.file {
        open_log_file(&resolved_config.logging.resolved_file_path())
    } else {
        None
    };

    if let Some(file) = log_file {
        let file_filter = EnvFilter::new(resolved_config.logging.file_filter_directive());
        let file_writer = tracing_writer::FileMakeWriter::new(file);
        let file_layer = fmt::layer()
            .with_target(false)
            .with_ansi(false)
            .with_writer(file_writer);

        tracing_subscriber::registry()
            .with(terminal_layer.with_filter(terminal_filter))
            .with(file_layer.with_filter(file_filter))
            .init();
    } else {
        // No file logging — terminal only
        tracing_subscriber::registry()
            .with(terminal_layer.with_filter(terminal_filter))
            .init();
    };

    println!("ORCS CLI v{}", env!("CARGO_PKG_VERSION"));

    info!(
        path = %resolver.project_root.display(),
        "Project root"
    );

    if resolved_config.logging.file {
        info!(
            path = %resolved_config.logging.resolved_file_path().join("orcs.log").display(),
            level = %resolved_config.logging.file_level,
            "File logging enabled"
        );
    }

    // Handle --install-builtins before full app startup
    if args.install_builtins {
        let builtins_base = resolved_config.components.resolved_builtins_dir();
        let target = orcs_app::builtins::versioned_dir(&builtins_base);

        if args.force {
            println!("Force-installing builtins to {}", target.display());
            let written = orcs_app::builtins::force_expand(&builtins_base)
                .map_err(|e| anyhow::anyhow!("Failed to install builtins: {e}"))?;
            println!(
                "Installed {} file(s) to {}",
                written.len(),
                target.display()
            );
        } else {
            let written = orcs_app::builtins::ensure_expanded(&builtins_base)
                .map_err(|e| anyhow::anyhow!("Failed to install builtins: {e}"))?;
            if written.is_empty() {
                println!("Builtins already installed at {}", target.display());
            } else {
                println!(
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

    let mut builder = OrcsApp::builder(resolver)
        .with_sandbox(sandbox)
        .with_printer_slot(printer_slot);
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
        let exit_code = app.run_command(&cmd).await?;
        if exit_code != 0 {
            std::process::exit(exit_code);
        }
    }

    Ok(())
}

/// Opens the persistent log file in the given directory.
///
/// Creates `<log_dir>/orcs.log` in append mode.
/// Returns `None` if the directory/file cannot be created (non-fatal).
fn open_log_file(log_dir: &std::path::Path) -> Option<Arc<parking_lot::Mutex<std::fs::File>>> {
    if let Err(e) = std::fs::create_dir_all(log_dir) {
        eprintln!(
            "Warning: cannot create log directory {}: {e}",
            log_dir.display()
        );
        return None;
    }

    let log_path = log_dir.join("orcs.log");

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => Some(Arc::new(parking_lot::Mutex::new(file))),
        Err(e) => {
            eprintln!("Warning: cannot open log file {}: {e}", log_path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: creates a CliConfigResolver backed by a WorkDir (no config files).
    fn resolver_with(
        debug: bool,
        verbose: bool,
        session_path: Option<PathBuf>,
    ) -> (WorkDir, CliConfigResolver) {
        let wd = WorkDir::temporary().expect("should create temp WorkDir for test");
        let resolver = CliConfigResolver {
            project_root: wd.path().to_path_buf(),
            debug,
            verbose,
            experimental: false,
            session_path,
            builtins_dir: None,
            profile: None,
            log_file: None,
            log_level: None,
            sandbox_dir: None,
        };
        (wd, resolver)
    }

    #[test]
    fn resolve_defaults_no_overrides() {
        let (_wd, resolver) = resolver_with(false, false, None);
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(!config.debug);
        assert!(!config.ui.verbose);
        assert!(config.paths.session_dir.is_none());
    }

    #[test]
    fn resolve_debug_override() {
        let (_wd, resolver) = resolver_with(true, false, None);
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(config.debug);
        assert!(!config.ui.verbose);
    }

    #[test]
    fn resolve_verbose_override() {
        let (_wd, resolver) = resolver_with(false, true, None);
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(!config.debug);
        assert!(config.ui.verbose);
    }

    #[test]
    fn resolve_session_path_override() {
        let path = PathBuf::from("/custom/sessions");
        let (_wd, resolver) = resolver_with(false, false, Some(path.clone()));
        let config = resolver.resolve().expect("resolve should succeed");

        assert_eq!(config.paths.session_dir, Some(path));
    }

    #[test]
    fn resolve_all_overrides() {
        let path = PathBuf::from("/all/overrides");
        let (_wd, resolver) = resolver_with(true, true, Some(path.clone()));
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
        let (_wd, resolver) = resolver_with(false, false, None);
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
            sandbox: None,
            log_file: None,
            log_level: None,
            command: vec![],
        };
        let resolver = CliConfigResolver::from_args(&args);

        assert!(!resolver.debug);
        assert!(!resolver.verbose);
        assert!(!resolver.experimental);
        assert!(resolver.session_path.is_none());
        assert!(resolver.builtins_dir.is_none());
        assert!(resolver.sandbox_dir.is_none());
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
            sandbox: None,
            log_file: Some(PathBuf::from("/custom/logs")),
            log_level: Some("trace".into()),
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
        assert_eq!(resolver.log_file, Some(PathBuf::from("/custom/logs")));
        assert_eq!(resolver.log_level, Some("trace".into()));
    }

    #[test]
    fn resolve_builtins_dir_override() {
        let (_wd, mut resolver) = resolver_with(false, false, None);
        let custom = PathBuf::from("/custom/builtins");
        resolver.builtins_dir = Some(custom.clone());

        let config = resolver.resolve().expect("resolve should succeed");
        assert_eq!(config.components.builtins_dir, custom);
    }

    #[test]
    fn resolve_builtins_dir_default_when_unset() {
        let (_wd, resolver) = resolver_with(false, false, None);
        let config = resolver.resolve().expect("resolve should succeed");

        let default = OrcsConfig::default();
        assert_eq!(
            config.components.builtins_dir,
            default.components.builtins_dir
        );
    }

    #[test]
    fn resolve_experimental_adds_components() {
        let (_wd, mut resolver) = resolver_with(false, false, None);
        resolver.experimental = true;
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(config.components.load.contains(&"life_game".to_string()));
    }

    #[test]
    fn resolve_no_experimental_by_default() {
        let (_wd, resolver) = resolver_with(false, false, None);
        let config = resolver.resolve().expect("resolve should succeed");

        assert!(!config.components.load.contains(&"life_game".to_string()));
    }

    // --- Sandbox tests ---

    /// Helper: creates a CliConfigResolver with sandbox enabled.
    fn resolver_with_sandbox(sandbox_dir: PathBuf) -> (WorkDir, CliConfigResolver) {
        let wd = WorkDir::temporary().expect("should create temp WorkDir for sandbox test");
        let resolver = CliConfigResolver {
            project_root: wd.path().to_path_buf(),
            debug: false,
            verbose: false,
            experimental: false,
            session_path: None,
            builtins_dir: None,
            profile: None,
            log_file: None,
            log_level: None,
            sandbox_dir: Some(sandbox_dir),
        };
        (wd, resolver)
    }

    #[test]
    fn sandbox_redirects_session_dir() {
        let sandbox = PathBuf::from("/tmp/orcs-sandbox-test");
        let (_wd, resolver) = resolver_with_sandbox(sandbox.clone());
        let config = resolver.resolve().expect("resolve should succeed");

        assert_eq!(config.paths.session_dir, Some(sandbox.join("sessions")));
    }

    #[test]
    fn sandbox_redirects_history_file() {
        let sandbox = PathBuf::from("/tmp/orcs-sandbox-test");
        let (_wd, resolver) = resolver_with_sandbox(sandbox.clone());
        let config = resolver.resolve().expect("resolve should succeed");

        assert_eq!(config.paths.history_file, Some(sandbox.join("history")));
    }

    #[test]
    fn sandbox_redirects_builtins_dir() {
        let sandbox = PathBuf::from("/tmp/orcs-sandbox-test");
        let (_wd, resolver) = resolver_with_sandbox(sandbox.clone());
        let config = resolver.resolve().expect("resolve should succeed");

        assert_eq!(config.components.builtins_dir, sandbox.join("builtins"));
    }

    #[test]
    fn sandbox_redirects_component_paths() {
        let sandbox = PathBuf::from("/tmp/orcs-sandbox-test");
        let (_wd, resolver) = resolver_with_sandbox(sandbox.clone());
        let config = resolver.resolve().expect("resolve should succeed");

        assert_eq!(config.components.paths, vec![sandbox.join("components")]);
    }

    #[test]
    fn sandbox_redirects_script_dirs() {
        let sandbox = PathBuf::from("/tmp/orcs-sandbox-test");
        let (_wd, resolver) = resolver_with_sandbox(sandbox.clone());
        let config = resolver.resolve().expect("resolve should succeed");

        assert_eq!(config.scripts.dirs, vec![sandbox.join("scripts")]);
    }

    #[test]
    fn sandbox_redirects_log_path() {
        let sandbox = PathBuf::from("/tmp/orcs-sandbox-test");
        let (_wd, resolver) = resolver_with_sandbox(sandbox.clone());
        let config = resolver.resolve().expect("resolve should succeed");

        assert_eq!(config.logging.file_path, Some(sandbox.join("logs")));
    }

    #[test]
    fn sandbox_cli_overrides_take_precedence() {
        let sandbox = PathBuf::from("/tmp/orcs-sandbox-test");
        let (_wd, mut resolver) = resolver_with_sandbox(sandbox);
        // Explicit CLI overrides should win over sandbox defaults
        let custom_session = PathBuf::from("/custom/session-override");
        let custom_builtins = PathBuf::from("/custom/builtins-override");
        resolver.session_path = Some(custom_session.clone());
        resolver.builtins_dir = Some(custom_builtins.clone());

        let config = resolver.resolve().expect("resolve should succeed");

        assert_eq!(config.paths.session_dir, Some(custom_session));
        assert_eq!(config.components.builtins_dir, custom_builtins);
    }

    #[test]
    fn from_args_sandbox_none_when_absent() {
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
            sandbox: None,
            log_file: None,
            log_level: None,
            command: vec![],
        };
        let resolver = CliConfigResolver::from_args(&args);
        assert!(resolver.sandbox_dir.is_none());
    }

    #[test]
    fn from_args_sandbox_without_dir_defers_to_workdir() {
        // --sandbox without DIR: from_args sets sandbox_dir = None.
        // main() creates WorkDir::temporary() and injects the path.
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
            sandbox: Some(None), // --sandbox without DIR
            log_file: None,
            log_level: None,
            command: vec![],
        };
        let resolver = CliConfigResolver::from_args(&args);
        assert!(
            resolver.sandbox_dir.is_none(),
            "from_args should not auto-generate; WorkDir handles this in main()"
        );
    }

    #[test]
    fn from_args_sandbox_uses_explicit_dir() {
        let explicit = PathBuf::from("/my/sandbox");
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
            sandbox: Some(Some(explicit.clone())),
            log_file: None,
            log_level: None,
            command: vec![],
        };
        let resolver = CliConfigResolver::from_args(&args);
        assert_eq!(resolver.sandbox_dir, Some(explicit));
    }
}
