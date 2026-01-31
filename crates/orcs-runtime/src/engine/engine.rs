//! OrcsEngine - Main Runtime.
//!
//! The [`OrcsEngine`] is the central runtime that:
//!
//! - Manages Component lifecycle (register, poll, shutdown)
//! - Dispatches Signals to all Components
//! - Coordinates EventBus message routing
//! - Manages the World (Channel hierarchy)
//!
//! # Runtime Loop
//!
//! ```text
//! loop {
//!     1. Check Signals (highest priority - Veto stops everything)
//!     2. Poll each Component:
//!        - Deliver pending Signals
//!        - Deliver pending Requests
//!        - Collect Responses
//!     3. Yield to async runtime
//! }
//! ```
//!
//! # Signal Handling
//!
//! Signals are interrupts from Human (or authorized Components):
//!
//! - **Veto**: Immediate engine stop (checked first)
//! - **Cancel**: Stop specific Channel/Component
//! - **Pause/Resume**: Suspend/continue execution
//! - **Approve/Reject**: HIL responses
//!
//! Components MUST respond to Signals. If a Component returns
//! [`SignalResponse::Ignored`], the engine forces abort.

use super::eventbus::{ComponentHandle, EventBus};
use crate::channel::World;
use orcs_component::Component;
use orcs_event::{Signal, SignalKind, SignalResponse};
use orcs_types::ComponentId;
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// OrcsEngine - Main runtime for ORCS CLI.
///
/// Manages the complete lifecycle of Components and coordinates
/// communication via EventBus.
///
/// # Example
///
/// ```ignore
/// use orcs_runtime::World;
///
/// // Create World with primary channel (caller's responsibility)
/// let mut world = World::new();
/// world.create_primary().expect("first call always succeeds");
///
/// // Inject into engine
/// let mut engine = OrcsEngine::new(world);
///
/// // Register components
/// engine.register(Box::new(LlmComponent::new()));
/// engine.register(Box::new(ToolsComponent::new()));
///
/// // Run the engine
/// engine.run().await;
/// ```
///
/// # Signal Priority
///
/// Signals are processed with highest priority. A Veto signal
/// immediately stops the engine without processing further.
pub struct OrcsEngine {
    /// EventBus for message routing
    eventbus: EventBus,
    /// World for Channel management
    world: World,
    /// Registered Components
    components: HashMap<ComponentId, Box<dyn Component>>,
    /// Component handles for message delivery
    handles: HashMap<ComponentId, ComponentHandle>,
    /// Running state
    running: bool,
}

impl OrcsEngine {
    /// Create new engine with injected World.
    ///
    /// The World should already have a primary channel created.
    /// This design separates World initialization from Engine creation,
    /// making the Engine easier to test and configure.
    #[must_use]
    pub fn new(world: World) -> Self {
        Self {
            eventbus: EventBus::new(),
            world,
            components: HashMap::new(),
            handles: HashMap::new(),
            running: false,
        }
    }

    /// Register a component
    pub fn register(&mut self, component: Box<dyn Component>) {
        let id = component.id().clone();
        let handle = self.eventbus.register(id.clone());

        info!("Registered component: {}", id);

        self.handles.insert(id.clone(), handle);
        self.components.insert(id, component);
    }

    /// Send signal (from external, e.g., human input)
    pub fn signal(&self, signal: Signal) {
        info!("Signal dispatched: {:?}", signal.kind);
        self.eventbus.signal(signal);
    }

    /// Check if engine is running
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Stop the engine
    pub fn stop(&mut self) {
        self.running = false;
    }

    /// Get world reference
    #[must_use]
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Get mutable world reference
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Main run loop
    pub async fn run(&mut self) {
        self.running = true;
        info!("OrcsEngine started");

        while self.running {
            self.poll_once();
            tokio::task::yield_now().await;
        }

        self.shutdown();
        info!("OrcsEngine stopped");
    }

