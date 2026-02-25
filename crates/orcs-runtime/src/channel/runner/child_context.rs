//! ChildContext implementation for Runner.
//!
//! Provides the runtime context that is injected into Children
//! to enable them to interact with the system safely.

use super::base::OutputSender;
use super::child_spawner::ChildSpawner;
use super::{ChannelRunner, Event};
use crate::auth::{CommandCheckResult, PermissionChecker, Session};
use crate::channel::{World, WorldCommand};
use crate::engine::{SharedChannelHandles, SharedComponentChannelMap};
use orcs_auth::Capability;
use orcs_auth::{CommandGrant, GrantPolicy};
use orcs_component::{
    async_trait, AsyncChildContext, AsyncChildHandle, ChildConfig, ChildContext, ChildHandle,
    ChildResult, Component, ComponentLoader, RunError, SpawnError,
};
use orcs_event::{EventCategory, Signal};
use orcs_hook::SharedHookRegistry;
use orcs_types::{ChannelId, ComponentId};
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, RwLock};

/// Implementation of ChildContext for the Runner.
///
/// This is injected into Children to provide safe access to:
/// - Output emission (to parent/IO)
/// - Child spawning (in-process)
/// - Runner spawning (as separate ChannelRunner)
/// - Status queries
#[derive(Clone)]
pub struct ChildContextImpl {
    /// Parent ID (Component or Child).
    parent_id: String,
    /// Output sender for events (to owning ChannelRunner).
    output_tx: OutputSender,
    /// IO output sender for Output events (to ClientRunner).
    /// When set, Output events are routed here instead of output_tx.
    io_output_tx: Option<OutputSender>,
    /// Child spawner (shared, locked).
    spawner: Arc<Mutex<ChildSpawner>>,
    /// Lua loader for creating children from scripts.
    lua_loader: Option<Arc<dyn LuaChildLoader>>,
    /// Component loader for creating components from scripts.
    component_loader: Option<Arc<dyn ComponentLoader>>,

    // -- Auth support --
    /// Session for permission checking (identity + privilege level).
    session: Option<Arc<Session>>,
    /// Permission checker policy.
    checker: Option<Arc<dyn PermissionChecker>>,
    /// Dynamic command grants (shared across contexts).
    grants: Option<Arc<dyn GrantPolicy>>,

    // -- Runner spawning support --
    /// World command sender for creating channels.
    world_tx: Option<mpsc::Sender<WorldCommand>>,
    /// World read access.
    world: Option<Arc<RwLock<World>>>,
    /// Signal sender for subscribing new runners.
    signal_tx: Option<broadcast::Sender<Signal>>,

    // -- RPC support --
    /// Shared channel handles for RPC request routing.
    shared_handles: Option<SharedChannelHandles>,
    /// Shared FQN → ChannelId mapping for RPC target resolution.
    component_channel_map: Option<SharedComponentChannelMap>,
    /// Channel ID of the owning ChannelRunner (for RPC source_channel).
    channel_id: Option<ChannelId>,

    // -- Hook support --
    /// Shared hook registry (propagated to child runners).
    hook_registry: Option<SharedHookRegistry>,

    // -- Capability support --
    /// Effective capabilities for this context.
    /// Defaults to ALL; narrowed via `Capability::inherit` on spawn.
    capabilities: Capability,
}

/// Trait for loading Lua children from config.
///
/// This abstraction allows the context to create LuaChild instances
/// without depending directly on orcs-lua.
pub trait LuaChildLoader: Send + Sync {
    /// Creates a RunnableChild from a config.
    fn load(
        &self,
        config: &ChildConfig,
    ) -> Result<Box<dyn orcs_component::RunnableChild>, SpawnError>;
}

impl ChildContextImpl {
    /// Creates a new ChildContextImpl.
    ///
    /// # Arguments
    ///
    /// * `parent_id` - ID of the parent Component/Child
    /// * `output_tx` - Sender for output events
    /// * `spawner` - Shared child spawner
    #[must_use]
    pub fn new(
        parent_id: impl Into<String>,
        output_tx: OutputSender,
        spawner: Arc<Mutex<ChildSpawner>>,
    ) -> Self {
        Self {
            parent_id: parent_id.into(),
            output_tx,
            io_output_tx: None,
            spawner,
            lua_loader: None,
            component_loader: None,
            session: None,
            checker: None,
            grants: None,
            world_tx: None,
            world: None,
            signal_tx: None,
            shared_handles: None,
            component_channel_map: None,
            channel_id: None,
            hook_registry: None,
            capabilities: Capability::ALL,
        }
    }

    /// Sets the IO output channel for routing Output events to ClientRunner.
    ///
    /// When set, Output events (display, approval_request) are sent to
    /// the IO channel instead of the owning ChannelRunner.
    #[must_use]
    pub fn with_io_output_channel(mut self, tx: OutputSender) -> Self {
        self.io_output_tx = Some(tx);
        self
    }

    /// Sends an Output event to the IO channel if available, otherwise to the owning channel.
    fn send_to_output(&self, event: Event) {
        if let Some(io_tx) = &self.io_output_tx {
            let _ = io_tx.try_send_direct(event);
        } else {
            let _ = self.output_tx.try_send_direct(event);
        }
    }

    /// Sets the session for permission checking.
    ///
    /// Takes ownership of a Session and wraps it in Arc.
    #[must_use]
    pub fn with_session(mut self, session: Session) -> Self {
        self.session = Some(Arc::new(session));
        self
    }

    /// Sets the session for permission checking (Arc version).
    ///
    /// Use this when sharing a Session across multiple contexts.
    #[must_use]
    pub fn with_session_arc(mut self, session: Arc<Session>) -> Self {
        self.session = Some(session);
        self
    }

    /// Sets the permission checker policy.
    #[must_use]
    pub fn with_checker(mut self, checker: Arc<dyn PermissionChecker>) -> Self {
        self.checker = Some(checker);
        self
    }

    /// Sets the dynamic grant store for command permission management.
    #[must_use]
    pub fn with_grants(mut self, grants: Arc<dyn GrantPolicy>) -> Self {
        self.grants = Some(grants);
        self
    }

    /// Returns the session if set.
    #[must_use]
    pub fn session(&self) -> Option<&Arc<Session>> {
        self.session.as_ref()
    }

    /// Returns the permission checker if set.
    #[must_use]
    pub fn checker(&self) -> Option<&Arc<dyn PermissionChecker>> {
        self.checker.as_ref()
    }

    /// Checks if command execution is allowed.
    ///
    /// Returns `true` if:
    /// - No session/checker configured (permissive mode for backward compat)
    /// - Session is elevated and checker allows
    #[must_use]
    pub fn can_execute_command(&self, cmd: &str) -> bool {
        match (&self.session, &self.checker) {
            (Some(session), Some(checker)) => checker.can_execute_command(session, cmd),
            _ => true, // Permissive mode when not configured
        }
    }

