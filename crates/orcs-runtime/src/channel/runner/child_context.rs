//! ChildContext implementation for Runner.
//!
//! Provides the runtime context that is injected into Children
//! to enable them to interact with the system safely.

use super::base::OutputSender;
use super::child_spawner::ChildSpawner;
use super::{ChannelRunner, Event};
use crate::auth::{CommandCheckResult, PermissionChecker, Session};
use crate::channel::{World, WorldCommand};
use orcs_auth::{CommandGrant, GrantPolicy};
use orcs_component::{
    async_trait, AsyncChildContext, AsyncChildHandle, ChildConfig, ChildContext, ChildHandle,
    ChildResult, Component, ComponentLoader, RunError, SpawnError,
};
use orcs_event::{EventCategory, Signal};
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
        match (&self.session, &self.checker) {
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
        }
    }

    /// Grants a command pattern for future execution.
    ///
    /// This is typically called after HIL approval to allow
    /// the same command pattern without re-approval.
    ///
    /// Does nothing if no grants store is configured.
    pub fn grant_command(&self, pattern: &str) {
        if let Some(grants) = &self.grants {
            grants.grant(CommandGrant::persistent(pattern));
        }
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

    /// Returns true if runner spawning is enabled.
    #[must_use]
    pub fn can_spawn_runner(&self) -> bool {
        self.world_tx.is_some() && self.world.is_some() && self.signal_tx.is_some()
    }

    /// Spawns a Component as a separate ChannelRunner.
    ///
    /// This creates a new Channel in the World and spawns a ChannelRunner
    /// to execute the Component in parallel.
    ///
    /// # Arguments
    ///
    /// * `component` - The Component to run
    ///
    /// # Returns
    ///
    /// The ChannelId of the spawned runner.
    ///
    /// # Errors
    ///
    /// - [`SpawnError::Internal`] if runner spawning is not enabled
    /// - [`SpawnError::Internal`] if permission check fails (requires elevated session)
    pub fn spawn_runner(&self, component: Box<dyn Component>) -> Result<ChannelId, SpawnError> {
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

        // Create a new channel ID
        let channel_id = ChannelId::new();
        let component_id = component.id().clone();

        // Clone what we need for the spawned task
        let world_tx_clone = world_tx.clone();
        let world_clone = Arc::clone(world);
        let signal_rx = signal_tx.subscribe();
        let signal_tx_clone = signal_tx.clone();
        let output_tx = self.output_tx.clone();

        // Clone auth context to propagate to child runner
        let session_clone = self.session.clone();
        let checker_clone = self.checker.clone();
        let grants_clone = self.grants.clone();

        // Spawn the runner in a new task
        tokio::spawn(async move {
            // Build the ChannelRunner with auth context propagation
            let mut builder = ChannelRunner::builder(
                channel_id,
                world_tx_clone,
                world_clone,
                signal_rx,
                component,
            )
            .with_emitter(signal_tx_clone)
            .with_output_channel(output_tx);

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

            let (runner, _handle) = builder.build();

            tracing::info!(
                "Spawned child runner: channel={}, component={} (auth propagated)",
                channel_id,
                component_id.fqn()
            );

            runner.run().await;
        });

        Ok(channel_id)
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
        let child = loader.load(&config)?;

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

    fn spawn_runner_from_script(
        &self,
        script: &str,
        id: Option<&str>,
    ) -> Result<ChannelId, SpawnError> {
        // Get component loader
        let loader = self
            .component_loader
            .as_ref()
            .ok_or_else(|| SpawnError::Internal("no component loader configured".into()))?;

        // Create component from script
        let component = loader.load_from_script(script, id)?;

        // Spawn as runner
        self.spawn_runner(component)
    }

    fn can_execute_command(&self, cmd: &str) -> bool {
        match (&self.session, &self.checker) {
            (Some(session), Some(checker)) => checker.can_execute_command(session, cmd),
            _ => true, // Permissive mode when not configured
        }
    }

    fn check_command_permission(&self, cmd: &str) -> orcs_component::CommandPermission {
        use orcs_component::CommandPermission;
        match (&self.session, &self.checker) {
            (Some(session), Some(checker)) => {
                let empty;
                let grants: &dyn GrantPolicy = match &self.grants {
                    Some(g) => g.as_ref(),
                    None => {
                        empty = crate::auth::DefaultGrantStore::new();
                        &empty
                    }
                };
                let result = checker.check_command(session, grants, cmd);
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
            _ => CommandPermission::Allowed, // Permissive mode when not configured
        }
    }

    fn grant_command(&self, pattern: &str) {
        if let Some(grants) = &self.grants {
            grants.grant(CommandGrant::persistent(pattern));
        }
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
        let child = loader.load(&config)?;

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

        let event = output_rx.try_recv().unwrap();
        assert_eq!(event.category, EventCategory::Output);
        assert_eq!(event.payload["message"], "Hello, World!");
        assert_eq!(event.payload["level"], "info");
    }

    #[test]
    fn emit_output_with_level() {
        let (ctx, mut output_rx) = setup();

        ChildContext::emit_output_with_level(&ctx, "Warning message", "warn");

        let event = output_rx.try_recv().unwrap();
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
        let mut handle = AsyncChildContext::spawn_child(&ctx, config).await.unwrap();

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

        let event = output_rx.try_recv().unwrap();
        assert_eq!(event.payload["message"], "Async Hello!");
    }

    #[tokio::test]
    async fn async_clone_box() {
        let (ctx, _) = setup();
        let cloned: Box<dyn AsyncChildContext> = AsyncChildContext::clone_box(&ctx);

        assert_eq!(AsyncChildContext::parent_id(cloned.as_ref()), "test-parent");
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
}
