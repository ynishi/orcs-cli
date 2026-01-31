//! Core types for ORCS CLI.
//!
//! This crate provides foundational identifier types for the ORCS
//! (Orchestrated Runtime for Collaborative Systems) architecture.
//!
//! # Crate Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Plugin SDK Layer                          │
//! │  (External, SemVer stable, safe to depend on)               │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-types     : ID types, Principal, ErrorCode  ◄── HERE   │
//! │  orcs-event     : Signal, Request, Event                    │
//! │  orcs-component : Component trait (WIT target)              │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    Runtime Layer                             │
//! │  (Internal implementation, NOT for plugins)                  │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-runtime   : auth, channel, engine                     │
//! └─────────────────────────────────────────────────────────────┘
//!                               ↓
//! ┌─────────────────────────────────────────────────────────────┐
//! │                   Frontend Layer                             │
//! │  (CLI, GUI, Network interfaces)                              │
//! ├─────────────────────────────────────────────────────────────┤
//! │  orcs-cli       : Command-line interface                    │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Why Plugin SDK?
//!
//! This crate is part of the **Plugin SDK** layer because:
//!
//! - **SemVer stable**: API changes follow semantic versioning
//! - **Minimal dependencies**: Only what plugins truly need
//! - **WIT compatible**: Types can be exported to WASM components
//! - **Implementation freedom**: Runtime can change without breaking plugins
//!
//! # Event Architecture
//!
//! ORCS uses an EventBus-based architecture where all communication
//! between Components flows through unified message types:
//!
//! - **Request/Response**: Synchronous queries between Components
//! - **Event**: Asynchronous broadcasts (no response expected)
//! - **Signal**: Human control interrupts (highest priority)
//!
//! # Identifier Design
//!
//! All identifiers are UUID-based for:
//!
//! - **Network compatibility**: Safe to transmit across processes/machines
//! - **Cloud readiness**: Globally unique without coordination
//! - **Serialization**: First-class serde support
//!
//! # Example
//!
//! ```
//! use orcs_types::{ComponentId, ChannelId, RequestId, EventId};
//!
//! // Builtin components have deterministic UUIDs
//! let llm = ComponentId::builtin("llm");
//! let llm2 = ComponentId::builtin("llm");
//! assert_eq!(llm, llm2);  // Same UUID
//!
//! // Custom components get random UUIDs
//! let plugin = ComponentId::new("plugin", "my-tool");
//!
//! // Channel for parallel execution context
//! let channel = ChannelId::new();
//!
//! // Request/Response tracking
//! let req_id = RequestId::new();
//!
//! // Event broadcast tracking
//! let evt_id = EventId::new();
//! ```

mod construct;
mod error;
mod id;
mod principal;
mod scope;

pub use construct::TryNew;
pub use error::{assert_error_code, assert_error_codes, ErrorCode};
pub use id::{ChannelId, ComponentId, EventId, PrincipalId, RequestId};
pub use principal::Principal;
pub use scope::SignalScope;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_id_creation() {
        let id = ComponentId::new("test", "component");
        assert_eq!(id.namespace, "test");
        assert_eq!(id.name, "component");
        assert_eq!(id.fqn(), "test::component");
    }

    #[test]
    fn component_id_builtin_deterministic() {
        let id1 = ComponentId::builtin("llm");
        let id2 = ComponentId::builtin("llm");
        assert_eq!(id1.namespace, "builtin");
        assert_eq!(id1.name, "llm");
        // Same name produces same UUID (deterministic)
        assert_eq!(id1.uuid, id2.uuid);
        assert_eq!(id1, id2);
    }

    #[test]
    fn component_id_builtin_different_names() {
        let id1 = ComponentId::builtin("llm");
        let id2 = ComponentId::builtin("tool");
        // Different names produce different UUIDs
        assert_ne!(id1.uuid, id2.uuid);
    }

    #[test]
    fn component_id_display() {
        let id = ComponentId::builtin("test");
        let display = format!("{id}");
        assert!(display.starts_with("builtin::test@"));
        assert!(display.contains(&id.uuid.to_string()));
    }

    #[test]
    fn component_id_new_random() {
        let id1 = ComponentId::new("test", "comp");
        let id2 = ComponentId::new("test", "comp");
        // new() produces random UUIDs
        assert_ne!(id1.uuid, id2.uuid);
        assert_eq!(id1.fqn(), id2.fqn());
    }

    #[test]
    fn component_id_fqn_eq() {
        let id1 = ComponentId::new("test", "comp");
        let id2 = ComponentId::new("test", "comp");
        let id3 = ComponentId::new("test", "other");
        // Different UUIDs but same FQN
        assert_ne!(id1, id2);
        assert!(id1.fqn_eq(&id2));
        assert!(!id1.fqn_eq(&id3));
    }

    #[test]
    fn component_id_matches() {
        let id = ComponentId::builtin("llm");
        assert!(id.matches("builtin", "llm"));
        assert!(!id.matches("builtin", "tool"));
        assert!(!id.matches("custom", "llm"));
    }

    #[test]
    fn component_id_is_builtin() {
        let builtin = ComponentId::builtin("llm");
        let custom = ComponentId::new("custom", "llm");
        assert!(builtin.is_builtin());
        assert!(!custom.is_builtin());
    }

    #[test]
    fn channel_id_uniqueness() {
        let id1 = ChannelId::new();
        let id2 = ChannelId::new();
        assert_ne!(id1, id2);
    }

    // NOTE: ChannelId does not implement Default intentionally.
    // See id.rs for rationale.

    #[test]
    fn channel_id_display() {
        let id = ChannelId::new();
        let display = format!("{id}");
        assert!(display.starts_with("ch:"));
        assert!(display.contains(&id.uuid().to_string()));
    }

    #[test]
    fn channel_id_uuid() {
        let id = ChannelId::new();
        assert_eq!(id.uuid(), id.0);
    }

    #[test]
    fn request_id_display() {
        let id = RequestId::new();
        let display = format!("{id}");
        assert!(display.starts_with("req:"));
        assert!(display.contains(&id.uuid().to_string()));
    }

    // NOTE: RequestId does not implement Default intentionally.
    // See id.rs for rationale.

    #[test]
    fn event_id_creation() {
        let id = EventId::new();
        let display = format!("{id}");
        assert!(display.starts_with("evt:"));
        assert!(display.contains(&id.uuid().to_string()));
    }

    #[test]
    fn event_id_default() {
        let id1 = EventId::default();
        let id2 = EventId::default();
        assert_ne!(id1, id2);
    }

    #[test]
    fn event_id_uniqueness() {
        let id1 = EventId::new();
        let id2 = EventId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn principal_id_creation() {
        let id = PrincipalId::new();
        let display = format!("{id}");
        assert!(display.starts_with("principal:"));
    }

    #[test]
    fn principal_id_uniqueness() {
        let id1 = PrincipalId::new();
        let id2 = PrincipalId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn principal_id_default() {
        let id1 = PrincipalId::default();
        let id2 = PrincipalId::default();
        assert_ne!(id1, id2);
    }

    #[test]
    fn principal_id_uuid() {
        let id = PrincipalId::new();
        assert_eq!(id.uuid(), id.0);
    }
}
