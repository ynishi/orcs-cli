//! Signal types for control and interrupt.
//!
//! Signals are high-priority control messages that can interrupt
//! ongoing operations. Unlike Requests, Signals do not expect
//! a response and are processed immediately.
//!
//! # Signal Flow
//!
//! ```text
//! ┌─────────────┐  Signal    ┌─────────────┐
//! │   Session   │ ─────────► │   EventBus  │ ─────► Components
//! │ (Principal) │            │             │
//! └─────────────┘            └─────────────┘
//! ```
//!
//! # Permission Model
//!
//! Signal permission is **not** embedded in the signal itself.
//! The runtime layer's Session carries the privilege level,
//! and permission checking happens at the EventBus layer.
//!
//! This separation enables:
//!
//! - Same principal with different privilege levels over time
//! - Policy changes without modifying signal types
//! - Clear audit trails (who sent what, with what privilege)
//!
//! # Example
//!
//! ```
//! use orcs_event::{Signal, SignalKind};
//! use orcs_types::{Principal, ChannelId, PrincipalId, SignalScope};
//!
//! // Create a cancel signal for a specific channel
//! let principal = Principal::User(PrincipalId::new());
//! let channel = ChannelId::new();
//! let signal = Signal::cancel(channel, principal.clone());
//!
//! // Create a veto signal (requires elevated privilege at runtime)
//! let veto = Signal::veto(principal);
//! ```

use orcs_types::{ChannelId, Principal, SignalScope};
use serde::{Deserialize, Serialize};

/// The type of signal being sent.
///
/// # Variants
///
/// | Kind | Scope | Effect |
/// |------|-------|--------|
/// | `Veto` | Global only | Stop all operations |
/// | `Cancel` | Channel | Stop operations in channel |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalKind {
    /// Global stop - requires elevated privilege.
    ///
    /// Stops all operations across all channels.
    /// This is the "emergency stop" for the entire system.
    Veto,

    /// Cancel operations in a specific scope.
    ///
    /// Stops operations in the targeted channel(s).
    Cancel,
}

/// A control signal message.
///
/// Signals are used for immediate control operations like
/// stopping or canceling work in progress.
///
/// # Permission Checking
///
/// The signal itself does not contain permission information.
/// Permission is determined by:
///
/// 1. The runtime layer's Session that sends the signal
/// 2. The [`SignalScope`] being targeted
/// 3. The policy configured in the EventBus
///
/// # Why No Default?
///
/// **DO NOT implement `Default` for Signal.**
///
/// A signal requires:
/// - A valid [`SignalKind`]
/// - A valid [`SignalScope`]
/// - A valid [`Principal`] (who is sending)
///
/// There is no sensible default for any of these.
///
/// # Example
///
/// ```
/// use orcs_event::{Signal, SignalKind};
/// use orcs_types::{Principal, ChannelId, PrincipalId, SignalScope};
///
/// let principal = Principal::User(PrincipalId::new());
/// let channel = ChannelId::new();
///
/// let signal = Signal::new(
///     SignalKind::Cancel,
///     SignalScope::Channel(channel),
///     principal,
/// );
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    /// What type of signal.
    pub kind: SignalKind,
    /// What scope is affected.
    pub scope: SignalScope,
    /// Who is sending the signal.
    pub source: Principal,
}

impl Signal {
    /// Creates a new signal.
    ///
    /// # Arguments
    ///
    /// * `kind` - The type of signal
    /// * `scope` - What scope is affected
    /// * `source` - Who is sending the signal
    ///
    /// # Note
    ///
    /// This does **not** check permissions. Permission checking
    /// is done at the EventBus layer using the Session's privilege.
    #[must_use]
    pub fn new(kind: SignalKind, scope: SignalScope, source: Principal) -> Self {
        Self {
            kind,
            scope,
            source,
        }
    }

    /// Creates a Veto (global stop) signal.
    ///
    /// **Requires elevated privilege to execute.**
    ///
    /// # Arguments
    ///
    /// * `source` - Who is sending the signal
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_event::Signal;
    /// use orcs_types::{Principal, PrincipalId};
    ///
    /// let principal = Principal::User(PrincipalId::new());
    /// let signal = Signal::veto(principal);
    ///
    /// assert!(signal.is_veto());
    /// assert!(signal.is_global());
    /// ```
    #[must_use]
    pub fn veto(source: Principal) -> Self {
        Self {
            kind: SignalKind::Veto,
            scope: SignalScope::Global,
            source,
        }
    }

