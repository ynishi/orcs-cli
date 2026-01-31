//! Builtin Components for ORCS runtime.
//!
//! These components provide core functionality:
//!
//! - [`HilComponent`] - Human-in-the-Loop approval management
//! - [`EchoWithHilComponent`] - Echo with HIL integration (test/example)

mod echo_with_hil;
mod hil;

pub use echo_with_hil::EchoWithHilComponent;
pub use hil::{ApprovalRequest, ApprovalResult, HilComponent};
