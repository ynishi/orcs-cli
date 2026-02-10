use clap::{Parser, Subcommand};
use orcs_lint::analyzer::AnalyzerError;
use orcs_lint::config::Config;
use orcs_lint::Analyzer;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "orcs-lint",
    about = "Architecture linter for orcs-cli workspace"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run lint checks on the workspace.
    Check {
        /// Workspace root path (default: current directory).
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Only run a specific rule (e.g., "OL001").
        #[arg(long)]
        rule: Option<String>,

        /// Config file path.
        #[arg(long, default_value = "orcs-lint.toml")]
        config: PathBuf,
    },

    /// List all available rules.
    ListRules,

    /// Generate a default orcs-lint.toml.
    Init,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Check {
            path,
            rule,
            config: config_path,
        } => run_check(&path, &config_path, rule.as_deref()),
        Command::ListRules => {
            run_list_rules();
            ExitCode::SUCCESS
        }
        Command::Init => run_init(),
    }
}

fn run_check(path: &Path, config_path: &Path, rule_filter: Option<&str>) -> ExitCode {
    let config = if config_path.exists() {
        match Config::from_file(config_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Config error: {e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        Config::default()
    };

    let root = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(e) => {
                eprintln!("Warning: failed to get current directory ({e}), using \".\"");
                PathBuf::from(".").join(path)
            }
        }
    };

    let analyzer = Analyzer::new(root, config).with_rule_filter(rule_filter);

    match analyzer.analyze() {
        Ok(result) => {
            result.print_report();
            if result.has_errors() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(AnalyzerError::Io(e)) => {
            eprintln!("IO error: {e}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("Analysis error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run_list_rules() {
    eprintln!("Available rules:\n");
    for (name, desc) in orcs_lint::rules::ALL_RULES {
        eprintln!("  {name}");
        eprintln!("    {desc}\n");
    }
}

fn run_init() -> ExitCode {
    let default_config = r#"[analyzer]
root = "."
exclude = ["**/target/**"]

[rules.layer-dependency]
enabled = true
severity = "error"

[rules.no-unwrap]
enabled = true
severity = "error"
allow_in_tests = true

[rules.no-panic-in-lib]
enabled = true
severity = "error"
allow_in_tests = true
"#;

    let path = PathBuf::from("orcs-lint.toml");
    if path.exists() {
        eprintln!("orcs-lint.toml already exists");
        return ExitCode::FAILURE;
    }

    match std::fs::write(&path, default_config) {
        Ok(()) => {
            eprintln!("Created orcs-lint.toml");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Failed to write orcs-lint.toml: {e}");
            ExitCode::FAILURE
        }
    }
}
