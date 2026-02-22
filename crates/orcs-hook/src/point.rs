//! Hook lifecycle points.
//!
//! Every point where the runtime can invoke registered hooks.
//! Points are categorized as "pre" (can modify/abort), "post" (observe),
//! or "on" (event notification).

use crate::HookError;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// All lifecycle points where hooks can intercept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookPoint {
    // ── Component Lifecycle ──────────────────────────────────
    /// Before `Component::init()`.
    ComponentPreInit,
    /// After `Component::init()` succeeds.
    ComponentPostInit,
    /// Before `Component::shutdown()`.
    ComponentPreShutdown,
    /// After `Component::shutdown()`.
    ComponentPostShutdown,

    // ── Request Processing ───────────────────────────────────
    /// Before `Component::on_request()`.
    RequestPreDispatch,
    /// After `Component::on_request()` completes.
    RequestPostDispatch,

    // ── Signal Processing ────────────────────────────────────
    /// Before signal is dispatched to component.
    SignalPreDispatch,
    /// After signal dispatch completes.
    SignalPostDispatch,

    // ── Child Lifecycle ──────────────────────────────────────
    /// Before `ChildSpawner::spawn()`.
    ChildPreSpawn,
    /// After child is spawned successfully.
    ChildPostSpawn,
    /// Before `RunnableChild::run()`.
    ChildPreRun,
    /// After `RunnableChild::run()` completes.
    ChildPostRun,

    // ── Channel Lifecycle ────────────────────────────────────
    /// Before `OrcsEngine::spawn_runner*()`.
    ChannelPreCreate,
    /// After channel runner is created.
    ChannelPostCreate,
    /// Before channel runner loop exits.
    ChannelPreDestroy,
    /// After channel is fully torn down.
    ChannelPostDestroy,

    // ── Tool Execution ───────────────────────────────────────
    /// Before Lua tool function executes.
    ToolPreExecute,
    /// After Lua tool function completes.
    ToolPostExecute,

    // ── Auth ─────────────────────────────────────────────────
    /// Before permission check.
    AuthPreCheck,
    /// After permission check.
    AuthPostCheck,
    /// On dynamic permission grant.
    AuthOnGrant,

    // ── EventBus ─────────────────────────────────────────────
    /// Before `EventBus::broadcast()`.
    BusPreBroadcast,
    /// After `EventBus::broadcast()`.
    BusPostBroadcast,
    /// On component registration with EventBus.
    BusOnRegister,
    /// On component unregistration from EventBus.
    BusOnUnregister,
}

impl HookPoint {
    /// Returns `true` if this is a "pre" hook (can modify/abort the operation).
    #[must_use]
    pub fn is_pre(&self) -> bool {
        matches!(
            self,
            Self::ComponentPreInit
                | Self::ComponentPreShutdown
                | Self::RequestPreDispatch
                | Self::SignalPreDispatch
                | Self::ChildPreSpawn
                | Self::ChildPreRun
                | Self::ChannelPreCreate
                | Self::ChannelPreDestroy
                | Self::ToolPreExecute
                | Self::AuthPreCheck
                | Self::BusPreBroadcast
        )
    }

    /// Returns `true` if this is a "post" hook (observe-only by default).
    #[must_use]
    pub fn is_post(&self) -> bool {
        matches!(
            self,
            Self::ComponentPostInit
                | Self::ComponentPostShutdown
                | Self::RequestPostDispatch
                | Self::SignalPostDispatch
                | Self::ChildPostSpawn
                | Self::ChildPostRun
                | Self::ChannelPostCreate
                | Self::ChannelPostDestroy
                | Self::ToolPostExecute
                | Self::AuthPostCheck
                | Self::BusPostBroadcast
        )
    }

    /// Returns `true` if this is an "on" event hook (neither pre nor post).
    #[must_use]
    pub fn is_event(&self) -> bool {
        !self.is_pre() && !self.is_post()
    }