    /// Poll all components once
    fn poll_once(&mut self) {
        let component_ids: Vec<ComponentId> = self.components.keys().cloned().collect();
        let mut to_remove: Vec<ComponentId> = Vec::new();

        for id in component_ids {
            // Skip if already marked for removal
            if to_remove.contains(&id) {
                continue;
            }

            // Collect signals first
            let signals: Vec<Signal> = {
                if let Some(handle) = self.handles.get_mut(&id) {
                    let mut sigs = Vec::new();
                    while let Some(signal) = handle.try_recv_signal() {
                        sigs.push(signal);
                    }
                    sigs
                } else {
                    continue;
                }
            };

            // Process collected signals
            for signal in signals {
                // Veto stops everything - check first
                if signal.kind == SignalKind::Veto {
                    self.running = false;
                    return;
                }

                let should_remove = if let Some(component) = self.components.get_mut(&id) {
                    let response = component.on_signal(&signal);

                    match response {
                        SignalResponse::Handled => {
                            info!("Component {} handled signal {:?}", id, signal.kind);
                            false
                        }
                        SignalResponse::Ignored => {
                            // Signal is a command - ignoring is not acceptable
                            // Force kill the component
                            warn!(
                                "Component {} ignored signal {:?} - forcing abort",
                                id, signal.kind
                            );
                            component.abort();
                            true
                        }
                        SignalResponse::Abort => {
                            info!("Component {} aborting due to signal {:?}", id, signal.kind);
                            component.abort();
                            true
                        }
                    }
                } else {
                    false
                };

                if should_remove {
                    to_remove.push(id.clone());
                    break; // Don't process more signals for this component
                }
            }

            // Skip request processing if component is being removed
            if to_remove.contains(&id) {
                continue;
            }

            // Collect requests
            let requests: Vec<orcs_event::Request> = {
                if let Some(handle) = self.handles.get_mut(&id) {
                    let mut reqs = Vec::new();
                    while let Some(request) = handle.try_recv_request() {
                        reqs.push(request);
                    }
                    reqs
                } else {
                    continue;
                }
            };

            // Process collected requests
            for request in requests {
                let request_id = request.id;
                if let Some(component) = self.components.get_mut(&id) {
                    debug!("Component {} handling request: {}", id, request.operation);
                    let result = component.on_request(&request);
                    let mapped = result.map_err(|e| e.to_string());
                    self.eventbus.respond(request_id, mapped);
                }
            }
        }

        // Remove aborted components
        for id in to_remove {
            self.unregister(&id);
        }
    }

    /// Unregister a component
    fn unregister(&mut self, id: &ComponentId) {
        self.eventbus.unregister(id);
        self.handles.remove(id);
        self.components.remove(id);
        info!("Unregistered component: {}", id);
    }