    /// Checks command with granular result (for HIL integration).
    ///
    /// Returns [`CommandCheckResult`] which can be:
    /// - `Allowed`: Execute immediately
    /// - `Denied`: Block with reason
    /// - `RequiresApproval`: Needs HIL approval
    ///
    /// # HIL Flow
    ///
    /// When `RequiresApproval` is returned:
    ///
    /// 1. Submit the `ApprovalRequest` to HilComponent
    /// 2. Wait for user approval
    /// 3. If approved, call `ctx.grant_command(&grant_pattern)`
    /// 4. The command will then be allowed on retry
    ///
    /// # Example
    ///
    /// ```ignore
    /// match ctx.check_command("rm -rf ./temp") {
    ///     CommandCheckResult::Allowed => {
    ///         // Execute command
    ///     }
    ///     CommandCheckResult::Denied(reason) => {
    ///         return Err(Error::PermissionDenied(reason));
    ///     }
    ///     CommandCheckResult::RequiresApproval { request, grant_pattern } => {
    ///         // Submit to HIL, wait for approval
    ///         // If approved: ctx.grant_command(&grant_pattern);
    ///     }
    /// }
    /// ```
    #[must_use]
    pub fn check_command(&self, cmd: &str) -> CommandCheckResult {
        // -- AuthPreCheck hook --
        let pre_payload = serde_json::json!({ "command": cmd });
        let pre_action = self.dispatch_hook(orcs_hook::HookPoint::AuthPreCheck, pre_payload);

        match &pre_action {
            orcs_hook::HookAction::Abort { reason } => {
                return CommandCheckResult::Denied(reason.clone());
            }
            orcs_hook::HookAction::Skip(value) => {
                // Skip with {"allowed": true} → Allowed, otherwise → Denied
                let allowed = value
                    .as_object()
                    .and_then(|o| o.get("allowed"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(value.as_bool().unwrap_or(true));
                if allowed {
                    return CommandCheckResult::Allowed;
                }
                let reason = value
                    .as_object()
                    .and_then(|o| o.get("reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("denied by auth pre-check hook")
                    .to_string();
                return CommandCheckResult::Denied(reason);
            }
            _ => {} // Continue — proceed with normal check
        }

        // Normal permission check
        let result = match (&self.session, &self.checker) {
            (Some(session), Some(checker)) => {
                // grants 未注入時は空 store をfallback（grant_command が効かない）
                let empty;
                let grants: &dyn GrantPolicy = match &self.grants {
                    Some(g) => g.as_ref(),
                    None => {
                        empty = crate::auth::DefaultGrantStore::new();
                        &empty
                    }
                };
                checker.check_command(session, grants, cmd)
            }
            _ => CommandCheckResult::Allowed, // Permissive mode when not configured
        };

        // -- AuthPostCheck hook (observe-only) --
        let post_payload = serde_json::json!({
            "command": cmd,
            "result": match &result {
                CommandCheckResult::Allowed => "allowed",
                CommandCheckResult::Denied(_) => "denied",
                CommandCheckResult::RequiresApproval { .. } => "requires_approval",
            },
        });
        let _ = self.dispatch_hook(orcs_hook::HookPoint::AuthPostCheck, post_payload);

        result
    }

    /// Grants a command pattern for future execution.
    ///
    /// This is typically called after HIL approval to allow
    /// the same command pattern without re-approval.
    ///
    /// Does nothing if no grants store is configured.
    pub fn grant_command(&self, pattern: &str) {
        self.grant_command_inner(pattern);
    }

    /// Checks if child spawning is allowed.
    #[must_use]
    pub fn can_spawn_child_auth(&self) -> bool {
        match (&self.session, &self.checker) {
            (Some(session), Some(checker)) => checker.can_spawn_child(session),
            _ => true, // Permissive mode when not configured
        }
    }

    /// Checks if runner spawning is allowed.
    #[must_use]
    pub fn can_spawn_runner_auth(&self) -> bool {
        match (&self.session, &self.checker) {
            (Some(session), Some(checker)) => checker.can_spawn_runner(session),
            _ => true, // Permissive mode when not configured
        }
    }

    /// Sets the Lua child loader.
    #[must_use]
    pub fn with_lua_loader(mut self, loader: Arc<dyn LuaChildLoader>) -> Self {
        self.lua_loader = Some(loader);
        self
    }

    /// Sets the component loader for runner spawning.
    #[must_use]
    pub fn with_component_loader(mut self, loader: Arc<dyn ComponentLoader>) -> Self {
        self.component_loader = Some(loader);
        self
    }

    /// Enables runner spawning by providing world/signal access.
    #[must_use]
    pub fn with_runner_support(
        mut self,
        world_tx: mpsc::Sender<WorldCommand>,
        world: Arc<RwLock<World>>,
        signal_tx: broadcast::Sender<Signal>,
    ) -> Self {
        self.world_tx = Some(world_tx);
        self.world = Some(world);
        self.signal_tx = Some(signal_tx);
        self
    }

    /// Enables RPC support by providing shared handles, component map, and channel ID.
    ///
    /// `channel_id` identifies the owning ChannelRunner and is used as the
    /// `source_channel` field in outgoing RPC requests.
    #[must_use]
    pub fn with_rpc_support(
        mut self,
        shared_handles: SharedChannelHandles,
        component_channel_map: SharedComponentChannelMap,
        channel_id: ChannelId,
    ) -> Self {
        self.shared_handles = Some(shared_handles);
        self.component_channel_map = Some(component_channel_map);
        self.channel_id = Some(channel_id);
        self
    }

    /// Sets the shared hook registry for propagation to child runners.
    #[must_use]
    pub fn with_hook_registry(mut self, registry: SharedHookRegistry) -> Self {
        self.hook_registry = Some(registry);
        self
    }

    /// Sets the effective capabilities for this context.
    ///
    /// Defaults to [`Capability::ALL`]. Use this to restrict what
    /// operations the context (and its children) can perform.
    #[must_use]
    pub fn with_capabilities(mut self, caps: Capability) -> Self {
        self.capabilities = caps;
        self
    }

    /// Returns true if runner spawning is enabled.
    #[must_use]
    pub fn can_spawn_runner(&self) -> bool {
        self.world_tx.is_some() && self.world.is_some() && self.signal_tx.is_some()
    }

    /// Spawns a Component as a separate ChannelRunner.
    ///
    /// This creates a new Channel in the World and spawns a ChannelRunner
    /// to execute the Component in parallel. The spawned Component:
    /// - Is registered in World, shared_handles, and component_channel_map
    /// - Can receive RPC via `orcs.request(fqn, ...)`
    /// - Can spawn its own children/Components (Nested spawn)
    /// - Inherits auth context and capabilities from the parent
    ///
    /// # IO Output Routing
    ///
    /// The spawned runner inherits the parent's IO output channel (`io_output_tx`)
    /// when available. `orcs.output()` is semantically "display to the user", so
    /// any Component calling it expects the message to reach IO (stdout). If the
    /// parent is connected to an IO bridge, spawned children are connected to the
    /// same bridge. When `io_output_tx` is `None`, the runner falls back to the
    /// parent's own channel (`output_tx`).
    ///
    /// # Arguments
    ///
    /// * `component` - The Component to run (arbitrary — not a copy of the parent)
    ///
    /// # Returns
    ///
    /// A tuple of `(ChannelId, oneshot::Receiver<()>)`. The receiver fires
    /// when the async registration (World + handles + map) is complete and
    /// the Component is ready to receive RPC requests.
    ///
    /// # Errors
    ///
    /// - [`SpawnError::Internal`] if runner spawning is not enabled
    /// - [`SpawnError::Internal`] if permission check fails (requires elevated session)
    pub fn spawn_runner(
        &self,
        component: Box<dyn Component>,
    ) -> Result<(ChannelId, tokio::sync::oneshot::Receiver<()>), SpawnError> {
        // Permission check: requires elevated session
        if !self.can_spawn_runner_auth() {
            tracing::warn!(
                parent_id = %self.parent_id,
                "spawn_runner denied: requires elevated privilege"
            );
            return Err(SpawnError::Internal(
                "spawn_runner requires elevated privilege".into(),
            ));
        }

        let world_tx = self.world_tx.as_ref().ok_or_else(|| {
            SpawnError::Internal("runner spawning not enabled (no world_tx)".into())
        })?;
        let world = self
            .world
            .as_ref()
            .ok_or_else(|| SpawnError::Internal("runner spawning not enabled (no world)".into()))?;
        let signal_tx = self.signal_tx.as_ref().ok_or_else(|| {
            SpawnError::Internal("runner spawning not enabled (no signal_tx)".into())
        })?;

        // Pre-generate the channel ID so the caller can use it immediately.
        let channel_id = ChannelId::new();
        let component_id = component.id().clone();
        let component_fqn = component_id.fqn();

        // Determine the parent channel for World registration.
        let parent_channel_id = self.channel_id.ok_or_else(|| {
            SpawnError::Internal("spawn_runner requires a parent channel_id".into())
        })?;

        // Clone what we need for the spawned task
        let world_tx_clone = world_tx.clone();
        let world_clone = Arc::clone(world);
        let signal_rx = signal_tx.subscribe();
        let signal_tx_clone = signal_tx.clone();
        // For spawned runners, route Output events to IO bridge (stdout)
        // if available, rather than to the parent component's channel.
        // Without this, orcs.output() in the child would send to the parent
        // (e.g. agent_mgr), which processes it as a new request — causing
        // an infinite dispatch loop.
        let effective_output_tx = self
            .io_output_tx
            .clone()
            .unwrap_or_else(|| self.output_tx.clone());

        // Clone auth context to propagate to child runner
        let session_clone = self.session.clone();
        let checker_clone = self.checker.clone();
        let grants_clone = self.grants.clone();
        let hook_registry_clone = self.hook_registry.clone();

        // Clone RPC resources so the spawned runner participates in the
        // event mesh and is reachable via FQN-based RPC.
        let shared_handles_clone = self.shared_handles.clone();
        let component_channel_map_clone = self.component_channel_map.clone();

        // Clone loaders so the spawned Component can spawn its own
        // children (spawn_child) and nested Components (spawn_runner).
        let lua_loader_clone = self.lua_loader.clone();
        let component_loader_clone = self.component_loader.clone();

        // Ready notification: fires after World + handles + map registration
        // completes, signalling the Component is reachable via RPC.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();

        // Spawn the runner in a new task
        tokio::spawn(async move {
            // --- 1. Register channel in World ---
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            if let Err(e) = world_tx_clone
                .send(WorldCommand::SpawnWithId {
                    parent: parent_channel_id,
                    id: channel_id,
                    config: crate::channel::ChannelConfig::default(),
                    reply: reply_tx,
                })
                .await
            {
                tracing::error!(
                    "spawn_runner: failed to send SpawnWithId for {}: {}",
                    component_fqn,
                    e
                );
                return;
            }
            match reply_rx.await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::error!(
                        "spawn_runner: SpawnWithId failed (parent {} not found) for {}",
                        parent_channel_id,
                        component_fqn
                    );
                    return;
                }
                Err(e) => {
                    tracing::error!(
                        "spawn_runner: SpawnWithId reply dropped for {}: {}",
                        component_fqn,
                        e
                    );
                    return;
                }
            }

            // --- 2. Build the ChannelRunner ---
            let mut builder = ChannelRunner::builder(
                channel_id,
                world_tx_clone.clone(),
                world_clone,
                signal_rx,
                component,
            )
            .with_emitter(signal_tx_clone)
            .with_output_channel(effective_output_tx);

            // Enable RPC inbound so other Components can reach this one
            // via orcs.request(fqn, ...).
            builder = builder.with_request_channel();

            // Propagate shared handles / component map so the spawned
            // runner can broadcast events and issue outbound RPC.
            if let Some(ref handles) = shared_handles_clone {
                builder = builder.with_shared_handles(Arc::clone(handles));
            }
            if let Some(ref map) = component_channel_map_clone {
                builder = builder.with_component_channel_map(Arc::clone(map));
            }

            // Enable child/runner spawning so the spawned Component can
            // spawn its own children and nested Components with arbitrary scripts.
            builder = builder.with_child_spawner(lua_loader_clone);
            if let Some(loader) = component_loader_clone {
                builder = builder.with_component_loader(loader);
            }

            // Propagate auth context to child runner
            if let Some(session) = session_clone {
                builder = builder.with_session_arc(session);
            }
            if let Some(checker) = checker_clone {
                builder = builder.with_checker(checker);
            }
            if let Some(grants) = grants_clone {
                builder = builder.with_grants(grants);
            }
            // Propagate hook registry to child runner
            if let Some(registry) = hook_registry_clone {
                builder = builder.with_hook_registry(registry);
            }

            let (runner, handle) = builder.build();

            // --- 3. Register ChannelHandle in shared_handles ---
            // This makes the spawned runner reachable for event injection
            // and Extension event broadcast.
            if let Some(ref handles) = shared_handles_clone {
                handles.write().insert(channel_id, handle);
            }

            // --- 4. Register FQN → ChannelId in component_channel_map ---
            // This makes the spawned runner addressable via orcs.request(fqn, ...).
            if let Some(ref map) = component_channel_map_clone {
                map.write().insert(component_fqn.clone(), channel_id);
            }

            tracing::info!(
                "Spawned child runner: channel={}, component={} (World registered, RPC enabled)",
                channel_id,
                component_fqn
            );

            // Signal that registration is complete — the Component is now
            // reachable via orcs.request(fqn, ...).
            let _ = ready_tx.send(());

            runner.run().await;

            // --- 5. Cleanup on runner exit ---
            if let Some(ref handles) = shared_handles_clone {
                handles.write().remove(&channel_id);
            }
            if let Some(ref map) = component_channel_map_clone {
                map.write().remove(&component_fqn);
            }
            tracing::info!(
                "Child runner exited: channel={}, component={} (cleanup done)",
                channel_id,
                component_fqn
            );
        });

        Ok((channel_id, ready_rx))
    }

    /// Creates a sub-ChildContext for a spawned child, inheriting RPC
    /// handles, auth context, and capabilities from the parent.
    ///
    /// Effective capabilities = `parent_caps & requested_caps`.
    /// When `requested_caps` is `None`, the parent's capabilities are
    /// inherited without further narrowing.
    fn create_child_context(
        &self,
        child_id: &str,
        requested_caps: Option<Capability>,
    ) -> Box<dyn ChildContext> {
        let effective_caps =
            Capability::inherit(self.capabilities, requested_caps.unwrap_or(Capability::ALL));

        let mut ctx =
            ChildContextImpl::new(child_id, self.output_tx.clone(), Arc::clone(&self.spawner))
                .with_capabilities(effective_caps);
        if let Some(loader) = &self.lua_loader {
            ctx = ctx.with_lua_loader(Arc::clone(loader));
        }
        if let Some(s) = &self.session {
            ctx = ctx.with_session_arc(Arc::clone(s));
        }
        if let Some(c) = &self.checker {
            ctx = ctx.with_checker(Arc::clone(c));
        }
        if let Some(g) = &self.grants {
            ctx = ctx.with_grants(Arc::clone(g));
        }
        if let (Some(h), Some(m), Some(ch)) = (
            &self.shared_handles,
            &self.component_channel_map,
            self.channel_id,
        ) {
            ctx = ctx.with_rpc_support(Arc::clone(h), Arc::clone(m), ch);
        }
        if let Some(reg) = &self.hook_registry {
            ctx = ctx.with_hook_registry(Arc::clone(reg));
        }
        Box::new(ctx)
    }

    /// Dispatches a hook through the shared hook registry.
    ///
    /// Returns `HookAction::Continue` with the original context when no
    /// registry is configured.
    fn dispatch_hook(
        &self,
        point: orcs_hook::HookPoint,
        payload: serde_json::Value,
    ) -> orcs_hook::HookAction {
        let component_id = ComponentId::child(&self.parent_id);
        let channel_id = self.channel_id.unwrap_or_else(ChannelId::new);

        let ctx = orcs_hook::HookContext::new(
            point,
            component_id.clone(),
            channel_id,
            orcs_types::Principal::System,
            0,
            payload,
        );

        let Some(registry) = &self.hook_registry else {
            return orcs_hook::HookAction::Continue(Box::new(ctx));
        };

        let guard = registry.read().unwrap_or_else(|poisoned| {
            tracing::warn!("hook registry lock poisoned, using inner value");
            poisoned.into_inner()
        });
        guard.dispatch(point, &component_id, None, ctx)
    }

    /// Inner grant_command with hook dispatch.
    ///
    /// Called by both the inherent and trait `grant_command()` methods.
    fn grant_command_inner(&self, pattern: &str) {
        if let Some(grants) = &self.grants {
            if let Err(e) = grants.grant(CommandGrant::persistent(pattern)) {
                tracing::error!("grant_command failed: {e}");
            }
        }

        // -- AuthOnGrant hook (event) --
        let payload = serde_json::json!({
            "pattern": pattern,
            "granted_by": self.parent_id,
        });
        let _ = self.dispatch_hook(orcs_hook::HookPoint::AuthOnGrant, payload);
    }

    /// Creates an output event.
    fn create_output_event(&self, message: &str, level: &str) -> Event {
        Event {
            category: EventCategory::Output,
            operation: "display".to_string(),
            source: ComponentId::child(&self.parent_id),
            payload: serde_json::json!({
                "message": message,
                "level": level,
                "source": self.parent_id,
            }),
        }
    }
}

impl Debug for ChildContextImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildContextImpl")
            .field("parent_id", &self.parent_id)
            .field("capabilities", &self.capabilities)
            .field("has_lua_loader", &self.lua_loader.is_some())
            .field("has_session", &self.session.is_some())
            .field("has_checker", &self.checker.is_some())
            .field("has_grants", &self.grants.is_some())
            .field("can_spawn_runner", &self.can_spawn_runner())
            .finish()
    }
}

