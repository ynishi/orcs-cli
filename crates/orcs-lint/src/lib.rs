//! Architecture lint rules for the ORCS workspace.
//!
//! Provides static analysis rules enforced across all crates:
//!
//! | Rule | ID | Description |
//! |------|----|-------------|
//! | `NoUnwrap` | OL002 | `.unwrap()` forbidden everywhere; `.expect()` forbidden in production code |
//! | `NoPanicInLib` | OL003 | `panic!`/`todo!`/`unimplemented!` forbidden in library code |
//! | `LayerDep` | OL001 | Cargo.toml dependency direction enforcement (upper layers cannot be depended on by lower layers) |
//!
//! # Usage
//!
//! ```bash
//! cargo run -p orcs-lint -- check .
//! ```

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
