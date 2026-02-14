//! Lua component testing harness.
//!
//! Provides specialized testing infrastructure for Lua-based components.
//!
//! # Features
//!
//! - Script-based component testing
//! - Request/Signal logging for snapshot testing
//! - Hot reload testing support
//! - Lua-specific error handling
//!
//! # Example
//!
//! ```
//! use orcs_lua::testing::LuaTestHarness;
//! use orcs_component::EventCategory;
//! use orcs_event::SignalResponse;
//! use orcs_runtime::sandbox::ProjectSandbox;
//! use serde_json::json;
//! use std::sync::Arc;
//!
//! let script = r#"
//!     return {
//!         id = "echo",
//!         subscriptions = {"Echo"},
//!         on_request = function(req)
//!             return { success = true, data = req.payload }
//!         end,
//!         on_signal = function(sig)
//!             if sig.kind == "Veto" then return "Abort" end
//!             return "Handled"
//!         end,
//!     }
//! "#;
//!
//! let sandbox = Arc::new(ProjectSandbox::new(".").unwrap());
//! let mut harness = LuaTestHarness::from_script(script, sandbox).unwrap();
//!
//! // Test request
//! let result = harness.request(EventCategory::Echo, "echo", json!({"msg": "hello"}));
//! assert!(result.is_ok());
//!
//! // Test veto signal
//! let response = harness.veto();
//! assert_eq!(response, SignalResponse::Abort);
//! ```

use crate::{LuaComponent, LuaError};
use orcs_component::testing::{ComponentTestHarness, RequestRecord, SignalRecord};
use orcs_component::{Component, ComponentError, EventCategory, Status};
use orcs_event::SignalResponse;
use orcs_runtime::sandbox::SandboxPolicy;
use orcs_types::ComponentId;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Test harness for Lua-based components.
///
/// Wraps [`ComponentTestHarness`] with Lua-specific functionality:
/// - Script loading from string or file
/// - Hot reload testing
/// - Lua error context
pub struct LuaTestHarness {
    /// Inner component test harness.
    inner: ComponentTestHarness<LuaComponent>,
    /// Original script source (for debugging).
    script_source: Option<String>,
}

impl LuaTestHarness {
    /// Creates a test harness from a Lua script string.
    ///
    /// # Arguments
    ///
    /// * `script` - Lua script content
    /// * `sandbox` - Sandbox policy for file operations
    ///
    /// # Errors
    ///
    /// Returns [`LuaError`] if the script is invalid.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_lua::testing::LuaTestHarness;
    /// use orcs_runtime::sandbox::ProjectSandbox;
    /// use std::sync::Arc;
    ///
    /// let script = r#"
    ///     return {
    ///         id = "test",
    ///         subscriptions = {"Echo"},
    ///         on_request = function(req) return { success = true } end,
    ///         on_signal = function(sig) return "Ignored" end,
    ///     }
    /// "#;
    ///
    /// let sandbox = Arc::new(ProjectSandbox::new(".").unwrap());
    /// let harness = LuaTestHarness::from_script(script, sandbox).unwrap();
    /// assert_eq!(harness.id().name, "test");
    /// ```
    pub fn from_script(script: &str, sandbox: Arc<dyn SandboxPolicy>) -> Result<Self, LuaError> {
        let component = LuaComponent::from_script(script, sandbox)?;
        Ok(Self {
            inner: ComponentTestHarness::new(component),
            script_source: Some(script.to_string()),
        })
    }

    /// Creates a test harness from a Lua script file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the Lua script file
    /// * `sandbox` - Sandbox policy for file operations
    ///
    /// # Errors
    ///
    /// Returns [`LuaError`] if the file cannot be read or parsed.
    pub fn from_file<P: AsRef<Path>>(
        path: P,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let script = std::fs::read_to_string(path.as_ref())
            .map_err(|_| LuaError::ScriptNotFound(path.as_ref().display().to_string()))?;
        let component = LuaComponent::from_file(path, sandbox)?;
        Ok(Self {
            inner: ComponentTestHarness::new(component),
            script_source: Some(script),
        })
    }

