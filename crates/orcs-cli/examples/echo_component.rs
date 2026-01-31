//! M1 E2E Test: Echo Component
//!
//! Demonstrates:
//! - Component registration
//! - Request/Response via EventBus
//! - Signal (Veto) handling

use orcs_app::{
    Component, ComponentError, ComponentId, OrcsEngine, Principal, PrincipalId, Request, Signal,
    SignalResponse, Status, World,
};
use serde_json::Value;

struct EchoComponent {
    id: ComponentId,
}

impl EchoComponent {
    fn new() -> Self {
        Self {
            id: ComponentId::builtin("echo"),
        }
    }
}

impl Component for EchoComponent {
    fn id(&self) -> &ComponentId {
        &self.id
    }

    fn on_request(&mut self, request: &Request) -> Result<Value, ComponentError> {
        println!("EchoComponent received: {}", request.operation);
        match request.operation.as_str() {
            "echo" => {
                if let Some(msg) = request.payload.as_str() {
                    println!("EchoComponent: {}", msg);
                }
                Ok(request.payload.clone())
            }
            _ => Err(ComponentError::NotSupported(request.operation.clone())),
        }
    }

    fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
        println!("EchoComponent received signal: {:?}", signal.kind);
        if signal.is_veto() {
            println!("EchoComponent: Veto received, aborting...");
            SignalResponse::Abort
        } else {
            SignalResponse::Handled
        }
    }

    fn abort(&mut self) {
        println!("EchoComponent: Aborted");
    }

    fn status(&self) -> Status {
        Status::Idle
    }
}

#[tokio::main]
async fn main() {
    println!("=== M1 E2E Test: Echo Component ===\n");

    // Create World with primary channel
    let mut world = World::new();
    world.create_primary().expect("first call always succeeds");

    // Inject World into Engine
    let mut engine = OrcsEngine::new(world);
    println!(
        "Engine created with primary channel: {:?}",
        engine.world().primary()
    );

    let echo = Box::new(EchoComponent::new());
    engine.register(echo);
    println!("EchoComponent registered\n");

    println!("Sending Veto signal...");
    engine.signal(Signal::veto(Principal::User(PrincipalId::new())));

    println!("Running poll cycle...\n");
    engine.run().await;

    println!("\n=== Test Complete ===");
    println!("Engine stopped: {}", !engine.is_running());
}
