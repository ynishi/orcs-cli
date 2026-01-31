//! EventBus - Unified Communication Between Components.
//!
//! The [`EventBus`] is the central message routing system in ORCS architecture.
//! All Component communication flows through EventBus.
//!
//! # Message Types
//!
//! ```text
//! ┌─────────────┐  Request   ┌─────────────┐
//! │   Source    │ ─────────► │   Target    │
//! │  Component  │            │  Component  │
//! │             │ ◄───────── │             │
//! └─────────────┘  Response  └─────────────┘
//!
//! ┌─────────────┐   Signal   ┌─────────────┐
//! │   Human     │ ─────────► │  All        │
//! │  (Source)   │            │  Components │
//! └─────────────┘            └─────────────┘
//! ```
//!
//! ## Request/Response
//!
//! Synchronous queries between Components:
//! - Source sends Request with target ComponentId
//! - Target processes and returns Response
//! - Timeout if no response within deadline
//!
//! ## Signal
//!
//! Control interrupts (highest priority):
//! - Broadcast to ALL registered Components
//! - Components MUST handle or face forced abort
//! - Veto signal stops everything immediately
//!
//! # Error Handling
//!
//! Operations return [`EngineError`] which implements [`orcs_types::ErrorCode`].
//!
//! | Error | Code | Recoverable |
//! |-------|------|-------------|
//! | Component not found | `ENGINE_COMPONENT_NOT_FOUND` | No |
//! | No target | `ENGINE_NO_TARGET` | No |
//! | Send failed | `ENGINE_SEND_FAILED` | Yes |
//! | Timeout | `ENGINE_TIMEOUT` | Yes |

use super::error::EngineError;
use orcs_event::{Request, Signal};
use orcs_types::{ComponentId, RequestId};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};

/// EventBus - routes messages between components.
///
/// The EventBus is responsible for:
/// - Registering/unregistering Components
/// - Routing Request messages to target Components
/// - Broadcasting Signal messages to all Components
/// - Managing pending response channels
///
/// # Thread Safety
///
/// EventBus itself is not `Send`/`Sync`. Use it within a single async task
/// or wrap with appropriate synchronization.
pub struct EventBus {
    /// Request senders per component
    request_senders: HashMap<ComponentId, mpsc::Sender<Request>>,
    /// Pending response receivers
    pending_responses: HashMap<RequestId, oneshot::Sender<Result<Value, EngineError>>>,
    /// Signal broadcaster
    signal_tx: broadcast::Sender<Signal>,
}

impl EventBus {
    /// Create new EventBus
    #[must_use]
    pub fn new() -> Self {
        let (signal_tx, _) = broadcast::channel(64);
        Self {
            request_senders: HashMap::new(),
            pending_responses: HashMap::new(),
            signal_tx,
        }
    }

    /// Register component and get handle
    pub fn register(&mut self, id: ComponentId) -> ComponentHandle {
        let (req_tx, req_rx) = mpsc::channel(32);
        let signal_rx = self.signal_tx.subscribe();

        self.request_senders.insert(id.clone(), req_tx);

        ComponentHandle {
            component_id: id,
            request_rx: req_rx,
            signal_rx,
        }
    }

    /// Unregister component
    pub fn unregister(&mut self, id: &ComponentId) {
        self.request_senders.remove(id);
    }

    /// Send request to target component.
    ///
    /// Routes the request to the specified target Component and waits for response.
    /// Target is required - use [`Request::with_target`] to set it.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] if:
    /// - No target specified ([`EngineError::NoTarget`])
    /// - Target component not found ([`EngineError::ComponentNotFound`])
    /// - Send failed ([`EngineError::SendFailed`])
    /// - Channel closed ([`EngineError::ChannelClosed`])
    /// - Request timed out ([`EngineError::Timeout`])
    pub async fn request(&mut self, req: Request) -> Result<Value, EngineError> {
        let request_id = req.id;
        let timeout_ms = req.timeout_ms;

        let Some(target) = &req.target else {
            return Err(EngineError::NoTarget);
        };

        let Some(sender) = self.request_senders.get(target) else {
            return Err(EngineError::ComponentNotFound(target.clone()));
        };

        let (tx, rx) = oneshot::channel();
        self.pending_responses.insert(request_id, tx);

        if sender.send(req).await.is_err() {
            self.pending_responses.remove(&request_id);
            return Err(EngineError::SendFailed("channel closed".into()));
        }

        let timeout_duration = Duration::from_millis(timeout_ms);
        match tokio::time::timeout(timeout_duration, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(EngineError::ChannelClosed),
            Err(_) => {
                self.pending_responses.remove(&request_id);
                Err(EngineError::Timeout(request_id))
            }
        }
    }