    /// Returns the canonical string representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ComponentPreInit => "component.pre_init",
            Self::ComponentPostInit => "component.post_init",
            Self::ComponentPreShutdown => "component.pre_shutdown",
            Self::ComponentPostShutdown => "component.post_shutdown",
            Self::RequestPreDispatch => "request.pre_dispatch",
            Self::RequestPostDispatch => "request.post_dispatch",
            Self::SignalPreDispatch => "signal.pre_dispatch",
            Self::SignalPostDispatch => "signal.post_dispatch",
            Self::ChildPreSpawn => "child.pre_spawn",
            Self::ChildPostSpawn => "child.post_spawn",
            Self::ChildPreRun => "child.pre_run",
            Self::ChildPostRun => "child.post_run",
            Self::ChannelPreCreate => "channel.pre_create",
            Self::ChannelPostCreate => "channel.post_create",
            Self::ChannelPreDestroy => "channel.pre_destroy",
            Self::ChannelPostDestroy => "channel.post_destroy",
            Self::ToolPreExecute => "tool.pre_execute",
            Self::ToolPostExecute => "tool.post_execute",
            Self::AuthPreCheck => "auth.pre_check",
            Self::AuthPostCheck => "auth.post_check",
            Self::AuthOnGrant => "auth.on_grant",
            Self::BusPreBroadcast => "bus.pre_broadcast",
            Self::BusPostBroadcast => "bus.post_broadcast",
            Self::BusOnRegister => "bus.on_register",
            Self::BusOnUnregister => "bus.on_unregister",
        }
    }

    /// All known HookPoint prefix categories (for shorthand parsing in Phase 3).
    pub const KNOWN_PREFIXES: &'static [&'static str] = &[
        "component.",
        "request.",
        "signal.",
        "child.",
        "channel.",
        "tool.",
        "auth.",
        "bus.",
    ];
}

impl FromStr for HookPoint {
    type Err = HookError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "component.pre_init" => Ok(Self::ComponentPreInit),
            "component.post_init" => Ok(Self::ComponentPostInit),
            "component.pre_shutdown" => Ok(Self::ComponentPreShutdown),
            "component.post_shutdown" => Ok(Self::ComponentPostShutdown),
            "request.pre_dispatch" => Ok(Self::RequestPreDispatch),
            "request.post_dispatch" => Ok(Self::RequestPostDispatch),
            "signal.pre_dispatch" => Ok(Self::SignalPreDispatch),
            "signal.post_dispatch" => Ok(Self::SignalPostDispatch),
            "child.pre_spawn" => Ok(Self::ChildPreSpawn),
            "child.post_spawn" => Ok(Self::ChildPostSpawn),
            "child.pre_run" => Ok(Self::ChildPreRun),
            "child.post_run" => Ok(Self::ChildPostRun),
            "channel.pre_create" => Ok(Self::ChannelPreCreate),
            "channel.post_create" => Ok(Self::ChannelPostCreate),
            "channel.pre_destroy" => Ok(Self::ChannelPreDestroy),
            "channel.post_destroy" => Ok(Self::ChannelPostDestroy),
            "tool.pre_execute" => Ok(Self::ToolPreExecute),
            "tool.post_execute" => Ok(Self::ToolPostExecute),
            "auth.pre_check" => Ok(Self::AuthPreCheck),
            "auth.post_check" => Ok(Self::AuthPostCheck),
            "auth.on_grant" => Ok(Self::AuthOnGrant),
            "bus.pre_broadcast" => Ok(Self::BusPreBroadcast),
            "bus.post_broadcast" => Ok(Self::BusPostBroadcast),
            "bus.on_register" => Ok(Self::BusOnRegister),
            "bus.on_unregister" => Ok(Self::BusOnUnregister),
            _ => Err(HookError::UnknownHookPoint(s.to_string())),
        }
    }
}

