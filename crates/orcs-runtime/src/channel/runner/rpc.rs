//! Shared RPC request helper for Component-to-Component calls.
//!
//! Used by both [`EventEmitter`](super::EventEmitter) and
//! [`ChildContextImpl`](super::ChildContextImpl) to avoid duplicating
//! the FQN resolution → ChannelHandle lookup → Request send logic.

use crate::engine::{SharedChannelHandles, SharedComponentChannelMap};
use orcs_event::{EventCategory, Request};
use orcs_types::{ChannelId, ComponentId};
use serde_json::Value;
use std::time::Duration;

/// Parameters for an RPC request to a target component.
pub(super) struct RpcParams<'a> {
    /// FQN → ChannelId mapping.
    pub component_channel_map: &'a SharedComponentChannelMap,
    /// ChannelId → ChannelHandle mapping.
    pub shared_handles: &'a SharedChannelHandles,
    /// Fully qualified name of the target (e.g. "skill::skill_manager").
    pub target_fqn: &'a str,
    /// Operation name to invoke on the target.
    pub operation: &'a str,
    /// ComponentId of the caller.
    pub source_id: ComponentId,
    /// ChannelId of the caller's owning channel.
    pub source_channel: ChannelId,
    /// Request payload.
    pub payload: Value,
    /// Timeout in milliseconds.
    pub timeout_ms: u64,
}

/// Resolves a target FQN to a ChannelHandle and sends an RPC request.
///
/// This is the async core shared by `EventEmitter::request()` and
/// `ChildContextImpl::request()`. Callers bridge sync→async via
/// `tokio::task::block_in_place`.
pub(super) async fn resolve_and_send_rpc(params: RpcParams<'_>) -> Result<Value, String> {
    let RpcParams {
        component_channel_map,
        shared_handles,
        target_fqn,
        operation,
        source_id,
        source_channel,
        payload,
        timeout_ms,
    } = params;

    // Resolve FQN → ChannelId
    let channel_id = {
        let m = component_channel_map
            .read()
            .map_err(|e| format!("map lock poisoned: {e}"))?;
        *m.get(target_fqn)
            .ok_or_else(|| format!("component not found: {target_fqn}"))?
    };

    // Get ChannelHandle
    let handle = {
        let h = shared_handles
            .read()
            .map_err(|e| format!("handles lock poisoned: {e}"))?;
        h.get(&channel_id)
            .cloned()
            .ok_or_else(|| format!("channel not found for: {target_fqn}"))?
    };

    if !handle.accepts_requests() {
        return Err(format!("component {target_fqn} does not accept requests"));
    }

    // Parse FQN into ComponentId for the Request target field
    let target_id = match target_fqn.split_once("::") {
        Some((ns, name)) => ComponentId::new(ns, name),
        None => ComponentId::new("unknown", target_fqn),
    };

    let req = Request::new(
        EventCategory::Extension {
            namespace: source_id.namespace.clone(),
            kind: operation.to_string(),
        },
        operation,
        source_id,
        source_channel,
        payload,
    )
    .with_target(target_id)
    .with_timeout(timeout_ms);

    // Send request and await response
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .send_request(req, reply_tx)
        .await
        .map_err(|_| "request channel closed".to_string())?;

    match tokio::time::timeout(Duration::from_millis(timeout_ms), reply_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("response channel closed".into()),
        Err(_) => Err(format!("request timeout ({timeout_ms}ms)")),
    }
}