    /// Shutdown all components
    fn shutdown(&mut self) {
        info!("Shutting down all components");
        for (id, component) in &mut self.components {
            debug!("Aborting component: {}", id);
            component.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Principal;
    use orcs_component::ComponentError;
    use orcs_event::Request;
    use serde_json::Value;

    /// Create a World with primary channel for testing.
    fn test_world() -> World {
        let mut world = World::new();
        world.create_primary().expect("first call always succeeds");
        world
    }

    struct EchoComponent {
        id: ComponentId,
        aborted: bool,
    }

    impl EchoComponent {
        fn new() -> Self {
            Self {
                id: ComponentId::builtin("echo"),
                aborted: false,
            }
        }
    }

    impl Component for EchoComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> orcs_component::Status {
            orcs_component::Status::Idle
        }

        fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
            Ok(request.payload.clone())
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {
            self.aborted = true;
        }

        fn status_detail(&self) -> Option<orcs_component::StatusDetail> {
            None
        }

        fn init(&mut self) -> Result<(), ComponentError> {
            Ok(())
        }

        fn shutdown(&mut self) {
            // Default: no-op
        }
    }

    #[test]
    fn engine_creation() {
        let engine = OrcsEngine::new(test_world());
        assert!(!engine.is_running());
        assert!(engine.world().primary().is_some());
    }

    #[test]
    fn register_component() {
        let mut engine = OrcsEngine::new(test_world());
        let echo = Box::new(EchoComponent::new());
        engine.register(echo);

        assert_eq!(engine.components.len(), 1);
    }

    #[tokio::test]
    async fn veto_stops_engine() {
        let mut engine = OrcsEngine::new(test_world());
        let echo = Box::new(EchoComponent::new());
        engine.register(echo);

        let principal = Principal::System;
        engine.signal(Signal::veto(principal));
        engine.running = true;
        engine.poll_once();

        assert!(!engine.is_running());
    }

    #[test]
    fn multiple_components() {
        let mut engine = OrcsEngine::new(test_world());

        let echo1 = Box::new(EchoComponent::new());
        let mut echo2_inner = EchoComponent::new();
        echo2_inner.id = ComponentId::builtin("echo2");
        let echo2 = Box::new(echo2_inner);

        engine.register(echo1);
        engine.register(echo2);

        assert_eq!(engine.components.len(), 2);
    }

    #[test]
    fn stop_engine() {
        let mut engine = OrcsEngine::new(test_world());
        engine.running = true;

        assert!(engine.is_running());
        engine.stop();
        assert!(!engine.is_running());
    }

    #[test]
    fn world_access() {
        let mut engine = OrcsEngine::new(test_world());

        assert!(engine.world().primary().is_some());

        let primary = engine.world().primary().unwrap();
        engine.world_mut().complete(primary);

        assert!(!engine.world().get(&primary).unwrap().is_running());
    }

    #[tokio::test]
    async fn cancel_signal_handled() {
        let mut engine = OrcsEngine::new(test_world());
        let echo = Box::new(EchoComponent::new());
        engine.register(echo);

        let channel_id = engine.world().primary().unwrap();
        let principal = Principal::System;
        let cancel = Signal::cancel(channel_id, principal);

        engine.signal(cancel);
        engine.running = true;
        engine.poll_once();

        // Cancel should not stop engine (only Veto does)
        assert!(engine.is_running());
    }

    #[tokio::test]
    async fn shutdown_aborts_all_components() {
        let mut engine = OrcsEngine::new(test_world());
        let echo = Box::new(EchoComponent::new());
        let id = echo.id.clone();
        engine.register(echo);

        engine.shutdown();

        // Component should have been aborted
        // (We can't directly check aborted flag, but shutdown was called)
        assert!(engine.components.contains_key(&id));
    }

    struct CountingComponent {
        id: ComponentId,
        request_count: std::sync::atomic::AtomicUsize,
        signal_count: std::sync::atomic::AtomicUsize,
    }

    impl CountingComponent {
        fn new(name: &str) -> Self {
            Self {
                id: ComponentId::builtin(name),
                request_count: std::sync::atomic::AtomicUsize::new(0),
                signal_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl Component for CountingComponent {
        fn id(&self) -> &ComponentId {
            &self.id
        }

        fn status(&self) -> orcs_component::Status {
            orcs_component::Status::Idle
        }

        fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
            self.request_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(request.payload.clone())
        }

        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            self.signal_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if signal.is_veto() {
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {}
    }

    #[tokio::test]
    async fn poll_processes_signals() {
        let mut engine = OrcsEngine::new(test_world());
        let comp = Box::new(CountingComponent::new("counter"));
        engine.register(comp);

        let channel_id = engine.world().primary().unwrap();
        let principal = Principal::System;

        // Send multiple non-veto signals
        engine.signal(Signal::cancel(channel_id, principal.clone()));
        engine.signal(Signal::cancel(channel_id, principal));

        engine.running = true;
        tokio::task::yield_now().await;
        engine.poll_once();

        // Engine should still be running (no veto)
        assert!(engine.is_running());
    }
}