impl fmt::Display for HookPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All 26 variants for exhaustive testing.
    const ALL_POINTS: &[HookPoint] = &[
        HookPoint::ComponentPreInit,
        HookPoint::ComponentPostInit,
        HookPoint::ComponentPreShutdown,
        HookPoint::ComponentPostShutdown,
        HookPoint::RequestPreDispatch,
        HookPoint::RequestPostDispatch,
        HookPoint::SignalPreDispatch,
        HookPoint::SignalPostDispatch,
        HookPoint::ChildPreSpawn,
        HookPoint::ChildPostSpawn,
        HookPoint::ChildPreRun,
        HookPoint::ChildPostRun,
        HookPoint::ChannelPreCreate,
        HookPoint::ChannelPostCreate,
        HookPoint::ChannelPreDestroy,
        HookPoint::ChannelPostDestroy,
        HookPoint::ToolPreExecute,
        HookPoint::ToolPostExecute,
        HookPoint::AuthPreCheck,
        HookPoint::AuthPostCheck,
        HookPoint::AuthOnGrant,
        HookPoint::BusPreBroadcast,
        HookPoint::BusPostBroadcast,
        HookPoint::BusOnRegister,
        HookPoint::BusOnUnregister,
    ];

    #[test]
    fn all_variants_count() {
        assert_eq!(ALL_POINTS.len(), 25);
    }

    #[test]
    fn from_str_roundtrip_all() {
        for &point in ALL_POINTS {
            let s = point.to_string();
            let parsed: HookPoint = s.parse().unwrap_or_else(|e| {
                panic!("Failed to parse '{s}': {e}");
            });
            assert_eq!(parsed, point, "roundtrip failed for {s}");
        }
    }

    #[test]
    fn from_str_unknown() {
        let result = "foo.bar".parse::<HookPoint>();
        assert!(result.is_err());
        assert!(matches!(
            result.expect_err("unknown hook point 'foo.bar' should return error"),
            HookError::UnknownHookPoint(_)
        ));
    }

    #[test]
    fn from_str_empty() {
        let result = "".parse::<HookPoint>();
        assert!(result.is_err());
    }

    #[test]
    fn is_pre_correct() {
        let pre_points = [
            HookPoint::ComponentPreInit,
            HookPoint::ComponentPreShutdown,
            HookPoint::RequestPreDispatch,
            HookPoint::SignalPreDispatch,
            HookPoint::ChildPreSpawn,
            HookPoint::ChildPreRun,
            HookPoint::ChannelPreCreate,
            HookPoint::ChannelPreDestroy,
            HookPoint::ToolPreExecute,
            HookPoint::AuthPreCheck,
            HookPoint::BusPreBroadcast,
        ];
        for &point in &pre_points {
            assert!(point.is_pre(), "{point} should be pre");
            assert!(!point.is_post(), "{point} should not be post");
        }
    }

    #[test]
    fn is_post_correct() {
        let post_points = [
            HookPoint::ComponentPostInit,
            HookPoint::ComponentPostShutdown,
            HookPoint::RequestPostDispatch,
            HookPoint::SignalPostDispatch,
            HookPoint::ChildPostSpawn,
            HookPoint::ChildPostRun,
            HookPoint::ChannelPostCreate,
            HookPoint::ChannelPostDestroy,
            HookPoint::ToolPostExecute,
            HookPoint::AuthPostCheck,
            HookPoint::BusPostBroadcast,
        ];
        for &point in &post_points {
            assert!(point.is_post(), "{point} should be post");
            assert!(!point.is_pre(), "{point} should not be pre");
        }
    }

    #[test]
    fn event_hooks_are_neither_pre_nor_post() {
        let event_points = [
            HookPoint::AuthOnGrant,
            HookPoint::BusOnRegister,
            HookPoint::BusOnUnregister,
        ];
        for &point in &event_points {
            assert!(!point.is_pre(), "{point} should not be pre");
            assert!(!point.is_post(), "{point} should not be post");
            assert!(point.is_event(), "{point} should be event");
        }
    }

    #[test]
    fn every_variant_is_exactly_one_category() {
        for &point in ALL_POINTS {
            let cats = [point.is_pre(), point.is_post(), point.is_event()];
            let count = cats.iter().filter(|&&v| v).count();
            assert_eq!(count, 1, "{point} should be in exactly 1 category");
        }
    }

    #[test]
    fn serde_roundtrip() {
        for &point in ALL_POINTS {
            let json = serde_json::to_string(&point).expect("HookPoint should serialize to JSON");
            let restored: HookPoint =
                serde_json::from_str(&json).expect("HookPoint should deserialize from JSON");
            assert_eq!(restored, point);
        }
    }
}
