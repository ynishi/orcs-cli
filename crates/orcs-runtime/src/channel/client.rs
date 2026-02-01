//! ClientChannel - Channel with Human interaction capabilities.
//!
//! A [`ClientChannel`] extends [`BaseChannel`] with IO bridging for Human
//! interaction. It owns an [`IOBridge`] for View-Model communication.
//!
//! # Architecture
//!
//! ```text
//! ClientChannel
//! ├── base: BaseChannel      ← State, tree, config, principal
//! │     ├── id: ChannelId
//! │     ├── principal: Principal
//! │     ├── state: ChannelState
//! │     ├── config: ChannelConfig
//! │     ├── parent: Option<ChannelId>
//! │     └── children: HashSet<ChannelId>
//! │
//! └── io_bridge: IOBridge    ← View-Model communication
//!       ├── io_port: IOPort
//!       └── parser: InputParser
//! ```
//!
//! # Example
//!
//! ```
//! use orcs_runtime::channel::{ClientChannel, ChannelConfig, ChannelCore};
//! use orcs_runtime::io::IOPort;
//! use orcs_types::{ChannelId, Principal, PrincipalId};
//!
//! let id = ChannelId::new();
//! let principal = Principal::User(PrincipalId::new());
//! let (port, input_handle, output_handle) = IOPort::with_defaults(id);
//!
//! let channel = ClientChannel::new(id, ChannelConfig::interactive(), principal, port);
//!
//! assert!(channel.is_running());
//! assert_eq!(channel.priority(), 255);
//! ```

use super::config::ChannelConfig;
use super::traits::{ChannelCore, ChannelMut};
use super::BaseChannel;
use crate::components::IOBridge;
use crate::io::IOPort;
use orcs_types::{ChannelId, Principal};
use std::collections::HashSet;

use super::ChannelState;

/// Channel with Human interaction capabilities.
///
/// `ClientChannel` wraps a [`BaseChannel`] and adds IO bridging via [`IOBridge`].
/// It implements [`ChannelCore`] and [`ChannelMut`] by delegating to the base channel.
///
/// # Principal Ownership
///
/// The Principal is stored in the base channel and passed to IOBridge methods
/// when converting input to Signals. This ensures consistent Principal usage
/// across the channel.
#[derive(Debug)]
pub struct ClientChannel {
    /// Base channel for state and tree management.
    base: BaseChannel,
    /// IO bridge for View-Model communication.
    io_bridge: IOBridge,
}

impl ClientChannel {
    /// Creates a new ClientChannel.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this channel
    /// * `config` - Configuration controlling channel behavior
    /// * `principal` - Principal for scope resolution (Human user)
    /// * `io_port` - IO port for View communication
    #[must_use]
    pub fn new(
        id: ChannelId,
        config: ChannelConfig,
        principal: Principal,
        io_port: IOPort,
    ) -> Self {
        let base = BaseChannel::new(id, None, config, principal);
        let io_bridge = IOBridge::new(io_port);
        Self { base, io_bridge }
    }

    /// Creates a new ClientChannel with a parent.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this channel
    /// * `parent` - Parent channel ID
    /// * `config` - Configuration controlling channel behavior
    /// * `principal` - Principal for scope resolution
    /// * `ancestor_path` - Pre-computed path: [parent, grandparent, ..., root]
    /// * `io_port` - IO port for View communication
    #[must_use]
    pub fn with_parent(
        id: ChannelId,
        parent: ChannelId,
        config: ChannelConfig,
        principal: Principal,
        ancestor_path: Vec<ChannelId>,
        io_port: IOPort,
    ) -> Self {
        let base = BaseChannel::new_with_ancestors(id, parent, config, principal, ancestor_path);
        let io_bridge = IOBridge::new(io_port);
        Self { base, io_bridge }
    }

    /// Returns a reference to the IO bridge.
    #[must_use]
    pub fn io_bridge(&self) -> &IOBridge {
        &self.io_bridge
    }

    /// Returns a mutable reference to the IO bridge.
    #[must_use]
    pub fn io_bridge_mut(&mut self) -> &mut IOBridge {
        &mut self.io_bridge
    }

    /// Returns a reference to the base channel.
    #[must_use]
    pub fn base(&self) -> &BaseChannel {
        &self.base
    }

    /// Returns a mutable reference to the base channel.
    #[must_use]
    pub fn base_mut(&mut self) -> &mut BaseChannel {
        &mut self.base
    }
}

