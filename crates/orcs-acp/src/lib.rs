//! ORCS ACP - Agent Client Protocol bridge for ORCS.
//!
//! Bridges the ORCS runtime with ACP-compatible editors (Zed, JetBrains, etc.)
//! over JSON-RPC 2.0 via stdio.

mod agent;
mod convert;
mod server;

pub use server::run_acp_server;