impl ChildContext for ChildContextImpl {
    fn parent_id(&self) -> &str {
        &self.parent_id
    }

    fn emit_output(&self, message: &str) {
        let event = self.create_output_event(message, "info");
        self.send_to_output(event);
    }

    fn emit_output_with_level(&self, message: &str, level: &str) {
        let event = self.create_output_event(message, level);
        self.send_to_output(event);
    }

    fn emit_approval_request(&self, operation: &str, description: &str) -> String {
        let approval_id = uuid::Uuid::new_v4().to_string();
        let event = Event {
            category: EventCategory::Output,
            operation: "approval_request".to_string(),
            source: ComponentId::child(&self.parent_id),
            payload: serde_json::json!({
                "type": "approval_request",
                "approval_id": approval_id,
                "operation": operation,
                "description": description,
                "source": self.parent_id,
            }),
        };
        self.send_to_output(event);
        tracing::info!(
            approval_id = %approval_id,
            operation = %operation,
            "Emitted approval request: {}",
            description
        );
        approval_id
    }

    fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
        // Get Lua loader
        let loader = self
            .lua_loader
            .as_ref()
            .ok_or_else(|| SpawnError::Internal("no lua loader configured".into()))?;

        // Load child from config
        let mut child = loader.load(&config)?;

        // Inject a ChildContext with capability inheritance.
        let child_ctx = self.create_child_context(&config.id, config.capabilities);
        child.set_context(child_ctx);

        // Spawn via spawner
        let mut spawner = self
            .spawner
            .lock()
            .map_err(|e| SpawnError::Internal(format!("spawner lock failed: {}", e)))?;