    /// Creates a test harness from a component directory containing `init.lua`.
    ///
    /// The directory is added to Lua's search paths, enabling `require()` for
    /// co-located modules (e.g. `require("helper")` resolves `helper.lua` in
    /// the same directory).
    ///
    /// # Arguments
    ///
    /// * `dir` - Path to the component directory
    /// * `sandbox` - Sandbox policy for file operations
    ///
    /// # Errors
    ///
    /// Returns [`LuaError`] if `init.lua` not found or script is invalid.
    pub fn from_dir<P: AsRef<Path>>(
        dir: P,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let init_path = dir.as_ref().join("init.lua");
        let script = std::fs::read_to_string(&init_path)
            .map_err(|_| LuaError::ScriptNotFound(init_path.display().to_string()))?;
        let component = LuaComponent::from_dir(dir, sandbox)?;
        Ok(Self {
            inner: ComponentTestHarness::new(component),
            script_source: Some(script),
        })
    }

    /// Returns a reference to the underlying LuaComponent.
    pub fn component(&self) -> &LuaComponent {
        self.inner.component()
    }

    /// Returns a mutable reference to the underlying LuaComponent.
    pub fn component_mut(&mut self) -> &mut LuaComponent {
        self.inner.component_mut()
    }

    /// Returns the original script source.
    pub fn script_source(&self) -> Option<&str> {
        self.script_source.as_deref()
    }

    /// Reloads the component from its script file.
    ///
    /// Only works if the component was loaded from a file.
    ///
    /// # Errors
    ///
    /// Returns [`LuaError`] if reload fails.
    pub fn reload(&mut self) -> Result<(), LuaError> {
        self.inner.component_mut().reload()
    }

    /// Calls `init()` on the component with an empty config.
    ///
    /// # Errors
    ///
    /// Returns the component's initialization error if any.
    pub fn init(&mut self) -> Result<(), ComponentError> {
        self.inner.init()
    }

    /// Calls `init()` on the component with the given config.
    ///
    /// # Errors
    ///
    /// Returns the component's initialization error if any.
    pub fn init_with_config(&mut self, config: &serde_json::Value) -> Result<(), ComponentError> {
        self.inner.init_with_config(config)
    }

    /// Sends a request to the component.
    ///
    /// # Arguments
    ///
    /// * `category` - Event category
    /// * `operation` - Operation name
    /// * `payload` - Request payload
    ///
    /// # Returns
    ///
    /// The result of the request.
    pub fn request(
        &mut self,
        category: EventCategory,
        operation: &str,
        payload: Value,
    ) -> Result<Value, ComponentError> {
        self.inner.request(category, operation, payload)
    }

    /// Sends a Veto signal to the component.
    ///
    /// # Returns
    ///
    /// The component's response (should be `Abort` for properly implemented components).
    pub fn veto(&mut self) -> SignalResponse {
        self.inner.veto()
    }

    /// Sends a Cancel signal for the test channel.
    pub fn cancel(&mut self) -> SignalResponse {
        self.inner.cancel()
    }

    /// Sends an Approve signal.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - The approval request ID
    pub fn approve(&mut self, approval_id: &str) -> SignalResponse {
        self.inner.approve(approval_id)
    }

    /// Sends a Reject signal.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - The approval request ID
    /// * `reason` - Optional rejection reason
    pub fn reject(&mut self, approval_id: &str, reason: Option<String>) -> SignalResponse {
        self.inner.reject(approval_id, reason)
    }

    /// Calls `abort()` on the component.
    pub fn abort(&mut self) {
        self.inner.abort();
    }

    /// Calls `shutdown()` on the component.
    pub fn shutdown(&mut self) {
        self.inner.shutdown();
    }

    /// Returns the current status of the component.
    pub fn status(&self) -> Status {
        self.inner.status()
    }

    /// Returns the component ID.
    pub fn id(&self) -> &ComponentId {
        self.inner.id()
    }

    /// Returns the request log for snapshot testing.
    pub fn request_log(&self) -> &[RequestRecord] {
        self.inner.request_log()
    }

    /// Returns the signal log for snapshot testing.
    pub fn signal_log(&self) -> &[SignalRecord] {
        self.inner.signal_log()
    }