// === ChannelCore trait implementation (delegate to base) ===

impl ChannelCore for ClientChannel {
    fn id(&self) -> ChannelId {
        self.base.id()
    }

    fn principal(&self) -> &Principal {
        self.base.principal()
    }

    fn state(&self) -> &ChannelState {
        self.base.state()
    }

    fn config(&self) -> &ChannelConfig {
        self.base.config()
    }

    fn parent(&self) -> Option<ChannelId> {
        self.base.parent()
    }

    fn children(&self) -> &HashSet<ChannelId> {
        self.base.children()
    }

    fn ancestor_path(&self) -> &[ChannelId] {
        self.base.ancestor_path()
    }
}

// === ChannelMut trait implementation (delegate to base) ===

impl ChannelMut for ClientChannel {
    fn complete(&mut self) -> bool {
        self.base.complete()
    }

    fn abort(&mut self, reason: String) -> bool {
        self.base.abort(reason)
    }

    fn pause(&mut self) -> bool {
        self.base.pause()
    }

    fn resume(&mut self) -> bool {
        self.base.resume()
    }

    fn await_approval(&mut self, request_id: String) -> bool {
        self.base.await_approval(request_id)
    }

    fn resolve_approval(&mut self) -> Option<String> {
        self.base.resolve_approval()
    }

    fn add_child(&mut self, id: ChannelId) {
        self.base.add_child(id)
    }

    fn remove_child(&mut self, id: &ChannelId) {
        self.base.remove_child(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::PrincipalId;

    fn test_principal() -> Principal {
        Principal::User(PrincipalId::new())
    }

    fn setup() -> (
        ClientChannel,
        crate::io::IOInputHandle,
        crate::io::IOOutputHandle,
    ) {
        let id = ChannelId::new();
        let (port, input_handle, output_handle) = IOPort::with_defaults(id);
        let channel = ClientChannel::new(id, ChannelConfig::interactive(), test_principal(), port);
        (channel, input_handle, output_handle)
    }

    #[test]
    fn client_channel_creation() {
        let (channel, _, _) = setup();

        assert!(channel.is_running());
        assert_eq!(channel.priority(), 255);
        assert!(channel.can_spawn());
        assert!(channel.parent().is_none());
    }

    #[test]
    fn client_channel_principal() {
        let id = ChannelId::new();
        let principal = Principal::System;
        let (port, _, _) = IOPort::with_defaults(id);
        let channel = ClientChannel::new(id, ChannelConfig::default(), principal.clone(), port);

        assert_eq!(channel.principal(), &principal);
    }

    #[test]
    fn client_channel_state_transitions() {
        let (mut channel, _, _) = setup();

        assert!(channel.is_running());

        assert!(channel.pause());
        assert!(channel.is_paused());

        assert!(channel.resume());
        assert!(channel.is_running());

        assert!(channel.complete());
        assert!(channel.is_terminal());
    }

    #[test]
    fn client_channel_io_bridge_access() {
        let (channel, _, _) = setup();

        // Can access IO bridge
        let bridge = channel.io_bridge();
        assert_eq!(bridge.channel_id(), channel.id());
    }

    #[test]
    fn client_channel_with_parent() {
        let parent_id = ChannelId::new();
        let id = ChannelId::new();
        let (port, _, _) = IOPort::with_defaults(id);

        let channel = ClientChannel::with_parent(
            id,
            parent_id,
            ChannelConfig::default(),
            test_principal(),
            vec![parent_id],
            port,
        );

        assert_eq!(channel.parent(), Some(parent_id));
        assert!(channel.is_descendant_of(parent_id));
        assert_eq!(channel.depth(), 1);
    }

    #[test]
    fn client_channel_children() {
        let (mut channel, _, _) = setup();
        let child1 = ChannelId::new();
        let child2 = ChannelId::new();

        assert!(!channel.has_children());

        channel.add_child(child1);
        channel.add_child(child2);
        assert!(channel.has_children());
        assert_eq!(channel.children().len(), 2);

        channel.remove_child(&child1);
        assert_eq!(channel.children().len(), 1);
    }

    #[test]
    fn client_channel_debug() {
        let (channel, _, _) = setup();
        let debug_str = format!("{:?}", channel);
        assert!(debug_str.contains("ClientChannel"));
    }
}