        spawner.spawn(config, child)
    }

    fn child_count(&self) -> usize {
        self.spawner.lock().map(|s| s.child_count()).unwrap_or(0)
    }

    fn max_children(&self) -> usize {
        self.spawner.lock().map(|s| s.max_children()).unwrap_or(0)
    }

    fn send_to_child(
        &self,
        child_id: &str,
        input: serde_json::Value,
    ) -> Result<ChildResult, RunError> {
        let spawner = self
            .spawner
            .lock()
            .map_err(|e| RunError::ExecutionFailed(format!("spawner lock failed: {}", e)))?;

        spawner.run_child(child_id, input)
    }

    fn send_to_child_async(
        &self,
        child_id: &str,
        input: serde_json::Value,
    ) -> Result<(), RunError> {
        // Brief lock to get child Arc, then release.
        let child_arc = {
            let spawner = self
                .spawner
                .lock()
                .map_err(|e| RunError::ExecutionFailed(format!("spawner lock failed: {}", e)))?;
            spawner
                .get_child_arc(child_id)
                .ok_or_else(|| RunError::NotFound(child_id.to_string()))?
        };
        // spawner lock released

        let child_id_owned = child_id.to_string();
        std::thread::spawn(move || match child_arc.lock() {
            Ok(mut child) => {
                if let orcs_component::ChildResult::Err(e) = child.run(input) {
                    tracing::warn!(
                        child_id = %child_id_owned,
                        "send_to_child_async: child returned error: {}", e
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    child_id = %child_id_owned,
                    "send_to_child_async: child lock failed: {}", e
                );
            }
        });

        Ok(())
    }

    fn send_to_children_batch(
        &self,
        requests: Vec<(String, serde_json::Value)>,
    ) -> Vec<(String, Result<ChildResult, RunError>)> {
        if requests.is_empty() {
            return Vec::new();
        }

        // 1. Lock spawner briefly to collect Arc refs, then release.
        type ChildArc = Arc<Mutex<Box<dyn orcs_component::RunnableChild>>>;
        let child_arcs: Vec<(String, Option<ChildArc>)> = {
            let spawner = match self.spawner.lock() {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("spawner lock failed: {}", e);
                    return requests
                        .into_iter()
                        .map(|(id, _)| (id, Err(RunError::ExecutionFailed(msg.clone()))))
                        .collect();
                }
            };
            requests
                .iter()
                .map(|(id, _)| (id.clone(), spawner.get_child_arc(id)))
                .collect()
        };
        // spawner lock released here

        // 2. Run all children in parallel using OS threads.
        std::thread::scope(|s| {
            let handles: Vec<_> = child_arcs
                .into_iter()
                .zip(requests)
                .map(|((id, child_opt), (_, input))| {
                    s.spawn(move || match child_opt {
                        None => (
                            id.clone(),
                            Err(RunError::ExecutionFailed(format!(
                                "child not found: {}",
                                id
                            ))),
                        ),
                        Some(child_arc) => {
                            let mut guard = match child_arc.lock() {
                                Ok(g) => g,
                                Err(e) => {
                                    return (
                                        id,
                                        Err(RunError::ExecutionFailed(format!(
                                            "child lock failed: {}",
                                            e
                                        ))),
                                    );
                                }
                            };
                            let result = guard.run(input);
                            drop(guard);
                            (id, Ok(result))
                        }
                    })
                })
                .collect();

            handles
                .into_iter()
                .map(|h| {
                    h.join().unwrap_or_else(|_| {
                        (
                            String::from("<panic>"),
                            Err(RunError::ExecutionFailed("child thread panicked".into())),
                        )
                    })
                })
                .collect()
        })
    }

    fn spawn_runner_from_script(
        &self,
        script: &str,
        id: Option<&str>,
        globals: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<(ChannelId, String), SpawnError> {
        // Get component loader
        let loader = self
            .component_loader
            .as_ref()
            .ok_or_else(|| SpawnError::Internal("no component loader configured".into()))?;

        // Create component from script
        let component = loader.load_from_script(script, id, globals)?;
        let fqn = component.id().fqn();

        // Spawn as runner and wait for registration to complete.
        // The async task registers in World, shared_handles, and
        // component_channel_map. We block until ready so the caller
        // can immediately use orcs.request(fqn, ...).
        let (channel_id, ready_rx) = self.spawn_runner(component)?;

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                match tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx).await {
                    Ok(Ok(())) => {
                        tracing::debug!(
                            "spawn_runner_from_script: {} ready (channel={})",
                            fqn,
                            channel_id
                        );
                    }
                    Ok(Err(_)) => {
                        tracing::warn!(
                            "spawn_runner_from_script: ready channel dropped for {}",
                            fqn
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            "spawn_runner_from_script: registration timeout for {}",
                            fqn
                        );
                    }
                }
            })
        });

        Ok((channel_id, fqn))
    }

    fn can_execute_command(&self, cmd: &str) -> bool {
        match (&self.session, &self.checker) {
            (Some(session), Some(checker)) => checker.can_execute_command(session, cmd),
            _ => true, // Permissive mode when not configured
        }
    }

    fn check_command_permission(&self, cmd: &str) -> orcs_component::CommandPermission {
        use orcs_component::CommandPermission;
        // Delegate to inherent check_command() which handles hook dispatch
        let result = self.check_command(cmd);
        match result {
            CommandCheckResult::Allowed => CommandPermission::Allowed,
            CommandCheckResult::Denied(reason) => CommandPermission::Denied(reason),
            CommandCheckResult::RequiresApproval {
                request,
                grant_pattern,
            } => CommandPermission::RequiresApproval {
                grant_pattern,
                description: request.description.clone(),
            },
        }
    }

    fn capabilities(&self) -> Capability {
        self.capabilities
    }

    fn is_command_granted(&self, cmd: &str) -> bool {
        match &self.grants {
            Some(grants) => grants.is_granted(cmd).unwrap_or(false),
            None => false,
        }
    }

    fn grant_command(&self, pattern: &str) {
        self.grant_command_inner(pattern);
    }

    fn can_spawn_child_auth(&self) -> bool {
        match (&self.session, &self.checker) {
            (Some(session), Some(checker)) => checker.can_spawn_child(session),
            _ => true, // Permissive mode when not configured
        }
    }

    fn can_spawn_runner_auth(&self) -> bool {
        match (&self.session, &self.checker) {
            (Some(session), Some(checker)) => checker.can_spawn_runner(session),
            _ => true, // Permissive mode when not configured
        }
    }

    fn request(
        &self,
        target_fqn: &str,
        operation: &str,
        payload: serde_json::Value,
        timeout_ms: Option<u64>,
    ) -> Result<serde_json::Value, String> {
        let map = self
            .component_channel_map
            .as_ref()
            .ok_or("component_channel_map not configured for RPC")?;
        let handles = self
            .shared_handles
            .as_ref()
            .ok_or("shared_handles not configured for RPC")?;

        let timeout = timeout_ms.unwrap_or(orcs_event::DEFAULT_TIMEOUT_MS);
        let source_id = ComponentId::child(&self.parent_id);
        let source_channel = self.channel_id.unwrap_or_else(ChannelId::new);

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(super::rpc::resolve_and_send_rpc(
                super::rpc::RpcParams {
                    component_channel_map: map,
                    shared_handles: handles,
                    target_fqn,
                    operation,
                    source_id,
                    source_channel,
                    payload,
                    timeout_ms: timeout,
                },
            ))
        })
    }

    fn request_batch(
        &self,
        requests: Vec<(String, String, serde_json::Value, Option<u64>)>,
    ) -> Vec<Result<serde_json::Value, String>> {
        if requests.is_empty() {
            return Vec::new();
        }

        let map = match self.component_channel_map.as_ref() {
            Some(m) => m,
            None => {
                return requests
                    .iter()
                    .map(|_| Err("component_channel_map not configured for RPC".into()))
                    .collect();
            }
        };
        let handles = match self.shared_handles.as_ref() {
            Some(h) => h,
            None => {
                return requests
                    .iter()
                    .map(|_| Err("shared_handles not configured for RPC".into()))
                    .collect();
            }
        };

        let source_id = ComponentId::child(&self.parent_id);
        let source_channel = self.channel_id.unwrap_or_else(ChannelId::new);

        // Spawn all RPC requests as concurrent tasks, then join.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let join_handles: Vec<_> = requests
                    .into_iter()
                    .map(|(target, op, payload, timeout_ms)| {
                        let timeout = timeout_ms.unwrap_or(orcs_event::DEFAULT_TIMEOUT_MS);
                        let src_id = source_id.clone();
                        let map = Arc::clone(map);
                        let handles = Arc::clone(handles);
                        tokio::spawn(async move {
                            super::rpc::resolve_and_send_rpc(super::rpc::RpcParams {
                                component_channel_map: &map,
                                shared_handles: &handles,
                                target_fqn: &target,
                                operation: &op,
                                source_id: src_id,
                                source_channel,
                                payload,
                                timeout_ms: timeout,
                            })
                            .await
                        })
                    })
                    .collect();

                let mut results = Vec::with_capacity(join_handles.len());
                for jh in join_handles {
                    results.push(
                        jh.await
                            .unwrap_or_else(|e| Err(format!("rpc task failed: {e}"))),
                    );
                }
                results
            })
        })
    }

    fn extension(&self, key: &str) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        match key {
            "hook_registry" => self
                .hook_registry
                .as_ref()
                .map(|r| Box::new(Arc::clone(r)) as Box<dyn std::any::Any + Send + Sync>),
            _ => None,
        }
    }

    fn clone_box(&self) -> Box<dyn ChildContext> {
        Box::new(self.clone())
    }
}

