use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Info => write!(f, "info"),
            Self::Warning => write!(f, "warning"),
            Self::Error => write!(f, "error"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Location {
    pub file: PathBuf,
    pub line: usize,
    pub column: usize,
    pub offset: usize,
    pub length: usize,
}

impl Location {
    #[must_use]
    pub fn from_span(file: PathBuf, span: proc_macro2::Span) -> Self {
        let start = span.start();
        Self {
            file,
            line: start.line,
            column: start.column + 1,
            offset: 0,
            length: 0,
        }
    }

    #[must_use]
    pub fn new(file: PathBuf, line: usize, column: usize) -> Self {
        Self {
            file,
            line,
            column,
            offset: 0,
            length: 0,
        }
    }

    #[must_use]
    pub fn with_span(mut self, offset: usize, length: usize) -> Self {
        self.offset = offset;
        self.length = length;
        self
    }
}

impl PartialOrd for Location {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Location {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.file
            .cmp(&other.file)
            .then(self.line.cmp(&other.line))
            .then(self.column.cmp(&other.column))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub message: String,
    pub replacement: Option<Replacement>,
}

impl Suggestion {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            replacement: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replacement {
    pub location: Location,
    pub new_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Violation {
    pub rule: String,
    pub severity: Severity,
    pub location: Location,
    pub message: String,
    pub suggestion: Option<Suggestion>,
}

impl Violation {
    #[must_use]
    pub fn new(
        rule: impl Into<String>,
        severity: Severity,
        location: Location,
        message: impl Into<String>,
    ) -> Self {
        Self {
            rule: rule.into(),
            severity,
            location,
            message: message.into(),
            suggestion: None,
        }
    }

    #[must_use]
    pub fn with_suggestion(mut self, suggestion: Suggestion) -> Self {
        self.suggestion = Some(suggestion);
        self
    }
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}:{}: {} [{}] {}",
            self.location.file.display(),
            self.location.line,
            self.location.column,
            self.severity,
            self.rule,
            self.message
        )
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LintResult {
    pub violations: Vec<Violation>,
    pub files_checked: usize,
}

impl LintResult {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.violations
            .iter()
            .any(|v| v.severity == Severity::Error)
    }

    #[must_use]
    pub fn count_by_severity(&self) -> (usize, usize, usize) {
        let mut errors = 0;
        let mut warnings = 0;
        let mut infos = 0;
        for v in &self.violations {
            match v.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                Severity::Info => infos += 1,
            }
        }
        (errors, warnings, infos)
    }

    pub fn print_report(&self) {
        let (errors, warnings, infos) = self.count_by_severity();

        for v in &self.violations {
            eprintln!("{v}");
            if let Some(s) = &v.suggestion {
                eprintln!("  = help: {}", s.message);
            }
        }

        eprintln!(
            "\n{} error(s), {} warning(s), {} info(s) in {} file(s)",
            errors, warnings, infos, self.files_checked
        );
    }
}
