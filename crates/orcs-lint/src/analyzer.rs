use crate::config::Config;
use crate::context::{CrateMeta, DepGraph, FileCtx, Layer, WorkspaceCtx};
use crate::rules;
use crate::types::{LintResult, Severity, Violation};

use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Debug, Error)]
pub enum AnalyzerError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error in {path}: {message}")]
    Parse { path: PathBuf, message: String },

    #[error("Invalid glob pattern: {0}")]
    Glob(#[from] glob::PatternError),

    #[error("Configuration error: {0}")]
    Config(#[from] crate::config::ConfigError),

    #[error("TOML parse error in {path}: {message}")]
    TomlParse { path: PathBuf, message: String },
}

pub struct Analyzer {
    root: PathBuf,
    config: Config,
    exclude_patterns: Vec<String>,
    rule_filter: Option<String>,
}

impl Analyzer {
    #[must_use]
    pub fn new(root: PathBuf, config: Config) -> Self {
        let mut exclude_patterns = config.analyzer.exclude.clone();
        if exclude_patterns.is_empty() {
            exclude_patterns.push("**/target/**".to_string());
        }
        Self {
            root,
            config,
            exclude_patterns,
            rule_filter: None,
        }
    }

    #[must_use]
    pub fn with_rule_filter(mut self, filter: Option<&str>) -> Self {
        self.rule_filter = filter.map(String::from);
        self
    }

    pub fn analyze(&self) -> Result<LintResult, AnalyzerError> {
        info!("Starting analysis at {:?}", self.root);

        let mut result = LintResult::new();

        // Phase 1: Workspace structure analysis
        let ws_ctx = self.build_workspace_ctx()?;
        if self.should_run_rule(rules::layer_dep::NAME) {
            let mut violations = rules::check_layer_dep(&ws_ctx);
            self.apply_severity(rules::layer_dep::NAME, &mut violations);
            result.violations.extend(violations);
        }

        // Phase 2: Per-file AST analysis
        let files = self.discover_files()?;
        info!("Found {} files to analyze", files.len());

        let no_unwrap = rules::NoUnwrap {
            severity: self
                .config
                .rule_severity(rules::no_unwrap::NAME)
                .unwrap_or(Severity::Error),
        };

        let no_panic = rules::NoPanicInLib {
            allow_in_tests: self
                .config
                .rules
                .get(rules::no_panic::NAME)
                .map_or(true, |c| c.get_bool("allow_in_tests", true)),
            severity: self
                .config
                .rule_severity(rules::no_panic::NAME)
                .unwrap_or(Severity::Error),
        };

        for file_path in &files {
            match self.analyze_file(file_path, &no_unwrap, &no_panic) {
                Ok(violations) => {
                    result.violations.extend(violations);
                    result.files_checked += 1;
                }
                Err(AnalyzerError::Parse { path, message }) => {
                    warn!("Failed to parse {}: {}", path.display(), message);
                }
                Err(e) => return Err(e),
            }
        }

        result
            .violations
            .sort_by(|a, b| a.location.cmp(&b.location));

        info!(
            "Analysis complete: {} violations in {} files",
            result.violations.len(),
            result.files_checked
        );

        Ok(result)
    }

    fn analyze_file(
        &self,
        path: &Path,
        no_unwrap: &rules::NoUnwrap,
        no_panic: &rules::NoPanicInLib,
    ) -> Result<Vec<Violation>, AnalyzerError> {
        debug!("Analyzing: {}", path.display());

        let content = std::fs::read_to_string(path)?;
        let ast = syn::parse_file(&content).map_err(|e| AnalyzerError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;

        let ctx = FileCtx::new(path, &content, &self.root);
        let mut violations = Vec::new();

        if self.should_run_rule(rules::no_unwrap::NAME) {
            violations.extend(no_unwrap.check(&ctx, &ast));
        }
        if self.should_run_rule(rules::no_panic::NAME) {
            violations.extend(no_panic.check(&ctx, &ast));
        }

        Ok(violations)
    }

    fn build_workspace_ctx(&self) -> Result<WorkspaceCtx, AnalyzerError> {
        let mut members = Vec::new();
        let mut dep_graph = DepGraph::default();

        let cargo_files = self.discover_cargo_files()?;

        for cargo_path in &cargo_files {
            let content = std::fs::read_to_string(cargo_path).map_err(AnalyzerError::Io)?;

            let table: toml::Table =
                toml::from_str(&content).map_err(|e| AnalyzerError::TomlParse {
                    path: cargo_path.clone(),
                    message: e.to_string(),
                })?;

            let Some(name) = table
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
            else {
                continue;
            };

            let layer = Layer::from_crate_name(name);
            let crate_dir = cargo_path
                .parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

            let mut dependencies = Vec::new();

            // [dependencies] セクションから内部 crate 依存を抽出
            if let Some(deps) = table.get("dependencies").and_then(|d| d.as_table()) {
                for dep_name in deps.keys() {
                    if Layer::from_crate_name(dep_name).is_some() {
                        dependencies.push(dep_name.clone());
                        dep_graph.add_edge(name, dep_name);
                    }
                }
            }

            members.push(CrateMeta {
                name: name.to_string(),
                path: crate_dir,
                layer,
                dependencies,
            });
        }

        Ok(WorkspaceCtx {
            root: self.root.clone(),
            members,
            dep_graph,
        })
    }

    fn discover_files(&self) -> Result<Vec<PathBuf>, AnalyzerError> {
        let pattern = format!("{}/**/*.rs", self.root.display());
        let mut files = Vec::new();

        for entry in glob::glob(&pattern)? {
            let path = entry.map_err(|e| AnalyzerError::Io(e.into_error()))?;
            if !self.should_exclude(&path) {
                files.push(path);
            }
        }

        Ok(files)
    }

    fn discover_cargo_files(&self) -> Result<Vec<PathBuf>, AnalyzerError> {
        let pattern = format!("{}/crates/*/Cargo.toml", self.root.display());
        let mut files = Vec::new();

        for entry in glob::glob(&pattern)? {
            let path = entry.map_err(|e| AnalyzerError::Io(e.into_error()))?;
            if !self.should_exclude(&path) {
                files.push(path);
            }
        }

        Ok(files)
    }

    fn should_exclude(&self, path: &Path) -> bool {
        let path_str = path.to_string_lossy();

        for pattern in &self.exclude_patterns {
            if let Ok(glob_pattern) = glob::Pattern::new(pattern) {
                if glob_pattern.matches(&path_str) {
                    return true;
                }
            }
            // glob::Pattern でマッチしなければスキップ
            // 以前のフォールバック（部分文字列マッチ）は誤除外の原因となるため廃止
        }

        false
    }

    fn should_run_rule(&self, rule_name: &str) -> bool {
        if let Some(ref filter) = self.rule_filter {
            if filter != rule_name {
                return false;
            }
        }
        self.config.is_rule_enabled(rule_name)
    }

    fn apply_severity(&self, rule_name: &str, violations: &mut [Violation]) {
        if let Some(severity) = self.config.rule_severity(rule_name) {
            for v in violations.iter_mut() {
                v.severity = severity;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_target_dir() {
        let analyzer = Analyzer::new(PathBuf::from("."), Config::default());
        assert!(analyzer.should_exclude(Path::new("/foo/target/debug/main.rs")));
        assert!(!analyzer.should_exclude(Path::new("/foo/src/lib.rs")));
    }

    #[test]
    fn exclude_does_not_false_positive() {
        let analyzer = Analyzer::new(PathBuf::from("."), Config::default());
        // "target" を含むがディレクトリパスではないケース
        assert!(!analyzer.should_exclude(Path::new("/foo/src/target_utils.rs")));
        assert!(!analyzer.should_exclude(Path::new("/foo/src/retarget.rs")));
    }
}