    /// Respond to a pending request.
    ///
    /// Components call this to complete a request with either a successful
    /// value or an error message. The error is wrapped as [`EngineError::ComponentFailed`].
    ///
    /// If the request_id is not found (already timed out or responded),
    /// this is a no-op.
    pub fn respond(&mut self, request_id: RequestId, result: Result<Value, String>) {
        if let Some(tx) = self.pending_responses.remove(&request_id) {
            let mapped = result.map_err(EngineError::ComponentFailed);
            let _ = tx.send(mapped);
        }
    }

    /// Broadcast signal to all components
    pub fn signal(&self, signal: Signal) {
        let _ = self.signal_tx.send(signal);
    }

    /// Get number of registered components
    #[must_use]
    pub fn component_count(&self) -> usize {
        self.request_senders.len()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle for component to receive messages
pub struct ComponentHandle {
    component_id: ComponentId,
    request_rx: mpsc::Receiver<Request>,
    signal_rx: broadcast::Receiver<Signal>,
}

impl ComponentHandle {
    /// Get component ID
    #[must_use]
    pub fn component_id(&self) -> &ComponentId {
        &self.component_id
    }

    /// Try to receive a request (non-blocking)
    pub fn try_recv_request(&mut self) -> Option<Request> {
        self.request_rx.try_recv().ok()
    }

    /// Try to receive a signal (non-blocking)
    pub fn try_recv_signal(&mut self) -> Option<Signal> {
        self.signal_rx.try_recv().ok()
    }

    /// Receive a request (async, waits until available).
    ///
    /// Returns `None` if the channel is closed.
    pub async fn recv_request(&mut self) -> Option<Request> {
        self.request_rx.recv().await
    }

    /// Receive a signal (async, waits until available).
    ///
    /// # Errors
    ///
    /// Returns error if the channel is closed or lagged.
    pub async fn recv_signal(&mut self) -> Result<Signal, broadcast::error::RecvError> {
        self.signal_rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Principal;
    use orcs_types::ChannelId;

    #[test]
    fn eventbus_creation() {
        let bus = EventBus::new();
        assert_eq!(bus.component_count(), 0);
    }

    #[test]
    fn register_component() {
        let mut bus = EventBus::new();
        let id = ComponentId::builtin("test");
        let _handle = bus.register(id);

        assert_eq!(bus.component_count(), 1);
    }

    #[test]
    fn unregister_component() {
        let mut bus = EventBus::new();
        let id = ComponentId::builtin("test");
        let _handle = bus.register(id.clone());

        bus.unregister(&id);
        assert_eq!(bus.component_count(), 0);
    }

    #[tokio::test]
    async fn signal_broadcast() {
        let mut bus = EventBus::new();
        let id = ComponentId::builtin("test");
        let mut handle = bus.register(id);

        let principal = Principal::System;
        bus.signal(Signal::veto(principal));
        tokio::task::yield_now().await;

        let signal = handle.try_recv_signal();
        assert!(signal.is_some());
        assert!(signal.unwrap().is_veto());
    }

    #[tokio::test]
    async fn request_to_nonexistent_target() {
        use orcs_types::ErrorCode;

        let mut bus = EventBus::new();
        let source = ComponentId::builtin("source");
        let target = ComponentId::builtin("nonexistent");
        let channel = ChannelId::new();

        let req = Request::new("test", source, channel, Value::Null).with_target(target);

        let result = bus.request(req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "ENGINE_COMPONENT_NOT_FOUND");
    }

    #[tokio::test]
    async fn request_without_target() {
        use orcs_types::ErrorCode;

        let mut bus = EventBus::new();
        let source = ComponentId::builtin("source");
        let channel = ChannelId::new();

        let req = Request::new("test", source, channel, Value::Null);

        let result = bus.request(req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code(), "ENGINE_NO_TARGET");
    }

    #[tokio::test]
    async fn request_respond_flow() {
        let mut bus = EventBus::new();
        let source = ComponentId::builtin("source");
        let target = ComponentId::builtin("target");
        let channel = ChannelId::new();

        let mut handle = bus.register(target.clone());

        let req = Request::new("echo", source, channel, Value::String("hello".into()))
            .with_target(target);
        let request_id = req.id;

        let (tx, rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            if let Some(req) = handle.recv_request().await {
                tx.send(req).ok();
            }
        });

        let _response_task = tokio::spawn(async move {
            let mut bus = bus;
            bus.request(req).await
        });

        let received_req = rx.await.unwrap();
        assert_eq!(received_req.id, request_id);
    }

    #[tokio::test]
    async fn respond_completes_request() {
        let mut bus = EventBus::new();
        let source = ComponentId::builtin("source");
        let target = ComponentId::builtin("target");
        let channel = ChannelId::new();

        let mut handle = bus.register(target.clone());

        let req = Request::new("echo", source, channel, Value::String("test".into()))
            .with_target(target.clone());
        let request_id = req.id;

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel::<Result<Value, String>>();
        tokio::spawn(async move {
            if let Some(req) = handle.recv_request().await {
                resp_tx.send(Ok(req.payload)).ok();
            }
        });

        let (tx, rx) = tokio::sync::oneshot::channel();
        bus.pending_responses.insert(request_id, tx);

        if let Some(sender) = bus.request_senders.get(&target) {
            sender.send(req).await.unwrap();
        }

        let result = resp_rx.await.unwrap();
        bus.respond(request_id, result);

        let received = rx.await.unwrap();
        assert!(received.is_ok());
    }

    #[test]
    fn respond_to_pending_request() {
        let mut bus = EventBus::new();
        let request_id = RequestId::new();

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        bus.pending_responses.insert(request_id, tx);

        bus.respond(request_id, Ok(Value::String("result".into())));

        let received = rx.try_recv();
        assert!(received.is_ok());
        assert!(received.unwrap().is_ok());
    }

    #[test]
    fn respond_to_unknown_request_is_noop() {
        let mut bus = EventBus::new();
        let request_id = RequestId::new();

        bus.respond(request_id, Ok(Value::Null));
    }

    #[tokio::test]
    async fn multiple_signal_receivers() {
        let mut bus = EventBus::new();
        let id1 = ComponentId::builtin("comp1");
        let id2 = ComponentId::builtin("comp2");

        let mut handle1 = bus.register(id1);
        let mut handle2 = bus.register(id2);

        let principal = Principal::System;
        bus.signal(Signal::veto(principal));
        tokio::task::yield_now().await;

        assert!(handle1.try_recv_signal().is_some());
        assert!(handle2.try_recv_signal().is_some());
    }

    #[test]
    fn component_handle_getters() {
        let mut bus = EventBus::new();
        let id = ComponentId::builtin("test");
        let handle = bus.register(id.clone());

        assert_eq!(handle.component_id(), &id);
    }

    #[tokio::test]
    async fn request_timeout() {
        use orcs_types::ErrorCode;

        let mut bus = EventBus::new();
        let source = ComponentId::builtin("source");
        let target = ComponentId::builtin("target");
        let channel = ChannelId::new();

        let _handle = bus.register(target.clone());

        let req = Request::new("slow_op", source, channel, Value::Null)
            .with_target(target)
            .with_timeout(10);

        let result = bus.request(req).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "ENGINE_TIMEOUT");
        assert!(err.is_recoverable());
    }
}