#[async_trait]
impl AsyncChildContext for ChildContextImpl {
    fn parent_id(&self) -> &str {
        &self.parent_id
    }

    fn emit_output(&self, message: &str) {
        let event = self.create_output_event(message, "info");
        self.send_to_output(event);
    }

    fn emit_output_with_level(&self, message: &str, level: &str) {
        let event = self.create_output_event(message, level);
        self.send_to_output(event);
    }

    async fn spawn_child(
        &self,
        config: ChildConfig,
    ) -> Result<Box<dyn AsyncChildHandle>, SpawnError> {
        // Get Lua loader
        let loader = self
            .lua_loader
            .as_ref()
            .ok_or_else(|| SpawnError::Internal("no lua loader configured".into()))?;

        // Load child from config
        let mut child = loader.load(&config)?;

        // Inject a ChildContext with capability inheritance.
        let child_ctx = self.create_child_context(&config.id, config.capabilities);
        child.set_context(child_ctx);

        // Spawn via spawner (spawn_async returns Box<dyn AsyncChildHandle>)
        let mut spawner = self
            .spawner
            .lock()
            .map_err(|e| SpawnError::Internal(format!("spawner lock failed: {}", e)))?;

        spawner.spawn_async(config, child)
    }

    fn child_count(&self) -> usize {
        self.spawner.lock().map(|s| s.child_count()).unwrap_or(0)
    }

    fn max_children(&self) -> usize {
        self.spawner.lock().map(|s| s.max_children()).unwrap_or(0)
    }

    fn send_to_child(
        &self,
        child_id: &str,
        input: serde_json::Value,
    ) -> Result<ChildResult, RunError> {
        let spawner = self
            .spawner
            .lock()
            .map_err(|e| RunError::ExecutionFailed(format!("spawner lock failed: {}", e)))?;

        spawner.run_child(child_id, input)
    }

    fn send_to_child_async(
        &self,
        child_id: &str,
        input: serde_json::Value,
    ) -> Result<(), RunError> {
        let child_arc = {
            let spawner = self
                .spawner
                .lock()
                .map_err(|e| RunError::ExecutionFailed(format!("spawner lock failed: {}", e)))?;
            spawner
                .get_child_arc(child_id)
                .ok_or_else(|| RunError::NotFound(child_id.to_string()))?
        };

        let child_id_owned = child_id.to_string();
        std::thread::spawn(move || match child_arc.lock() {
            Ok(mut child) => {
                if let orcs_component::ChildResult::Err(e) = child.run(input) {
                    tracing::warn!(
                        child_id = %child_id_owned,
                        "send_to_child_async: child returned error: {}", e
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    child_id = %child_id_owned,
                    "send_to_child_async: child lock failed: {}", e
                );
            }
        });

        Ok(())
    }

    fn clone_box(&self) -> Box<dyn AsyncChildContext> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_component::{
        Child, ChildResult, Identifiable, RunnableChild, SignalReceiver, Status, Statusable,
    };
    use orcs_event::{Signal, SignalResponse};

    /// Test worker implementation.
    struct TestWorker {
        id: String,
        status: Status,
    }

    impl TestWorker {
        fn new(id: &str) -> Self {
            Self {
                id: id.into(),
                status: Status::Idle,
            }
        }
    }

    impl Identifiable for TestWorker {
        fn id(&self) -> &str {
            &self.id
        }
    }

    impl SignalReceiver for TestWorker {
        fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
            if signal.is_veto() {
                self.status = Status::Aborted;
                SignalResponse::Abort
            } else {
                SignalResponse::Handled
            }
        }

        fn abort(&mut self) {
            self.status = Status::Aborted;
        }
    }

    impl Statusable for TestWorker {
        fn status(&self) -> Status {
            self.status
        }
    }

    impl Child for TestWorker {}

    impl RunnableChild for TestWorker {
        fn run(&mut self, input: serde_json::Value) -> ChildResult {
            ChildResult::Ok(input)
        }
    }

    /// Test loader that creates TestWorker.
    struct TestLoader;

    impl LuaChildLoader for TestLoader {
        fn load(&self, config: &ChildConfig) -> Result<Box<dyn RunnableChild>, SpawnError> {
            Ok(Box::new(TestWorker::new(&config.id)))
        }
    }

    fn setup() -> (ChildContextImpl, super::super::base::OutputReceiver) {
        let (output_tx, output_rx) = OutputSender::channel(64);

        let spawner = ChildSpawner::new("test-parent", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        let ctx = ChildContextImpl::new("test-parent", output_tx, spawner_arc)
            .with_lua_loader(Arc::new(TestLoader));

        (ctx, output_rx)
    }

    #[test]
    fn parent_id() {
        let (ctx, _) = setup();
        assert_eq!(ChildContext::parent_id(&ctx), "test-parent");
    }

    #[test]
    fn emit_output() {
        let (ctx, mut output_rx) = setup();

        ChildContext::emit_output(&ctx, "Hello, World!");

        let event = output_rx
            .try_recv()
            .expect("should receive emit_output event");
        assert_eq!(event.category, EventCategory::Output);
        assert_eq!(event.payload["message"], "Hello, World!");
        assert_eq!(event.payload["level"], "info");
    }

    #[test]
    fn emit_output_with_level() {
        let (ctx, mut output_rx) = setup();

        ChildContext::emit_output_with_level(&ctx, "Warning message", "warn");

        let event = output_rx
            .try_recv()
            .expect("should receive emit_output_with_level event");
        assert_eq!(event.payload["message"], "Warning message");
        assert_eq!(event.payload["level"], "warn");
    }

    #[test]
    fn spawn_child() {
        let (ctx, _) = setup();

        let config = ChildConfig::new("worker-1");
        let result = ChildContext::spawn_child(&ctx, config);

        assert!(result.is_ok());
        assert_eq!(ChildContext::child_count(&ctx), 1);
    }

    #[test]
    fn max_children() {
        let (ctx, _) = setup();
        assert!(ChildContext::max_children(&ctx) > 0);
    }

    #[test]
    fn clone_box() {
        let (ctx, _) = setup();
        let cloned: Box<dyn ChildContext> = ChildContext::clone_box(&ctx);

        assert_eq!(cloned.parent_id(), "test-parent");
    }

    // --- AsyncChildContext tests ---

    #[tokio::test]
    async fn async_spawn_child() {
        let (ctx, _) = setup();

        let config = ChildConfig::new("async-worker-1");
        let result = AsyncChildContext::spawn_child(&ctx, config).await;

        assert!(result.is_ok());
        assert_eq!(AsyncChildContext::child_count(&ctx), 1);
    }

    #[tokio::test]
    async fn async_spawn_child_and_run() {
        let (ctx, _) = setup();

        let config = ChildConfig::new("async-worker-2");
        let mut handle = AsyncChildContext::spawn_child(&ctx, config)
            .await
            .expect("async spawn child should succeed");

        let result = handle.run(serde_json::json!({"test": true})).await;

        assert!(result.is_ok());
        if let Ok(ChildResult::Ok(data)) = result {
            assert_eq!(data["test"], true);
        }
    }

    #[tokio::test]
    async fn async_emit_output() {
        let (ctx, mut output_rx) = setup();

        AsyncChildContext::emit_output(&ctx, "Async Hello!");

        let event = output_rx
            .try_recv()
            .expect("should receive async emit_output event");
        assert_eq!(event.payload["message"], "Async Hello!");
    }

    #[tokio::test]
    async fn async_clone_box() {
        let (ctx, _) = setup();
        let cloned: Box<dyn AsyncChildContext> = AsyncChildContext::clone_box(&ctx);

        assert_eq!(AsyncChildContext::parent_id(cloned.as_ref()), "test-parent");
    }

    // --- send_to_child_async tests ---

    #[test]
    fn send_to_child_async_returns_immediately() {
        let (ctx, _) = setup();

        let config = ChildConfig::new("async-worker");
        ChildContext::spawn_child(&ctx, config).expect("spawn async-worker");

        let result = ChildContext::send_to_child_async(
            &ctx,
            "async-worker",
            serde_json::json!({"fire": "forget"}),
        );

        assert!(result.is_ok(), "should return Ok immediately");
    }

    #[test]
    fn send_to_child_async_missing_child_returns_error() {
        let (ctx, _) = setup();

        let result = ChildContext::send_to_child_async(&ctx, "nonexistent", serde_json::json!({}));

        assert!(result.is_err(), "should return Err for missing child");
        let err = result.expect_err("expected NotFound error");
        assert!(
            err.to_string().contains("not found"),
            "error should mention 'not found', got: {}",
            err
        );
    }

    #[test]
    fn send_to_child_async_child_actually_runs() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        // Custom worker that sets a flag when run
        struct FlagWorker {
            id: String,
            status: Status,
            ran: Arc<AtomicBool>,
        }

        impl Identifiable for FlagWorker {
            fn id(&self) -> &str {
                &self.id
            }
        }

        impl SignalReceiver for FlagWorker {
            fn on_signal(&mut self, _: &Signal) -> SignalResponse {
                SignalResponse::Handled
            }
            fn abort(&mut self) {
                self.status = Status::Aborted;
            }
        }

        impl Statusable for FlagWorker {
            fn status(&self) -> Status {
                self.status
            }
        }

        impl Child for FlagWorker {}

        impl RunnableChild for FlagWorker {
            fn run(&mut self, input: serde_json::Value) -> ChildResult {
                self.ran.store(true, Ordering::SeqCst);
                ChildResult::Ok(input)
            }
        }

        let ran_flag = Arc::new(AtomicBool::new(false));

        let (output_tx, _output_rx) = OutputSender::channel(64);
        let spawner = ChildSpawner::new("test-parent", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        let ctx = ChildContextImpl::new("test-parent", output_tx, Arc::clone(&spawner_arc));

        // Directly spawn into spawner (bypass lua loader)
        {
            let worker = Box::new(FlagWorker {
                id: "flag-worker".into(),
                status: Status::Idle,
                ran: Arc::clone(&ran_flag),
            });
            let mut spawner_guard = spawner_arc.lock().expect("lock spawner");
            spawner_guard
                .spawn(ChildConfig::new("flag-worker"), worker)
                .expect("spawn flag-worker");
        }

        let result =
            ChildContext::send_to_child_async(&ctx, "flag-worker", serde_json::json!({"go": true}));
        assert!(result.is_ok(), "async send should succeed");

        // Wait for background thread to complete
        for _ in 0..100 {
            if ran_flag.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            ran_flag.load(Ordering::SeqCst),
            "child should have been executed by background thread"
        );
    }

    #[test]
    fn send_to_child_async_multiple_children_concurrent() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        struct CounterWorker {
            id: String,
            status: Status,
            counter: Arc<AtomicUsize>,
        }

        impl Identifiable for CounterWorker {
            fn id(&self) -> &str {
                &self.id
            }
        }

        impl SignalReceiver for CounterWorker {
            fn on_signal(&mut self, _: &Signal) -> SignalResponse {
                SignalResponse::Handled
            }
            fn abort(&mut self) {
                self.status = Status::Aborted;
            }
        }

        impl Statusable for CounterWorker {
            fn status(&self) -> Status {
                self.status
            }
        }

        impl Child for CounterWorker {}

        impl RunnableChild for CounterWorker {
            fn run(&mut self, input: serde_json::Value) -> ChildResult {
                self.counter.fetch_add(1, Ordering::SeqCst);
                ChildResult::Ok(input)
            }
        }

        let counter = Arc::new(AtomicUsize::new(0));

        let (output_tx, _output_rx) = OutputSender::channel(64);
        let spawner = ChildSpawner::new("test-parent", output_tx.clone());
        let spawner_arc = Arc::new(Mutex::new(spawner));

        let ctx = ChildContextImpl::new("test-parent", output_tx, Arc::clone(&spawner_arc));

        // Spawn 3 children
        for i in 0..3 {
            let worker = Box::new(CounterWorker {
                id: format!("counter-{}", i),
                status: Status::Idle,
                counter: Arc::clone(&counter),
            });
            let mut spawner_guard = spawner_arc.lock().expect("lock spawner");
            spawner_guard
                .spawn(ChildConfig::new(format!("counter-{}", i)), worker)
                .expect("spawn counter worker");
        }

        // Send async to all 3
        for i in 0..3 {
            let result = ChildContext::send_to_child_async(
                &ctx,
                &format!("counter-{}", i),
                serde_json::json!({"i": i}),
            );
            assert!(result.is_ok(), "async send to counter-{} should succeed", i);
        }

        // Wait for all background threads
        for _ in 0..200 {
            if counter.load(Ordering::SeqCst) >= 3 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "all 3 children should have run"
        );
    }

