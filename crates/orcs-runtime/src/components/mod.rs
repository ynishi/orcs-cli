//! Builtin Components for ORCS runtime.
//!
//! These components provide core functionality:
//!
//! - [`HilComponent`] - Human-in-the-Loop approval management
//! - [`IOBridge`] - Bridge between View and Model layers
//! - [`EchoWithHilComponent`] - Echo with HIL integration (test/example)
//! - [`NoopComponent`] - Minimal component for channel binding

mod echo_with_hil;
mod hil;
mod io_bridge;
mod noop;

pub use echo_with_hil::{DecoratorConfig, EchoWithHilComponent};
pub use hil::{ApprovalRequest, ApprovalResult, HilComponent};
pub use io_bridge::IOBridge;
pub use noop::NoopComponent;
