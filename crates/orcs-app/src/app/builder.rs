//! Builder for [`OrcsApp`].

use super::OrcsApp;
use crate::AppError;
use mlua::Lua;
use orcs_component::{ChildConfig, Component, RunnableChild, SpawnError};
use orcs_lua::{LuaChild, ScriptLoader};
use orcs_runtime::auth::{DefaultGrantStore, DefaultPolicy, GrantPolicy, PermissionChecker};
use orcs_runtime::session::SessionStore;
use orcs_runtime::LuaChildLoader;
use orcs_runtime::{
    ChannelConfig, ConfigResolver, IOPort, LocalFileStore, OrcsEngine, Session, SessionAsset, World,
};
use orcs_types::{Principal, PrincipalId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// LuaChildLoader implementation for spawning Lua children.
struct AppLuaChildLoader {
    sandbox: Arc<dyn orcs_runtime::sandbox::SandboxPolicy>,
}

impl LuaChildLoader for AppLuaChildLoader {
    fn load(&self, config: &ChildConfig) -> Result<Box<dyn RunnableChild>, SpawnError> {
        // Get script from config (prefer inline over path)
        let script = config
            .script_inline
            .as_ref()
            .ok_or_else(|| SpawnError::Internal("no script_inline in config".into()))?;

        // Create new Lua instance for this child
        let lua = Lua::new();
        let lua_arc = Arc::new(Mutex::new(lua));

        // Create LuaChild from script
        let child = LuaChild::from_script(lua_arc, script, Arc::clone(&self.sandbox))
            .map_err(|e| SpawnError::Internal(format!("failed to load Lua child: {}", e)))?;

        Ok(Box::new(child))
    }
}

/// Builder for [`OrcsApp`].
///
/// Accepts a [`ConfigResolver`] that encapsulates all config resolution
/// logic (file loading, env vars, CLI overrides). The builder only
/// handles launch parameters (e.g., session resume).
///
/// # Example
///
/// ```ignore
/// use orcs_runtime::{ConfigResolver, OrcsConfig, ConfigError};
///
/// struct MyResolver;
/// impl ConfigResolver for MyResolver {
///     fn resolve(&self) -> Result<OrcsConfig, ConfigError> {
///         Ok(OrcsConfig::default())
///     }
/// }
///
/// let app = OrcsApp::builder(MyResolver)
///     .build()
///     .await?;
/// ```
pub struct OrcsAppBuilder {
    /// Configuration resolver (owned, moved into OrcsApp).
    resolver: Box<dyn ConfigResolver>,
    /// Session ID to resume from (launch parameter).
    resume_session_id: Option<String>,
    /// Sandbox policy for file operations.
    sandbox: Option<Arc<dyn orcs_runtime::sandbox::SandboxPolicy>>,
    /// Shared printer slot for tracing + IOOutput routing.
    printer_slot: crate::SharedPrinterSlot,
}

impl OrcsAppBuilder {
    /// Creates a new builder with the given resolver.
    #[must_use]
    pub fn new(resolver: impl ConfigResolver + 'static) -> Self {
        Self {
            resolver: Box::new(resolver),
            resume_session_id: None,
            sandbox: None,
            printer_slot: crate::SharedPrinterSlot::new(),
        }
    }

    /// Sets a shared printer slot for tracing and IOOutput routing.
    ///
    /// The same slot must be passed to the tracing `MakeWriter` so that
    /// log output routes through ExternalPrinter during interactive mode.
    #[must_use]
    pub fn with_printer_slot(mut self, slot: crate::SharedPrinterSlot) -> Self {
        self.printer_slot = slot;
        self
    }

    /// Sets the sandbox policy for file operations.
    ///
    /// Required before calling [`build()`](Self::build).
    #[must_use]
    pub fn with_sandbox(mut self, sandbox: Arc<dyn orcs_runtime::sandbox::SandboxPolicy>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Sets a session ID to resume from.
    #[must_use]
    pub fn resume(mut self, session_id: impl Into<String>) -> Self {
        self.resume_session_id = Some(session_id.into());
        self
    }

    /// Builds the application.
    ///
    /// Resolves configuration via the provided [`ConfigResolver`],
    /// creates the World, Engine, and spawns component runners.
    ///
    /// # Errors
    ///
    /// Returns [`AppError::Config`] if configuration resolution fails.
    pub async fn build(self) -> Result<OrcsApp, AppError> {
        // Resolve configuration (all layers handled by the resolver)
        let config = self
            .resolver
            .resolve()
            .map_err(|e| AppError::Config(e.to_string()))?;

        // Sandbox is required for file operations
        let sandbox = self.sandbox.ok_or_else(|| {
            AppError::Config("sandbox policy not set (use .with_sandbox())".into())
        })?;

        // Create session store
        let session_path = config.paths.session_dir_or_default();
        let store = LocalFileStore::new(session_path)
            .map_err(|e| AppError::Config(format!("Failed to create session store: {e}")))?;

        // Expand builtins to versioned directory on disk
        let builtins_base = config.components.resolved_builtins_dir();
        crate::builtins::ensure_expanded(&builtins_base)
            .map_err(|e| AppError::Config(format!("Failed to expand builtins: {e}")))?;
        let versioned_builtins = crate::builtins::versioned_dir(&builtins_base);

        // Build component loader: user paths (higher priority) + builtins (fallback)
        let component_loader = ScriptLoader::new(Arc::clone(&sandbox))
            .with_paths(config.components.resolved_paths())
            .with_path(&versioned_builtins);

        // Create World with IO channel + pre-allocated component channels
        let component_names = &config.components.load;
        let mut world = World::new();
        let io = world.create_channel(ChannelConfig::interactive());
        let builtin_channels: Vec<_> = component_names
            .iter()
            .map(|_| world.create_channel(ChannelConfig::default()))
            .collect();

        // Create engine with IO channel (required)
        let mut engine = OrcsEngine::new(world, io);

        // Initialize hook registry and load hooks from config
        if !config.hooks.hooks.is_empty() {
            let registry = orcs_runtime::shared_hook_registry();
            let result = orcs_lua::hook_helpers::load_hooks_from_config(
                &config.hooks,
                &registry,
                sandbox.root(),
            );
            for err in &result.errors {
                tracing::warn!(hook = %err.hook_label, error = %err.error, "Hook load failed");
            }
            if result.loaded > 0 {
                tracing::info!(
                    loaded = result.loaded,
                    skipped = result.skipped,
                    errors = result.errors.len(),
                    "Hooks initialized"
                );
                engine.set_hook_registry(registry);
            }
        }

        // Load user scripts from configured directories (auto_load)
        if config.scripts.auto_load {
            let script_dirs = config.scripts.resolve_dirs(Some(sandbox.root()));
            if !script_dirs.is_empty() {
                let script_loader = ScriptLoader::new(Arc::clone(&sandbox)).with_paths(script_dirs);
                let result = script_loader.load_all();

                let loaded_count = result.loaded_count();
                let warning_count = result.warning_count();

                for warn in &result.warnings {
                    tracing::warn!(
                        path = %warn.path.display(),
                        error = %warn.error,
                        "Failed to load user script"
                    );
                }

                for (name, component) in result.loaded {
                    let world_ref = engine.world_read();
                    let channel_id = {
                        let mut w = world_ref.blocking_write();
                        w.create_channel(ChannelConfig::default())
                    };
                    let component_id = component.id().clone();
                    engine.spawn_runner_with_emitter(channel_id, Box::new(component), None);
                    tracing::info!(
                        name = %name,
                        channel = %channel_id,
                        component = %component_id.fqn(),
                        "User script loaded"
                    );
                }

                if loaded_count > 0 {
                    tracing::info!(
                        "Loaded {} user script(s), {} warning(s)",
                        loaded_count,
                        warning_count
                    );
                }
            }
        }

        // Create IO port for ClientRunner
        let (io_port, io_input, io_output) = IOPort::with_defaults(io);
        let principal = Principal::User(PrincipalId::new());

        // Spawn ClientRunner for IO channel (no component - bridge only)
        let (_io_handle, io_event_tx) = engine.spawn_client_runner(io, io_port, principal.clone());
        tracing::info!("ClientRunner spawned: channel={} (IO bridge)", io);

        // Shared auth context
        let elevated_session: Arc<Session> =
            Arc::new(Session::new(principal.clone()).elevate(std::time::Duration::from_secs(3600)));
        let non_elevated_session: Arc<Session> = Arc::new(Session::new(principal.clone()));
        let auth_checker: Arc<dyn PermissionChecker> = Arc::new(DefaultPolicy);
        // Elevated components share grants; non-elevated get their own store.
        // Concrete type so save_session/resume can call restore_grants directly.
        let shared_grants = Arc::new(DefaultGrantStore::new());
        tracing::info!(
            "Auth context created: principal={:?}, elevated={}",
            elevated_session.principal(),
            elevated_session.is_elevated()
        );

        // Load or create session (before component spawn so snapshots are available)
        let (session_asset, is_resumed, restored_count) =
            if let Some(session_id) = &self.resume_session_id {
                tracing::info!("Resuming session: {}", session_id);
                let asset = store.load(session_id).await?;
                let count = asset.component_snapshots.len();

                // Restore grants from previous session
                let saved_grants = asset.granted_commands();
                if !saved_grants.is_empty() {
                    match shared_grants.restore_grants(saved_grants) {
                        Ok(()) => {
                            tracing::info!("Restored {} grant(s) from session", saved_grants.len());
                        }
                        Err(e) => {
                            tracing::error!("Failed to restore grants: {e}");
                        }
                    }
                }

                (asset, true, count)
            } else {
                (SessionAsset::new(), false, 0)
            };

        // Spawn components driven by RuntimeHints
        let mut component_routes = HashMap::new();

        for (name, &channel_id) in component_names.iter().zip(builtin_channels.iter()) {
            let component = component_loader
                .load(name)
                .map_err(|e| AppError::Config(format!("Failed to load component '{name}': {e}")))?;
            let hints = component.runtime_hints();
            let component_id = component.id().clone();

            // Derive spawn config from hints
            let output_tx = if hints.output_to_io {
                Some(io_event_tx.clone())
            } else {
                None
            };
            let session = if hints.elevated {
                Arc::clone(&elevated_session)
            } else {
                Arc::clone(&non_elevated_session)
            };
            let grants: Arc<dyn GrantPolicy> = if hints.elevated {
                Arc::clone(&shared_grants) as Arc<dyn GrantPolicy>
            } else {
                Arc::new(DefaultGrantStore::new())
            };
            let lua_loader: Option<Arc<dyn LuaChildLoader>> = if hints.child_spawner {
                Some(Arc::new(AppLuaChildLoader {
                    sandbox: Arc::clone(&sandbox),
                }))
            } else {
                None
            };

            // Look up initial snapshot for this component (session resume)
            let initial_snapshot = session_asset.get_snapshot(&component_id.fqn()).cloned();

            if initial_snapshot.is_some() {
                tracing::info!(
                    component = %component_id.fqn(),
                    "Restoring snapshot for component"
                );
            }

            // Per-component settings from [components.settings.<name>]
            let component_config = config.components.component_settings(name);

            engine.spawn_runner_full_auth_with_snapshot(
                channel_id,
                Box::new(component),
                output_tx,
                lua_loader,
                session,
                Arc::clone(&auth_checker),
                grants,
                initial_snapshot,
                component_config,
            );

            let short_name = component_id.name.clone();
            component_routes.insert(short_name, channel_id);

            tracing::info!(
                channel = %channel_id,
                component = %component_id.fqn(),
                output_to_io = hints.output_to_io,
                elevated = hints.elevated,
                child_spawner = hints.child_spawner,
                "Spawned component"
            );
        }

        tracing::info!(
            "Component routes registered: {:?}",
            component_routes.keys().collect::<Vec<_>>()
        );

        // Create console renderer based on config
        let renderer = if config.ui.verbose {
            orcs_runtime::io::ConsoleRenderer::verbose()
        } else {
            orcs_runtime::io::ConsoleRenderer::new()
        };

        tracing::debug!("OrcsApp created with config: debug={}", config.debug);

        Ok(OrcsApp {
            config,
            resolver: self.resolver,
            engine,
            principal,
            renderer,
            io_input,
            io_output,
            pending_approval: None,
            store,
            session: session_asset,
            is_resumed,
            restored_count,
            shared_grants,
            component_routes,
            printer_slot: self.printer_slot,
        })
    }
}