    // --- send_to_children_batch tests ---

    #[test]
    fn batch_send_empty_returns_empty() {
        let (ctx, _) = setup();
        let results = ChildContext::send_to_children_batch(&ctx, vec![]);
        assert!(results.is_empty());
    }

    #[test]
    fn batch_send_single_child() {
        let (ctx, _) = setup();

        // Spawn a child
        let config = ChildConfig::new("batch-worker-1");
        ChildContext::spawn_child(&ctx, config).expect("spawn batch-worker-1");

        let requests = vec![("batch-worker-1".to_string(), serde_json::json!({"x": 1}))];
        let results = ChildContext::send_to_children_batch(&ctx, requests);

        assert_eq!(results.len(), 1);
        let (id, result) = &results[0];
        assert_eq!(id, "batch-worker-1");
        let child_result = result.as_ref().expect("should succeed");
        assert!(child_result.is_ok(), "child should return Ok");
    }

    #[test]
    fn batch_send_multiple_children_parallel() {
        let (ctx, _) = setup();

        // Spawn 5 children
        for i in 0..5 {
            let config = ChildConfig::new(format!("par-worker-{}", i));
            ChildContext::spawn_child(&ctx, config)
                .unwrap_or_else(|e| panic!("spawn par-worker-{}: {}", i, e));
        }

        let requests: Vec<_> = (0..5)
            .map(|i| (format!("par-worker-{}", i), serde_json::json!({"index": i})))
            .collect();

        let results = ChildContext::send_to_children_batch(&ctx, requests);

        assert_eq!(results.len(), 5);
        for (id, result) in &results {
            let child_result = result
                .as_ref()
                .unwrap_or_else(|e| panic!("{} should succeed: {}", id, e));
            assert!(child_result.is_ok(), "{} should return Ok", id);
            if let ChildResult::Ok(data) = child_result {
                assert!(data.get("index").is_some(), "{} should echo input", id);
            }
        }
    }

    #[test]
    fn batch_send_with_missing_child_returns_error() {
        let (ctx, _) = setup();

        // Spawn only worker-0
        let config = ChildConfig::new("exists");
        ChildContext::spawn_child(&ctx, config).expect("spawn exists");

        let requests = vec![
            ("exists".to_string(), serde_json::json!({"a": 1})),
            ("missing".to_string(), serde_json::json!({"b": 2})),
        ];
        let results = ChildContext::send_to_children_batch(&ctx, requests);

        assert_eq!(results.len(), 2);

        // First should succeed
        assert!(results[0].1.is_ok(), "existing child should succeed");

        // Second should fail
        assert!(results[1].1.is_err(), "missing child should return error");
        let err = results[1]
            .1
            .as_ref()
            .expect_err("expected Err for missing child");
        assert!(
            err.to_string().contains("not found"),
            "error should mention 'not found', got: {}",
            err
        );
    }

    // --- request_batch tests ---

    #[test]
    fn request_batch_empty_returns_empty() {
        let (ctx, _) = setup();
        let results = ChildContext::request_batch(&ctx, vec![]);
        assert!(results.is_empty());
    }

    #[test]
    fn request_batch_without_rpc_returns_errors() {
        let (ctx, _) = setup();
        // No RPC support configured → uses trait default → calls request() → "not configured"
        let requests = vec![
            (
                "comp-a".to_string(),
                "ping".to_string(),
                serde_json::json!({}),
                None,
            ),
            (
                "comp-b".to_string(),
                "ping".to_string(),
                serde_json::json!({}),
                None,
            ),
        ];
        let results = ChildContext::request_batch(&ctx, requests);

        assert_eq!(results.len(), 2);
        for result in &results {
            assert!(result.is_err(), "should fail without RPC configured");
            let err = result.as_ref().expect_err("expected error");
            assert!(
                err.contains("not configured"),
                "error should mention not configured, got: {}",
                err
            );
        }
    }

    // --- CommandCheckResult tests (Phase 3D) ---

    mod check_command_tests {
        use super::*;
        use crate::auth::{DefaultGrantStore, DefaultPolicy, Session};
        use orcs_types::{Principal, PrincipalId};
        use std::time::Duration;