    /// Clears all logs.
    pub fn clear_logs(&mut self) {
        self.inner.clear_logs();
    }

    /// Returns the subscribed event categories.
    pub fn subscriptions(&self) -> &[EventCategory] {
        self.component().subscriptions()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_runtime::sandbox::ProjectSandbox;

    fn test_sandbox() -> Arc<dyn SandboxPolicy> {
        Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
    }

    const ECHO_SCRIPT: &str = r#"
        return {
            id = "echo-test",
            subscriptions = {"Echo"},
            on_request = function(req)
                if req.operation == "echo" then
                    return { success = true, data = req.payload }
                end
                return { success = false, error = "unknown operation" }
            end,
            on_signal = function(sig)
                if sig.kind == "Veto" then
                    return "Abort"
                end
                return "Handled"
            end,
        }
    "#;

    const LIFECYCLE_SCRIPT: &str = r#"
        local state = { initialized = false, shutdown = false }

        return {
            id = "lifecycle-test",
            subscriptions = {"Lifecycle"},

            init = function()
                state.initialized = true
            end,

            shutdown = function()
                state.shutdown = true
            end,

            on_request = function(req)
                if req.operation == "get_state" then
                    return {
                        success = true,
                        data = {
                            initialized = state.initialized,
                            shutdown = state.shutdown
                        }
                    }
                end
                return { success = true }
            end,

            on_signal = function(sig)
                return "Handled"
            end,
        }
    "#;

    #[test]
    fn harness_from_script() {
        let harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_sandbox()).unwrap();
        assert_eq!(harness.id().name, "echo-test");
        assert!(harness.script_source().is_some());
    }

    #[test]
    fn harness_request_success() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_sandbox()).unwrap();

        let result = harness.request(
            EventCategory::Echo,
            "echo",
            serde_json::json!({"msg": "hello"}),
        );

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"msg": "hello"}));
        assert_eq!(harness.request_log().len(), 1);
    }

    #[test]
    fn harness_request_error() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_sandbox()).unwrap();

        let result = harness.request(EventCategory::Echo, "unknown", Value::Null);

        assert!(result.is_err());
        assert_eq!(harness.request_log().len(), 1);
    }

    #[test]
    fn harness_veto_aborts() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_sandbox()).unwrap();

        let response = harness.veto();

        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(harness.status(), Status::Aborted);
        assert_eq!(harness.signal_log().len(), 1);
    }

    #[test]
    fn harness_cancel_handled() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_sandbox()).unwrap();

        let response = harness.cancel();

        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(harness.status(), Status::Idle);
    }

    #[test]
    fn harness_lifecycle() {
        let mut harness = LuaTestHarness::from_script(LIFECYCLE_SCRIPT, test_sandbox()).unwrap();

        // Init
        assert!(harness.init().is_ok());

        // Verify init was called
        let result = harness
            .request(EventCategory::Lifecycle, "get_state", Value::Null)
            .unwrap();
        assert_eq!(result["initialized"], true);
        assert_eq!(result["shutdown"], false);

        // Shutdown
        harness.shutdown();
    }

    #[test]
    fn harness_subscriptions() {
        let harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_sandbox()).unwrap();
        assert_eq!(harness.subscriptions(), &[EventCategory::Echo]);
    }

    #[test]
    fn harness_clear_logs() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_sandbox()).unwrap();

        harness
            .request(EventCategory::Echo, "echo", Value::Null)
            .ok();
        harness.cancel();

        assert_eq!(harness.request_log().len(), 1);
        assert_eq!(harness.signal_log().len(), 1);

        harness.clear_logs();

        assert_eq!(harness.request_log().len(), 0);
        assert_eq!(harness.signal_log().len(), 0);
    }

    #[test]
    fn invalid_script_error() {
        let result = LuaTestHarness::from_script("invalid lua {{{{", test_sandbox());
        assert!(result.is_err());
    }

    #[test]
    fn missing_callback_error() {
        let script = r#"
            return {
                id = "incomplete",
                subscriptions = {"Echo"},
                -- missing on_request and on_signal
            }
        "#;

        let result = LuaTestHarness::from_script(script, test_sandbox());
        assert!(result.is_err());
    }
}