    /// Creates a Cancel signal for a specific channel.
    ///
    /// # Arguments
    ///
    /// * `channel` - The channel to cancel
    /// * `source` - Who is sending the signal
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_event::Signal;
    /// use orcs_types::{Principal, ChannelId, PrincipalId};
    ///
    /// let principal = Principal::User(PrincipalId::new());
    /// let channel = ChannelId::new();
    /// let signal = Signal::cancel(channel, principal);
    ///
    /// assert!(!signal.is_veto());
    /// assert!(signal.affects_channel(channel));
    /// ```
    #[must_use]
    pub fn cancel(channel: ChannelId, source: Principal) -> Self {
        Self {
            kind: SignalKind::Cancel,
            scope: SignalScope::Channel(channel),
            source,
        }
    }

    /// Returns `true` if this is a Veto signal.
    #[must_use]
    pub fn is_veto(&self) -> bool {
        matches!(self.kind, SignalKind::Veto)
    }

    /// Returns `true` if this signal has Global scope.
    #[must_use]
    pub fn is_global(&self) -> bool {
        matches!(self.scope, SignalScope::Global)
    }

    /// Returns `true` if this signal affects the given channel.
    ///
    /// Global signals affect all channels.
    #[must_use]
    pub fn affects_channel(&self, channel: ChannelId) -> bool {
        self.scope.affects(channel)
    }
}

/// Response from a component after receiving a signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalResponse {
    /// Signal was handled successfully.
    Handled,
    /// Signal was ignored (out of scope or not relevant).
    Ignored,
    /// Component is aborting due to the signal.
    Abort,
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_types::{ComponentId, PrincipalId};

    fn test_user() -> Principal {
        Principal::User(PrincipalId::new())
    }

    fn test_component() -> Principal {
        Principal::Component(ComponentId::builtin("test"))
    }

    #[test]
    fn veto_signal() {
        let signal = Signal::veto(test_user());
        assert!(signal.is_veto());
        assert!(signal.is_global());
        assert_eq!(signal.scope, SignalScope::Global);
    }

    #[test]
    fn cancel_signal() {
        let channel = ChannelId::new();
        let signal = Signal::cancel(channel, test_user());

        assert!(!signal.is_veto());
        assert!(!signal.is_global());
        assert_eq!(signal.scope, SignalScope::Channel(channel));
    }

    #[test]
    fn cancel_from_component() {
        let channel = ChannelId::new();
        let signal = Signal::cancel(channel, test_component());

        assert!(signal.source.is_component());
    }

    #[test]
    fn affects_channel_global() {
        let channel1 = ChannelId::new();
        let channel2 = ChannelId::new();

        let veto = Signal::veto(test_user());
        assert!(veto.affects_channel(channel1));
        assert!(veto.affects_channel(channel2));
    }

    #[test]
    fn affects_channel_specific() {
        let channel1 = ChannelId::new();
        let channel2 = ChannelId::new();

        let cancel = Signal::cancel(channel1, test_user());
        assert!(cancel.affects_channel(channel1));
        assert!(!cancel.affects_channel(channel2));
    }

    #[test]
    fn signal_new() {
        let channel = ChannelId::new();
        let signal = Signal::new(
            SignalKind::Cancel,
            SignalScope::Channel(channel),
            test_user(),
        );

        assert!(!signal.is_veto());
        assert!(signal.affects_channel(channel));
    }

    #[test]
    fn signal_response_variants() {
        assert_eq!(SignalResponse::Handled, SignalResponse::Handled);
        assert_eq!(SignalResponse::Ignored, SignalResponse::Ignored);
        assert_eq!(SignalResponse::Abort, SignalResponse::Abort);
        assert_ne!(SignalResponse::Handled, SignalResponse::Ignored);
    }

    #[test]
    fn signal_kind_variants() {
        assert_eq!(SignalKind::Veto, SignalKind::Veto);
        assert_eq!(SignalKind::Cancel, SignalKind::Cancel);
        assert_ne!(SignalKind::Veto, SignalKind::Cancel);
    }

    #[test]
    fn signal_scope_equality() {
        let ch1 = ChannelId::new();
        let ch2 = ChannelId::new();

        assert_eq!(SignalScope::Global, SignalScope::Global);
        assert_eq!(SignalScope::Channel(ch1), SignalScope::Channel(ch1));
        assert_ne!(SignalScope::Channel(ch1), SignalScope::Channel(ch2));
        assert_ne!(SignalScope::Global, SignalScope::Channel(ch1));
    }
}
