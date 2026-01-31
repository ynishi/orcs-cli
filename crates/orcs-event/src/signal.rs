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
/// | `Pause` | Channel/Component | Pause operations |
/// | `Resume` | Channel/Component | Resume paused operations |
/// | `Steer` | Channel | Modify direction with message |
/// | `Approve` | Component | Approve pending HIL request |
/// | `Reject` | Component | Reject pending HIL request |
/// | `Modify` | Component | Approve with modifications |
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalKind {
    // === Stop signals ===
    /// Global stop - requires elevated privilege.
    ///
    /// Stops all operations across all channels.
    /// This is the "emergency stop" for the entire system.
    Veto,

    /// Cancel operations in a specific scope.
    ///
    /// Stops operations in the targeted channel(s).
    Cancel,

    // === Control signals ===
    /// Pause operations in a specific scope.
    ///
    /// Paused operations can be resumed with [`Resume`](SignalKind::Resume).
    Pause,

    /// Resume paused operations.
    ///
    /// Resumes operations that were paused with [`Pause`](SignalKind::Pause).
    Resume,

    /// Modify direction with a message.
    ///
    /// Allows Human to steer the execution without stopping.
    Steer {
        /// Instruction for how to change direction.
        message: String,
    },

    // === HIL signals ===
    /// Approve a pending HIL request.
    ///
    /// Sent by Human to approve an operation that requires approval.
    Approve {
        /// ID of the approval request to approve.
        approval_id: String,
    },

    /// Reject a pending HIL request.
    ///
    /// Sent by Human to reject an operation that requires approval.
    Reject {
        /// ID of the approval request to reject.
        approval_id: String,
        /// Optional reason for rejection.
        reason: Option<String>,
    },

    /// Approve with modifications.
    ///
    /// Sent by Human to approve an operation with modified parameters.
    /// This allows Human to adjust the proposed action before execution.
    ///
    /// # Example Use Cases
    ///
    /// - Modify file path before write
    /// - Adjust command arguments before execution
    /// - Change scope of operation
    Modify {
        /// ID of the approval request to modify.
        approval_id: String,
        /// Modified payload to use instead of original.
        modified_payload: serde_json::Value,
    },
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

    /// Creates a Pause signal for a specific channel.
    ///
    /// # Arguments
    ///
    /// * `channel` - The channel to pause
    /// * `source` - Who is sending the signal
    #[must_use]
    pub fn pause(channel: ChannelId, source: Principal) -> Self {
        Self {
            kind: SignalKind::Pause,
            scope: SignalScope::Channel(channel),
            source,
        }
    }

    /// Creates a Resume signal for a specific channel.
    ///
    /// # Arguments
    ///
    /// * `channel` - The channel to resume
    /// * `source` - Who is sending the signal
    #[must_use]
    pub fn resume(channel: ChannelId, source: Principal) -> Self {
        Self {
            kind: SignalKind::Resume,
            scope: SignalScope::Channel(channel),
            source,
        }
    }

    /// Creates a Steer signal for a specific channel.
    ///
    /// # Arguments
    ///
    /// * `channel` - The channel to steer
    /// * `message` - Instruction for how to change direction
    /// * `source` - Who is sending the signal
    #[must_use]
    pub fn steer(channel: ChannelId, message: impl Into<String>, source: Principal) -> Self {
        Self {
            kind: SignalKind::Steer {
                message: message.into(),
            },
            scope: SignalScope::Channel(channel),
            source,
        }
    }

    /// Creates an Approve signal for a pending HIL request.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - ID of the approval request to approve
    /// * `source` - Who is sending the signal (typically Human)
    #[must_use]
    pub fn approve(approval_id: impl Into<String>, source: Principal) -> Self {
        Self {
            kind: SignalKind::Approve {
                approval_id: approval_id.into(),
            },
            scope: SignalScope::Global,
            source,
        }
    }

    /// Creates a Reject signal for a pending HIL request.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - ID of the approval request to reject
    /// * `reason` - Optional reason for rejection
    /// * `source` - Who is sending the signal (typically Human)
    #[must_use]
    pub fn reject(
        approval_id: impl Into<String>,
        reason: Option<String>,
        source: Principal,
    ) -> Self {
        Self {
            kind: SignalKind::Reject {
                approval_id: approval_id.into(),
                reason,
            },
            scope: SignalScope::Global,
            source,
        }
    }

    /// Returns `true` if this is a Pause signal.
    #[must_use]
    pub fn is_pause(&self) -> bool {
        matches!(self.kind, SignalKind::Pause)
    }

    /// Returns `true` if this is a Resume signal.
    #[must_use]
    pub fn is_resume(&self) -> bool {
        matches!(self.kind, SignalKind::Resume)
    }

    /// Returns `true` if this is an Approve signal.
    #[must_use]
    pub fn is_approve(&self) -> bool {
        matches!(self.kind, SignalKind::Approve { .. })
    }

    /// Returns `true` if this is a Reject signal.
    #[must_use]
    pub fn is_reject(&self) -> bool {
        matches!(self.kind, SignalKind::Reject { .. })
    }

    /// Returns `true` if this is a Modify signal.
    #[must_use]
    pub fn is_modify(&self) -> bool {
        matches!(self.kind, SignalKind::Modify { .. })
    }

    /// Returns `true` if this is a HIL response signal (Approve, Reject, or Modify).
    #[must_use]
    pub fn is_hil_response(&self) -> bool {
        self.is_approve() || self.is_reject() || self.is_modify()
    }

    /// Creates a Modify signal for a pending HIL request.
    ///
    /// Approves the request but with modified parameters.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - ID of the approval request to modify
    /// * `modified_payload` - Modified payload to use instead of original
    /// * `source` - Who is sending the signal (typically Human)
    #[must_use]
    pub fn modify(
        approval_id: impl Into<String>,
        modified_payload: serde_json::Value,
        source: Principal,
    ) -> Self {
        Self {
            kind: SignalKind::Modify {
                approval_id: approval_id.into(),
                modified_payload,
            },
            scope: SignalScope::Global,
            source,
        }
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

    // === M2 HIL Signal Tests ===

    #[test]
    fn pause_signal() {
        let channel = ChannelId::new();
        let signal = Signal::pause(channel, test_user());

        assert!(signal.is_pause());
        assert!(!signal.is_resume());
        assert!(signal.affects_channel(channel));
    }

    #[test]
    fn resume_signal() {
        let channel = ChannelId::new();
        let signal = Signal::resume(channel, test_user());

        assert!(signal.is_resume());
        assert!(!signal.is_pause());
        assert!(signal.affects_channel(channel));
    }

    #[test]
    fn steer_signal() {
        let channel = ChannelId::new();
        let signal = Signal::steer(channel, "focus on error handling", test_user());

        if let SignalKind::Steer { message } = &signal.kind {
            assert_eq!(message, "focus on error handling");
        } else {
            panic!("Expected Steer signal");
        }
        assert!(signal.affects_channel(channel));
    }

    #[test]
    fn approve_signal() {
        let signal = Signal::approve("req-123", test_user());

        assert!(signal.is_approve());
        assert!(signal.is_hil_response());
        assert!(!signal.is_reject());

        if let SignalKind::Approve { approval_id } = &signal.kind {
            assert_eq!(approval_id, "req-123");
        } else {
            panic!("Expected Approve signal");
        }
    }

    #[test]
    fn reject_signal_with_reason() {
        let signal = Signal::reject("req-456", Some("dangerous operation".into()), test_user());

        assert!(signal.is_reject());
        assert!(signal.is_hil_response());
        assert!(!signal.is_approve());

        if let SignalKind::Reject {
            approval_id,
            reason,
        } = &signal.kind
        {
            assert_eq!(approval_id, "req-456");
            assert_eq!(reason, &Some("dangerous operation".to_string()));
        } else {
            panic!("Expected Reject signal");
        }
    }

    #[test]
    fn reject_signal_without_reason() {
        let signal = Signal::reject("req-789", None, test_user());

        if let SignalKind::Reject {
            approval_id,
            reason,
        } = &signal.kind
        {
            assert_eq!(approval_id, "req-789");
            assert!(reason.is_none());
        } else {
            panic!("Expected Reject signal");
        }
    }

    #[test]
    fn signal_kind_equality_with_data() {
        assert_eq!(SignalKind::Pause, SignalKind::Pause);
        assert_eq!(SignalKind::Resume, SignalKind::Resume);

        let approve1 = SignalKind::Approve {
            approval_id: "a".into(),
        };
        let approve2 = SignalKind::Approve {
            approval_id: "a".into(),
        };
        let approve3 = SignalKind::Approve {
            approval_id: "b".into(),
        };

        assert_eq!(approve1, approve2);
        assert_ne!(approve1, approve3);
    }

    #[test]
    fn modify_signal() {
        let modified_payload = serde_json::json!({
            "path": "/safe/path/file.txt",
            "content": "modified content"
        });
        let signal = Signal::modify("req-mod-1", modified_payload.clone(), test_user());

        assert!(signal.is_modify());
        assert!(signal.is_hil_response());
        assert!(!signal.is_approve());
        assert!(!signal.is_reject());

        if let SignalKind::Modify {
            approval_id,
            modified_payload: payload,
        } = &signal.kind
        {
            assert_eq!(approval_id, "req-mod-1");
            assert_eq!(payload, &modified_payload);
        } else {
            panic!("Expected Modify signal");
        }
    }

    #[test]
    fn modify_signal_kind_equality() {
        let payload1 = serde_json::json!({"key": "value1"});
        let payload2 = serde_json::json!({"key": "value1"});
        let payload3 = serde_json::json!({"key": "value2"});

        let modify1 = SignalKind::Modify {
            approval_id: "a".into(),
            modified_payload: payload1,
        };
        let modify2 = SignalKind::Modify {
            approval_id: "a".into(),
            modified_payload: payload2,
        };
        let modify3 = SignalKind::Modify {
            approval_id: "a".into(),
            modified_payload: payload3,
        };

        assert_eq!(modify1, modify2);
        assert_ne!(modify1, modify3);
    }
}
