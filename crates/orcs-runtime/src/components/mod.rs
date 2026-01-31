//! Builtin Components for ORCS runtime.
//!
//! These components provide core functionality:
//!
//! - [`HilComponent`] - Human-in-the-Loop approval management
//! - [`EchoWithHilComponent`] - Echo with HIL integration (test/example)
//! - [`NoopComponent`] - Minimal component for channel binding

mod echo_with_hil;
mod hil;
mod noop;

pub use echo_with_hil::{DecoratorConfig, EchoWithHilComponent};
pub use hil::{ApprovalRequest, ApprovalResult, HilComponent};
pub use noop::NoopComponent;