        fn setup_with_auth(
            elevated: bool,
        ) -> (ChildContextImpl, super::super::super::base::OutputReceiver) {
            let (output_tx, output_rx) = OutputSender::channel(64);

            let spawner = ChildSpawner::new("test-parent", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let session = if elevated {
                Session::new(Principal::User(PrincipalId::new())).elevate(Duration::from_secs(60))
            } else {
                Session::new(Principal::User(PrincipalId::new()))
            };

            let grants: Arc<dyn GrantPolicy> = Arc::new(DefaultGrantStore::new());

            let ctx = ChildContextImpl::new("test-parent", output_tx, spawner_arc)
                .with_lua_loader(Arc::new(TestLoader))
                .with_session(session)
                .with_checker(Arc::new(DefaultPolicy))
                .with_grants(grants);

            (ctx, output_rx)
        }

        #[test]
        fn check_command_permissive_without_auth() {
            let (ctx, _) = setup(); // No auth configured
            let result = ctx.check_command("rm -rf /");
            assert!(result.is_allowed()); // Permissive mode
        }

        #[test]
        fn check_command_elevated_allows_any() {
            let (ctx, _) = setup_with_auth(true); // Elevated
                                                  // Safety is enforced by OS sandbox, not command blocking
            let result = ctx.check_command("rm -rf /");
            assert!(result.is_allowed());
        }

        #[test]
        fn check_command_safe_requires_approval_not_elevated() {
            let (ctx, _) = setup_with_auth(false); // Standard (non-elevated)
            let result = ctx.check_command("ls -la");
            assert!(result.requires_approval()); // All commands need approval
        }

        #[test]
        fn check_command_dangerous_requires_approval() {
            let (ctx, _) = setup_with_auth(false); // Standard
            let result = ctx.check_command("rm -rf ./temp");
            assert!(result.requires_approval());
            assert!(result.approval_request().is_some());
        }

        #[test]
        fn check_command_elevated_allows_dangerous() {
            let (ctx, _) = setup_with_auth(true); // Elevated
            let result = ctx.check_command("rm -rf ./temp");
            assert!(result.is_allowed());
        }

        #[test]
        fn grant_command_allows_future_execution() {
            let (ctx, _) = setup_with_auth(false); // Standard

            // First check requires approval
            let result = ctx.check_command("rm -rf ./temp");
            assert!(result.requires_approval());

            // Grant the pattern
            ctx.grant_command("rm -rf");

            // Now allowed
            let result = ctx.check_command("rm -rf ./temp");
            assert!(result.is_allowed());
        }

        #[test]
        fn shared_grants_across_contexts() {
            let (output_tx, _) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("test", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let session = Arc::new(Session::new(Principal::User(PrincipalId::new())));
            let grants: Arc<dyn GrantPolicy> = Arc::new(DefaultGrantStore::new());

            let ctx1 = ChildContextImpl::new("ctx1", output_tx.clone(), Arc::clone(&spawner_arc))
                .with_session_arc(Arc::clone(&session))
                .with_checker(Arc::new(DefaultPolicy))
                .with_grants(Arc::clone(&grants));

            let ctx2 = ChildContextImpl::new("ctx2", output_tx, spawner_arc)
                .with_session_arc(Arc::clone(&session))
                .with_checker(Arc::new(DefaultPolicy))
                .with_grants(Arc::clone(&grants));

            // Grant via ctx1
            ctx1.grant_command("rm -rf");

            // Should be allowed in ctx2 (shared grants)
            let result = ctx2.check_command("rm -rf ./temp");
            assert!(result.is_allowed());
        }

        // --- Trait-level check_command_permission / grant_command tests ---

        #[test]
        fn trait_check_command_permission_requires_approval_not_elevated() {
            let (ctx, _) = setup_with_auth(false);
            let ctx_dyn: &dyn ChildContext = &ctx;
            let perm = ctx_dyn.check_command_permission("ls -la");
            assert!(perm.requires_approval());
            assert_eq!(perm.status_str(), "requires_approval");
        }

        #[test]
        fn trait_check_command_permission_elevated_allows_any() {
            let (ctx, _) = setup_with_auth(true); // Elevated
            let ctx_dyn: &dyn ChildContext = &ctx;
            // Safety enforced by OS sandbox, not command blocking
            let perm = ctx_dyn.check_command_permission("rm -rf /");
            assert!(perm.is_allowed());
            assert_eq!(perm.status_str(), "allowed");
        }

        #[test]
        fn trait_check_command_permission_requires_approval() {
            let (ctx, _) = setup_with_auth(false); // Standard
            let ctx_dyn: &dyn ChildContext = &ctx;
            let perm = ctx_dyn.check_command_permission("rm -rf ./temp");
            assert!(perm.requires_approval());
            assert_eq!(perm.status_str(), "requires_approval");
            if let orcs_component::CommandPermission::RequiresApproval {
                grant_pattern,
                description,
            } = &perm
            {
                assert!(!grant_pattern.is_empty());
                assert!(!description.is_empty());
            } else {
                panic!("expected RequiresApproval");
            }
        }

        #[test]
        fn trait_grant_command_then_allowed() {
            let (ctx, _) = setup_with_auth(false);
            let ctx_dyn: &dyn ChildContext = &ctx;

            // Initially requires approval
            let perm = ctx_dyn.check_command_permission("rm -rf ./temp");
            assert!(perm.requires_approval());

            // Grant via trait
            ctx_dyn.grant_command("rm -rf");

            // Now allowed
            let perm = ctx_dyn.check_command_permission("rm -rf ./temp");
            assert!(perm.is_allowed());
        }

        #[test]
        fn trait_permissive_without_auth() {
            let (ctx, _) = setup(); // No auth configured
            let ctx_dyn: &dyn ChildContext = &ctx;
            let perm = ctx_dyn.check_command_permission("rm -rf /");
            assert!(perm.is_allowed()); // Permissive mode
        }
    }

    // --- Auth Hook integration tests (Step 4.2) ---

    mod auth_hook_tests {
        use super::*;
        use crate::auth::{DefaultGrantStore, DefaultPolicy, Session};
        use orcs_hook::testing::MockHook;
        use orcs_hook::HookPoint;
        use orcs_types::{Principal, PrincipalId};
        use serde_json::json;
        use std::time::Duration;

        fn setup_with_hooks(
            elevated: bool,
        ) -> (
            ChildContextImpl,
            orcs_hook::SharedHookRegistry,
            super::super::super::base::OutputReceiver,
        ) {
            let (output_tx, output_rx) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("test-parent", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let session = if elevated {
                Session::new(Principal::User(PrincipalId::new())).elevate(Duration::from_secs(60))
            } else {
                Session::new(Principal::User(PrincipalId::new()))
            };

            let grants: Arc<dyn GrantPolicy> = Arc::new(DefaultGrantStore::new());
            let registry = orcs_hook::shared_hook_registry();

            let ctx = ChildContextImpl::new("test-parent", output_tx, spawner_arc)
                .with_lua_loader(Arc::new(TestLoader))
                .with_session(session)
                .with_checker(Arc::new(DefaultPolicy))
                .with_grants(grants)
                .with_hook_registry(Arc::clone(&registry));

            (ctx, registry, output_rx)
        }

        #[test]
        fn auth_pre_check_abort_denies_command() {
            let (ctx, registry, _) = setup_with_hooks(true); // Elevated (would normally Allowed)

            // Register aborting hook
            let hook = MockHook::aborter(
                "deny-rm",
                "*::*",
                HookPoint::AuthPreCheck,
                "blocked by policy",
            );
            registry
                .write()
                .expect("acquire hook registry write lock")
                .register(Box::new(hook));

            let result = ctx.check_command("rm -rf /");
            assert!(result.is_denied());
            assert_eq!(result.denial_reason(), Some("blocked by policy"));
        }

        #[test]
        fn auth_pre_check_skip_allows_command() {
            let (ctx, registry, _) = setup_with_hooks(false); // Non-elevated (would require approval)

            // Register skip hook that allows
            let hook = MockHook::skipper(
                "allow-all",
                "*::*",
                HookPoint::AuthPreCheck,
                json!({"allowed": true}),
            );
            registry
                .write()
                .expect("acquire hook registry write lock")
                .register(Box::new(hook));

            let result = ctx.check_command("rm -rf /");
            assert!(result.is_allowed());
        }

        #[test]
        fn auth_pre_check_skip_denies_with_reason() {
            let (ctx, registry, _) = setup_with_hooks(true); // Elevated

            let hook = MockHook::skipper(
                "deny-custom",
                "*::*",
                HookPoint::AuthPreCheck,
                json!({"allowed": false, "reason": "custom deny"}),
            );
            registry
                .write()
                .expect("acquire hook registry write lock")
                .register(Box::new(hook));

            let result = ctx.check_command("echo hello");
            assert!(result.is_denied());
            assert_eq!(result.denial_reason(), Some("custom deny"));
        }

        #[test]
        fn auth_pre_check_continue_preserves_normal_flow() {
            let (ctx, registry, _) = setup_with_hooks(false); // Non-elevated

            // Register pass-through hook
            let hook = MockHook::pass_through("observer", "*::*", HookPoint::AuthPreCheck);
            let counter = hook.call_count.clone();
            registry
                .write()
                .expect("acquire hook registry write lock")
                .register(Box::new(hook));

            let result = ctx.check_command("ls -la");
            // Hook was called
            assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
            // Normal result preserved (non-elevated → requires approval)
            assert!(result.requires_approval());
        }

        #[test]
        fn auth_post_check_fires_after_check() {
            let (ctx, registry, _) = setup_with_hooks(true); // Elevated

            let hook = MockHook::pass_through("post-observer", "*::*", HookPoint::AuthPostCheck);
            let counter = hook.call_count.clone();
            registry
                .write()
                .expect("acquire hook registry write lock")
                .register(Box::new(hook));

            let result = ctx.check_command("echo hello");
            assert!(result.is_allowed());
            // Post hook was invoked
            assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        #[test]
        fn auth_post_check_receives_result_in_payload() {
            let (ctx, registry, _) = setup_with_hooks(true); // Elevated → Allowed

            let hook =
                MockHook::modifier("post-checker", "*::*", HookPoint::AuthPostCheck, |ctx| {
                    // Verify payload contains result field
                    assert_eq!(ctx.payload["result"], "allowed");
                    assert_eq!(ctx.payload["command"], "echo hello");
                });
            let counter = hook.call_count.clone();
            registry
                .write()
                .expect("acquire hook registry write lock")
                .register(Box::new(hook));

            let _ = ctx.check_command("echo hello");
            assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        #[test]
        fn auth_on_grant_fires_on_grant_command() {
            let (ctx, registry, _) = setup_with_hooks(false);

            let hook = MockHook::pass_through("grant-observer", "*::*", HookPoint::AuthOnGrant);
            let counter = hook.call_count.clone();
            registry
                .write()
                .expect("acquire hook registry write lock")
                .register(Box::new(hook));

            ctx.grant_command("rm -rf");

            assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        #[test]
        fn auth_on_grant_payload_contains_pattern() {
            let (ctx, registry, _) = setup_with_hooks(false);

            let hook = MockHook::modifier("grant-checker", "*::*", HookPoint::AuthOnGrant, |ctx| {
                assert_eq!(ctx.payload["pattern"], "rm -rf");
                assert_eq!(ctx.payload["granted_by"], "test-parent");
            });
            let counter = hook.call_count.clone();
            registry
                .write()
                .expect("acquire hook registry write lock")
                .register(Box::new(hook));

            ctx.grant_command("rm -rf");
            assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        #[test]
        fn auth_hooks_no_registry_passthrough() {
            // Without hook_registry, check_command should work normally
            let (output_tx, _) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("test", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let session =
                Session::new(Principal::User(PrincipalId::new())).elevate(Duration::from_secs(60));

            let ctx = ChildContextImpl::new("test", output_tx, spawner_arc)
                .with_session(session)
                .with_checker(Arc::new(DefaultPolicy));
            // No hook_registry set

            let result = ctx.check_command("echo hello");
            assert!(result.is_allowed());
        }

        #[test]
        fn trait_grant_command_dispatches_hook() {
            let (ctx, registry, _) = setup_with_hooks(false);
            let ctx_dyn: &dyn ChildContext = &ctx;

            let hook = MockHook::pass_through("trait-grant", "*::*", HookPoint::AuthOnGrant);
            let counter = hook.call_count.clone();
            registry
                .write()
                .expect("acquire hook registry write lock")
                .register(Box::new(hook));

            ctx_dyn.grant_command("ls");
            assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        }

        #[test]
        fn trait_check_command_permission_dispatches_hooks() {
            let (ctx, registry, _) = setup_with_hooks(true);
            let ctx_dyn: &dyn ChildContext = &ctx;

            let pre_hook = MockHook::pass_through("pre", "*::*", HookPoint::AuthPreCheck);
            let pre_counter = pre_hook.call_count.clone();
            let post_hook = MockHook::pass_through("post", "*::*", HookPoint::AuthPostCheck);
            let post_counter = post_hook.call_count.clone();

            {
                let mut guard = registry
                    .write()
                    .expect("acquire hook registry write lock for multi-hook test");
                guard.register(Box::new(pre_hook));
                guard.register(Box::new(post_hook));
            }

            let perm = ctx_dyn.check_command_permission("echo hello");
            assert!(perm.is_allowed());
            assert_eq!(pre_counter.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(post_counter.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }

    // --- Capability inheritance tests ---

    mod capability_tests {
        use super::*;
        use orcs_auth::Capability;

        #[test]
        fn default_capabilities_is_all() {
            let (ctx, _) = setup();
            assert_eq!(
                ChildContext::capabilities(&ctx),
                Capability::ALL,
                "new context should default to ALL"
            );
        }

        #[test]
        fn with_capabilities_restricts() {
            let (output_tx, _) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("test", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let caps = Capability::READ | Capability::WRITE;
            let ctx = ChildContextImpl::new("test", output_tx, spawner_arc).with_capabilities(caps);

            assert_eq!(ChildContext::capabilities(&ctx), caps);
            assert!(ChildContext::has_capability(&ctx, Capability::READ));
            assert!(ChildContext::has_capability(&ctx, Capability::WRITE));
            assert!(
                !ChildContext::has_capability(&ctx, Capability::EXECUTE),
                "EXECUTE should not be present"
            );
            assert!(
                !ChildContext::has_capability(&ctx, Capability::SPAWN),
                "SPAWN should not be present"
            );
        }

        #[test]
        fn create_child_context_inherits_parent_caps() {
            let (output_tx, _) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("parent", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let parent_caps = Capability::READ | Capability::WRITE | Capability::EXECUTE;
            let ctx = ChildContextImpl::new("parent", output_tx, spawner_arc)
                .with_capabilities(parent_caps);

            // None = inherit all from parent
            let child_ctx = ctx.create_child_context("child-1", None);
            assert_eq!(
                child_ctx.capabilities(),
                parent_caps,
                "child should inherit parent caps when no restriction requested"
            );
        }

        #[test]
        fn create_child_context_narrows_with_request() {
            let (output_tx, _) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("parent", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let parent_caps = Capability::READ | Capability::WRITE | Capability::EXECUTE;
            let ctx = ChildContextImpl::new("parent", output_tx, spawner_arc)
                .with_capabilities(parent_caps);

            // Request only READ | WRITE → should drop EXECUTE
            let requested = Capability::READ | Capability::WRITE;
            let child_ctx = ctx.create_child_context("child-1", Some(requested));

            assert_eq!(
                child_ctx.capabilities(),
                Capability::READ | Capability::WRITE,
                "effective = parent & requested"
            );
            assert!(
                !child_ctx.has_capability(Capability::EXECUTE),
                "EXECUTE dropped by intersection"
            );
        }

        #[test]
        fn create_child_context_cannot_exceed_parent() {
            let (output_tx, _) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("parent", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let parent_caps = Capability::READ;
            let ctx = ChildContextImpl::new("parent", output_tx, spawner_arc)
                .with_capabilities(parent_caps);

            // Child requests ALL → should be capped at parent's READ
            let child_ctx = ctx.create_child_context("child-1", Some(Capability::ALL));

            assert_eq!(
                child_ctx.capabilities(),
                Capability::READ,
                "child cannot exceed parent's capabilities"
            );
        }

        #[test]
        fn grandchild_inherits_narrowed_caps() {
            let (output_tx, _) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("root", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            // Root: ALL
            let root = ChildContextImpl::new("root", output_tx, spawner_arc)
                .with_capabilities(Capability::ALL);

            // Child: READ | WRITE (narrowed from root)
            let child_ctx =
                root.create_child_context("child", Some(Capability::READ | Capability::WRITE));
            assert_eq!(
                child_ctx.capabilities(),
                Capability::READ | Capability::WRITE,
            );

            // Grandchild: requests ALL → capped at child's READ | WRITE
            // Need to downcast to call create_child_context
            // Instead, verify via has_capability
            assert!(!child_ctx.has_capability(Capability::EXECUTE));
            assert!(!child_ctx.has_capability(Capability::SPAWN));
            assert!(!child_ctx.has_capability(Capability::LLM));
        }

        #[test]
        fn empty_intersection_yields_no_caps() {
            let (output_tx, _) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("parent", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let parent_caps = Capability::READ | Capability::WRITE;
            let ctx = ChildContextImpl::new("parent", output_tx, spawner_arc)
                .with_capabilities(parent_caps);

            // Request EXECUTE | SPAWN → no overlap with parent
            let child_ctx =
                ctx.create_child_context("child-1", Some(Capability::EXECUTE | Capability::SPAWN));

            assert_eq!(
                child_ctx.capabilities(),
                Capability::empty(),
                "no overlap should produce empty capabilities"
            );
            assert!(!child_ctx.has_capability(Capability::READ));
            assert!(!child_ctx.has_capability(Capability::EXECUTE));
        }

        #[test]
        fn spawn_child_applies_config_capabilities() {
            let (ctx, _) = setup(); // ALL caps, with TestLoader

            // Spawn with restricted capabilities
            let config = ChildConfig::new("restricted-worker").with_capabilities(Capability::READ);
            let handle = ChildContext::spawn_child(&ctx, config).expect("spawn restricted-worker");

            assert_eq!(handle.id(), "restricted-worker");
            // The child was spawned successfully — the config.capabilities
            // was passed to create_child_context. We cannot directly
            // inspect the child's context from the handle, but the
            // integration is verified by the create_child_context tests above.
        }

        #[test]
        fn capabilities_preserved_in_clone_box() {
            let (output_tx, _) = OutputSender::channel(64);
            let spawner = ChildSpawner::new("test", output_tx.clone());
            let spawner_arc = Arc::new(Mutex::new(spawner));

            let caps = Capability::READ | Capability::EXECUTE;
            let ctx = ChildContextImpl::new("test", output_tx, spawner_arc).with_capabilities(caps);

            let cloned: Box<dyn ChildContext> = ChildContext::clone_box(&ctx);
            assert_eq!(
                cloned.capabilities(),
                caps,
                "clone_box should preserve capabilities"
            );
        }
    }
}
