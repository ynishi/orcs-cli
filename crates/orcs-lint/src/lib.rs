#![forbid(unsafe_code)]

pub mod allowance;
pub mod analyzer;
pub mod config;
pub mod context;
pub mod rules;
pub mod types;

pub use analyzer::Analyzer;
pub use config::Config;
pub use types::{LintResult, Severity, Violation};
