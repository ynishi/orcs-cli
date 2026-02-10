# ADR-002: Hooks System Design

- **Status**: Draft
- **Date**: 2026-02-10
- **Author**: orcs-core
- **Depends on**: ADR-001 (Permission Model)

---

## 1. Context & Motivation

ORCS は Component → EventBus → ChannelRunner → Child の階層でイベント駆動型実行を行う。
現状、ライフサイクルの各ポイントにクロスカッティングな関心事（ロギング、監査、
capability 注入、ペイロード変換、メトリクス等）を差し込む統合的な仕組みがない。

**目標**: FQL (Fully Qualified Locator) ベースのアドレッシングで、
単一の設定インターフェースからすべてのフックポイントに宣言的にハンドラを登録できる
Hooks システムを設計する。

---

## 2. Hook Points — ライフサイクル全体マップ

コードベース調査の結果、以下の介入ポイントを特定した。

### 2.1 Component Lifecycle

| Hook Point | Timing | 介入箇所 (Runtime) | Context に含まれるデータ |
|---|---|---|---|
| `component.pre_init` | `Component::init()` 呼出し前 | `ChannelRunner::run()` 内 restore+init 前 | component_id, channel_id, snapshot(Option) |
| `component.post_init` | `Component::init()` 成功後 | 同上、init() 成功直後 | component_id, channel_id, init_result |
| `component.pre_shutdown` | `Component::shutdown()` 呼出し前 | `ChannelRunner::run()` ループ脱出後 | component_id, channel_id, status |
| `component.post_shutdown` | `Component::shutdown()` 完了後 | 同上、shutdown() 直後 | component_id, snapshot(Option) |

### 2.2 Request Processing

| Hook Point | Timing | 介入箇所 | Context データ |
|---|---|---|---|
| `request.pre_dispatch` | `Component::on_request()` 呼出し前 | `ChannelRunner::process_event()` / `handle_rpc_request()` | request(全体), component_id, channel_id |
| `request.post_dispatch` | `Component::on_request()` 完了後 | 同上 | request, response(Result), elapsed_ms |

### 2.3 Signal Processing

| Hook Point | Timing | 介入箇所 | Context データ |
|---|---|---|---|
| `signal.pre_dispatch` | `dispatch_signal_to_component()` 前 | `ChannelRunner::handle_signal()` | signal, component_id, channel_id |
| `signal.post_dispatch` | `dispatch_signal_to_component()` 後 | 同上 | signal, signal_response, channel_action |

### 2.4 Child Lifecycle

| Hook Point | Timing | 介入箇所 | Context データ |
|---|---|---|---|
| `child.pre_spawn` | `ChildSpawner::spawn()` 前 | `ChildContextImpl::spawn_child()` | child_config, parent_id, effective_capabilities |
| `child.post_spawn` | `ChildSpawner::spawn()` 成功後 | 同上 | child_id, parent_id, capabilities |
| `child.pre_run` | `RunnableChild::run()` / `handle.run_sync()` 前 | `ChildSpawner::run_child()` | child_id, input |
| `child.post_run` | `RunnableChild::run()` 完了後 | 同上 | child_id, child_result, elapsed_ms |

### 2.5 Channel (Runner) Lifecycle

| Hook Point | Timing | 介入箇所 | Context データ |
|---|---|---|---|
| `channel.pre_create` | `OrcsEngine::spawn_runner*()` 前 | Engine の各 `spawn_runner` variant | channel_id, component_id, runtime_hints |
| `channel.post_create` | `ChannelRunner` タスク起動後 | 同上 | channel_id, handle |
| `channel.pre_destroy` | Runner loop 終了 → snapshot 前 | `ChannelRunner::run()` ループ後 | channel_id, exit_reason |
| `channel.post_destroy` | RunnerResult 収集後 | `OrcsEngine::shutdown_parallel()` | channel_id, snapshot(Option) |

### 2.6 Tool Execution (Lua orcs.*)

| Hook Point | Timing | 介入箇所 | Context データ |
|---|---|---|---|
| `tool.pre_execute` | Lua tool function 実行前 | `orcs_helpers` 各 tool wrapper | tool_name, args, capabilities, sandbox_root |
| `tool.post_execute` | Lua tool function 実行後 | 同上 | tool_name, args, result, elapsed_ms |

### 2.7 Permission / Auth

| Hook Point | Timing | 介入箇所 | Context データ |
|---|---|---|---|
| `auth.pre_check` | Permission check 前 | `ChildContextImpl::check_command()` | command, capabilities, session |
| `auth.post_check` | Permission check 後 | 同上 | command, permission_result |
| `auth.on_grant` | 動的パーミッション付与時 | `ChildContextImpl::grant_command()` | pattern, granted_by |

### 2.8 EventBus Level

| Hook Point | Timing | 介入箇所 | Context データ |
|---|---|---|---|
| `bus.pre_broadcast` | `EventBus::broadcast()` 前 | EventBus | event, target_count |
| `bus.post_broadcast` | `EventBus::broadcast()` 後 | EventBus | event, delivered_count |
| `bus.on_register` | Component 登録時 | `EventBus::register()` | component_id, subscriptions |
| `bus.on_unregister` | Component 登録解除時 | `EventBus::unregister()` | component_id |

---

## 3. FQL (Fully Qualified Locator)

### 3.1 Syntax

```
FQL := <scope> "::" <target> [ "/" <child_path> ] [ "#" <instance> ]
```

| Part | Description | Examples |
|------|-------------|---------|
| `scope` | Namespace | `builtin`, `plugin`, `lua`, `*` (any) |
| `target` | Component name | `llm`, `hil`, `tools`, `*` (any) |
| `child_path` | Child 階層パス | `agent-1`, `agent-1/sub-worker` |
| `instance` | Channel instance | `#primary`, `#0` |

### 3.2 Examples

```
builtin::llm                    # LLM Component (exact)
builtin::*                      # All builtin components
*::*                            # Everything
plugin::my-tool                 # Plugin component
builtin::llm/agent-1            # LLM の child "agent-1"
builtin::llm/*                  # LLM の全 children
lua::custom-comp#primary        # Specific instance
```

### 3.3 Matching Rules

```rust
pub struct FqlPattern {
    scope: PatternSegment,      // Exact("builtin") | Wildcard
    target: PatternSegment,     // Exact("llm") | Wildcard
    child_path: Option<PatternSegment>,
    instance: Option<PatternSegment>,
}

impl FqlPattern {
    /// "builtin::llm" → Exact, Exact, None, None
    /// "*::*" → Wildcard, Wildcard, None, None
    /// "builtin::llm/*" → Exact, Exact, Wildcard, None
    pub fn parse(fql: &str) -> Result<Self, FqlError>;
    pub fn matches(&self, component_id: &ComponentId, child_id: Option<&str>) -> bool;
}
```

FqlPattern は ComponentId.fqn() の内部構造 (`namespace::name`) と直接対応する。
`ComponentId::builtin("llm")` の fqn は `"builtin::llm"` であり、
`ComponentId::new("plugin", "my-tool")` の fqn は `"plugin::my-tool"` である。

---

## 4. Core Types

### 4.1 HookPoint enum

```rust
/// All lifecycle points where hooks can intercept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookPoint {
    // Component Lifecycle
    ComponentPreInit,
    ComponentPostInit,
    ComponentPreShutdown,
    ComponentPostShutdown,

    // Request Processing
    RequestPreDispatch,
    RequestPostDispatch,

    // Signal Processing
    SignalPreDispatch,
    SignalPostDispatch,

    // Child Lifecycle
    ChildPreSpawn,
    ChildPostSpawn,
    ChildPreRun,
    ChildPostRun,

    // Channel Lifecycle
    ChannelPreCreate,
    ChannelPostCreate,
    ChannelPreDestroy,
    ChannelPostDestroy,

    // Tool Execution
    ToolPreExecute,
    ToolPostExecute,

    // Auth
    AuthPreCheck,
    AuthPostCheck,
    AuthOnGrant,

    // EventBus
    BusPreBroadcast,
    BusPostBroadcast,
    BusOnRegister,
    BusOnUnregister,
}

impl std::str::FromStr for HookPoint {
    type Err = HookError;
    /// Parse from string: "request.pre_dispatch" → RequestPreDispatch
    fn from_str(s: &str) -> Result<Self, Self::Err>;
}

impl HookPoint {
    /// Is this a "pre" hook (can modify/abort)?
    pub fn is_pre(&self) -> bool;

    /// Is this a "post" hook (observe-only by default)?
    pub fn is_post(&self) -> bool;
}
```

### 4.2 HookContext

```rust
/// Context passed to hook handlers.
///
/// Pre-hooks can modify `payload` to alter the downstream operation.
/// Post-hooks receive the final result in `result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    /// Which hook point triggered this.
    pub hook_point: HookPoint,

    /// The component targeted by this hook (from FQL match).
    pub component_id: ComponentId,

    /// The channel where this is happening.
    pub channel_id: ChannelId,

    /// Who initiated the action.
    pub principal: Principal,

    /// Monotonic timestamp (ms since engine start).
    pub timestamp_ms: u64,

    /// Hook-specific payload (varies by HookPoint).
    ///
    /// Pre hooks: the input data (request, config, args, etc.)
    /// Post hooks: the result data (response, child_result, etc.)
    pub payload: Value,

    /// Mutable metadata bag — hooks can store cross-hook state here.
    /// Carried from pre → post for the same operation.
    pub metadata: HashMap<String, Value>,

    /// 再帰防止カウンタ。hook 内から orcs.get() 等を呼ぶと
    /// 再帰的に hook が発火するため、depth で打ち切る。
    /// dispatch 時にインクリメントされる。
    pub depth: u8,

    /// 最大再帰深度 (デフォルト: 4)。
    pub max_depth: u8,
}
```

### 4.3 HookAction (return value from handlers)

```rust
/// What the hook wants the runtime to do after execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookAction {
    /// Continue with (possibly modified) context.
    Continue(HookContext),

    /// Skip the operation entirely (pre-hooks only).
    /// The `Value` is returned as the operation's result.
    Skip(Value),

    /// Abort the operation with an error (pre-hooks only).
    Abort { reason: String },

    /// Replace the result with a different value (post-hooks only).
    Replace(Value),
}

// NOTE: Default 実装は意図的に提供しない。
// `Continue(HookContext::empty())` は元コンテキストを破壊するため危険。
// Hook handler は明示的に HookAction を返すこと。
```

### 4.4 Hook trait

```rust
/// A single hook handler.
pub trait Hook: Send + Sync {
    /// Unique identifier for this hook.
    fn id(&self) -> &str;

    /// FQL pattern this hook matches.
    fn fql_pattern(&self) -> &FqlPattern;

    /// Which lifecycle point this hook fires on.
    fn hook_point(&self) -> HookPoint;

    /// Priority (lower = earlier). Default: 100.
    fn priority(&self) -> i32 { 100 }

    /// Execute the hook.
    ///
    /// TODO: Error handling strategy (Phase 2+)
    /// - Lua runtime error → log + skip this hook, continue chain
    /// - Panic → catch_unwind + log + skip this hook, continue chain
    /// - 戻り値を Result<HookAction, HookError> に変更するかは Phase 2 で判断
    fn execute(&self, ctx: HookContext) -> HookAction;
}
```

---

## 5. HookRegistry

### 5.1 並行モデル設計

コードベース調査に基づく Lock 選定:

| リソース (既存) | Lock 型 | 理由 |
|---|---|---|
| `World` | `tokio::sync::RwLock` | `comp.lock().await` 等、await 跨ぎあり |
| `Component` | `tokio::sync::Mutex` | 同上 |
| `SharedChannelHandles` | `std::sync::RwLock` | sync quick access、await 跨がない |
| `SharedComponentChannelMap` | `std::sync::RwLock` | 同上 |
| `ChildSpawner` | `std::sync::Mutex` | 同上 |

**HookRegistry** の dispatch は:
- async context（`process_event()`, `handle_signal()`）から呼ばれるが
- hook handler 自体は同期（Lua callback も同期）
- lock を await 跨ぎで保持しない
- read-heavy（dispatch: 全イベント）、write-rare（register/unregister: 起動・停止時のみ）

→ **`std::sync::RwLock`** を採用。`SharedChannelHandles` と同じパターン。

```
HookRegistry の共有型:
  Arc<std::sync::RwLock<HookRegistry>>

既存パターンと同一:
  SharedChannelHandles = Arc<std::sync::RwLock<HashMap<ChannelId, ChannelHandle>>>
```

> **NOTE**: EventBus に `// TODO: migrate to parking_lot::RwLock` コメントあり。
> `parking_lot` は現在 transitive dependency のみ（直接依存なし）。
> HookRegistry と SharedChannelHandles の parking_lot 移行は別 issue で一括対応が望ましい。

### 5.2 Struct & API

```rust
/// Central registry for all hooks.
///
/// Thread-safe: wrapped in Arc<std::sync::RwLock<>> at the engine level.
/// SharedChannelHandles と同じ並行パターンを採用。
///
/// - dispatch (read lock): 全 event/signal/request で呼ばれる hot path
/// - register/unregister (write lock): 起動・停止時のみ
pub struct HookRegistry {
    /// Hooks indexed by HookPoint for O(1) lookup.
    hooks: HashMap<HookPoint, Vec<RegisteredHook>>,
    /// Empty slice for missing hook points (allocation 回避).
    _empty: Vec<RegisteredHook>,
}

struct RegisteredHook {
    hook: Box<dyn Hook>,
    enabled: bool,
    /// Hook を登録した Component (自動 unregister 用)。
    /// Config 由来の hook は None。
    owner: Option<ComponentId>,
}

impl HookRegistry {
    pub fn new() -> Self;

    /// Register a hook. Returns its ID.
    pub fn register(&mut self, hook: Box<dyn Hook>) -> String;

    /// Register a hook with owner (Component shutdown 時に自動 unregister される).
    pub fn register_owned(&mut self, hook: Box<dyn Hook>, owner: ComponentId) -> String;

    /// Unregister by ID.
    pub fn unregister(&mut self, id: &str) -> bool;

    /// Unregister all hooks owned by the given component.
    /// Component shutdown 時に ChannelRunner から呼ばれる。
    pub fn unregister_by_owner(&mut self, owner: &ComponentId) -> usize;

    /// Enable/disable without removing.
    pub fn set_enabled(&mut self, id: &str, enabled: bool);

    /// Dispatch: run all matching hooks for a given point + target.
    ///
    /// Hooks are executed in priority order (ascending).
    /// Pre-hook: Skip or Abort → chain stops.
    /// Post-hook: Replace → payload 差し替え後チェーン継続 (後続 hook は差し替え後の値を参照)。
    pub fn dispatch(
        &self,
        point: HookPoint,
        component_id: &ComponentId,
        child_id: Option<&str>,
        ctx: HookContext,
    ) -> HookAction {
        let hooks = self.hooks.get(&point).unwrap_or(&self._empty);
        let mut current_ctx = ctx;

        for rh in hooks.iter().filter(|rh| rh.enabled) {
            if !rh.hook.fql_pattern().matches(component_id, child_id) {
                continue;
            }

            // depth check (再帰防止)
            if current_ctx.depth >= current_ctx.max_depth {
                tracing::warn!(
                    hook_id = rh.hook.id(),
                    depth = current_ctx.depth,
                    "hook chain depth exceeded, skipping"
                );
                break;
            }

            match rh.hook.execute(current_ctx.clone()) {
                HookAction::Continue(new_ctx) => {
                    current_ctx = new_ctx;
                }
                action @ (HookAction::Skip(_) | HookAction::Abort { .. }) => {
                    // Pre-hook aborted the chain
                    return action;
                }
                HookAction::Replace(value) => {
                    if point.is_post() {
                        // Post-hook: payload を差し替えてチェーン継続
                        current_ctx.payload = value;
                    } else {
                        // Pre-hook で Replace は無効 → Continue 扱い
                        tracing::warn!(
                            hook_id = rh.hook.id(),
                            "Replace returned from pre-hook, ignoring"
                        );
                    }
                }
            }
        }

        HookAction::Continue(current_ctx)
    }
}
```

---

## 6. Configuration — 統合的な宣言

### 6.1 TOML (OrcsConfig に追加)

```toml
# orcs.toml

[[hooks]]
id = "audit-requests"
fql = "builtin::*"
point = "request.pre_dispatch"
script = "hooks/audit.lua"
priority = 50

[[hooks]]
id = "narrow-child-caps"
fql = "builtin::llm"
point = "child.pre_spawn"
script = "hooks/narrow-caps.lua"
priority = 100

[[hooks]]
id = "tool-metrics"
fql = "*::*"
point = "tool.post_execute"
handler_inline = """
function(ctx)
    orcs.emit_output("[metric] " .. ctx.payload.tool_name .. ": " .. ctx.payload.elapsed_ms .. "ms")
    return { action = "continue", ctx = ctx }
end
"""
priority = 200
```

### 6.2 HooksConfig (新規)

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HooksConfig {
    /// Declarative hook definitions.
    pub hooks: Vec<HookDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookDef {
    /// Unique hook ID (auto-generated if not specified).
    pub id: Option<String>,

    /// FQL pattern: which components this hook targets.
    pub fql: String,

    /// Hook point: when this hook fires.
    pub point: String,

    /// Path to Lua script handler (relative to scripts.dirs).
    pub script: Option<String>,

    /// Inline Lua handler (for simple hooks).
    pub handler_inline: Option<String>,

    /// Priority (lower = earlier). Default: 100.
    #[serde(default = "default_priority")]
    pub priority: i32,

    /// Whether the hook is enabled. Default: true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_priority() -> i32 { 100 }
fn default_enabled() -> bool { true }
```

### 6.3 OrcsConfig への統合

```rust
pub struct OrcsConfig {
    pub debug: bool,
    pub model: ModelConfig,
    pub hil: HilConfig,
    pub paths: PathsConfig,
    pub ui: UiConfig,
    pub scripts: ScriptsConfig,
    pub hooks: HooksConfig,       // ← 追加
}
```

merge ロジックは `hooks` の場合 accumulate（追記）方式とする:

```rust
impl HooksConfig {
    fn merge(&mut self, other: &Self) {
        // Hook definitions accumulate across config layers.
        // Later layers can override by same `id`.
        for hook in &other.hooks {
            if let Some(id) = &hook.id {
                // Override existing hook with same ID
                self.hooks.retain(|h| h.id.as_deref() != Some(id));
            }
            self.hooks.push(hook.clone());
        }
    }
}
```

---

## 7. Lua Hook API — `orcs.hook()`

### 7.1 Registration (Lua 側)

```lua
-- 簡潔な形式
orcs.hook("builtin::llm:request.pre_dispatch", function(ctx)
    orcs.log("info", "LLM request: " .. ctx.payload.operation)
    return ctx  -- continue
end)

-- 詳細な形式
orcs.hook({
    id = "my-audit-hook",
    fql = "builtin::*",
    point = "request.pre_dispatch",
    priority = 50,
    handler = function(ctx)
        -- ctx.payload は request オブジェクト
        -- ctx.component_id, ctx.channel_id, ctx.principal 等が参照可能
        if ctx.payload.operation == "dangerous_op" then
            return { action = "abort", reason = "blocked by policy" }
        end

        -- payload を変更して continue
        ctx.metadata.audit_id = orcs.uuid()
        return { action = "continue", ctx = ctx }
    end,
})
```

### 7.2 Shorthand 構文

```lua
-- "{FQL}:{HookPoint}" を単一文字列でパース
orcs.hook("builtin::llm:request.pre_dispatch", handler_fn)
-- ↓ 内部的に分解
--   fql   = "builtin::llm"
--   point = "request.pre_dispatch"
```

パース規則: 最後の `:` で分割し、左辺を FQL、右辺を HookPoint とする。
FQL 内の `::` は namespace separator であり `:` (single colon) とは区別される。

### 7.3 Context → Lua テーブル変換

```lua
-- ctx は以下の構造で渡される:
ctx = {
    hook_point = "request.pre_dispatch",
    component_id = "builtin::llm",
    channel_id = "550e8400-...",
    principal = { type = "User", id = "..." },
    timestamp_ms = 12345678,

    -- hook_point 固有のペイロード
    payload = {
        -- request.pre_dispatch の場合:
        id = "req-001",
        category = "UserInput",
        operation = "chat",
        source = "builtin::cli",
        target = "builtin::llm",
        payload = { message = "Hello" },
        timeout_ms = 30000,
    },

    -- 前の hook が設定したメタデータ
    metadata = {},
}
```

### 7.4 Return 値規約

```lua
-- 1. Pass-through (何もしない)
return ctx

-- 2. Modified context
ctx.payload.operation = "modified_op"
return { action = "continue", ctx = ctx }

-- 3. Skip the operation
return { action = "skip", result = { skipped = true } }

-- 4. Abort
return { action = "abort", reason = "policy violation" }

-- 5. Replace result (post-hook only)
return { action = "replace", result = { custom = "response" } }
```

---

## 8. Injection Pattern — Runtime 統合

### 8.1 概念

```
orcs.exec("ls -la")
  │
  ├─ [1] HookRegistry::dispatch(ToolPreExecute, ctx_with_args)
  │      ↓ hooks chain: audit → validate → transform
  │      ↓ HookAction::Continue(modified_ctx)
  │
  ├─ [2] actual tool_exec(modified_ctx.payload.args...)
  │      ↓ result
  │
  └─ [3] HookRegistry::dispatch(ToolPostExecute, ctx_with_result)
         ↓ hooks chain: metrics → logging
         ↓ HookAction::Continue / Replace
```

### 8.2 ChannelRunner への注入

```rust
// ChannelRunner に HookRegistry を持たせる
pub struct ChannelRunner {
    // ... existing fields ...
    hook_registry: Option<Arc<RwLock<HookRegistry>>>,
}

impl ChannelRunner {
    // NOTE: 実際の signature は async fn process_event(&self, event: Event, is_direct: bool)
    // Event から Request::new() で Request を構築している。
    // hook 注入はこの構築前後に行う。
    async fn process_event(&self, event: Event, is_direct: bool) {
        let request = Request::new(event.category, &event.operation, ...);
        let component = self.component.clone();
        let comp_id = component.lock().id().clone();

        // === Pre-hook ===
        if let Some(registry) = &self.hook_registry {
            let ctx = HookContext::for_request(&request, &comp_id, &self.id);
            match registry.read().dispatch(
                HookPoint::RequestPreDispatch,
                &comp_id,
                None,
                ctx,
            ) {
                HookAction::Continue(ctx) => {
                    // Update request from potentially modified context
                    // request.payload = ctx.payload;
                }
                HookAction::Skip(value) => {
                    // Return skip value as response
                    return;
                }
                HookAction::Abort { reason } => {
                    tracing::warn!("Hook aborted request: {reason}");
                    return;
                }
                _ => {}
            }
        }

        // === Actual dispatch ===
        let result = component.lock().on_request(&request);

        // === Post-hook ===
        if let Some(registry) = &self.hook_registry {
            let ctx = HookContext::for_request_result(&request, &result, &comp_id, &self.id);
            registry.read().dispatch(
                HookPoint::RequestPostDispatch,
                &comp_id,
                None,
                ctx,
            );
        }
    }
}
```

### 8.3 ChildContextImpl への注入

```rust
impl ChildContext for ChildContextImpl {
    fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
        // === Pre-hook ===
        let ctx = HookContext::for_child_spawn(&config, self.parent_id(), self.capabilities());
        if let Some(registry) = &self.hook_registry {
            match registry.read().dispatch(
                HookPoint::ChildPreSpawn,
                &self.parent_component_id,
                None,
                ctx,
            ) {
                HookAction::Continue(ctx) => {
                    // 可能: ctx から config を再構築 (capabilities 変更等)
                }
                HookAction::Abort { reason } => {
                    return Err(SpawnError::PermissionDenied(reason));
                }
                _ => {}
            }
        }

        // ... existing spawn logic ...

        // === Post-hook ===
        if let Some(registry) = &self.hook_registry {
            let post_ctx = HookContext::for_child_spawned(&child_id, self.parent_id());
            registry.read().dispatch(
                HookPoint::ChildPostSpawn,
                &self.parent_component_id,
                Some(&child_id),
                post_ctx,
            );
        }

        Ok(handle)
    }
}
```

### 8.4 Lua Tool Function への注入

```rust
// tools.rs 内の各ツール関数に共通パターンを適用
fn tool_with_hooks<F, R>(
    tool_name: &str,
    args: &Value,
    hook_registry: Option<&Arc<RwLock<HookRegistry>>>,
    component_id: &ComponentId,
    actual_fn: F,
) -> Result<R, String>
where
    F: FnOnce(&Value) -> Result<R, String>,
    R: Into<Value>,
{
    // Pre-hook
    if let Some(registry) = hook_registry {
        let ctx = HookContext::for_tool(tool_name, args, component_id);
        match registry.read().dispatch(
            HookPoint::ToolPreExecute,
            component_id,
            None,
            ctx,
        ) {
            HookAction::Abort { reason } => return Err(reason),
            HookAction::Skip(val) => return Ok(val.into()),
            _ => {}
        }
    }

    // Execute
    let start = Instant::now();
    let result = actual_fn(args)?;
    let elapsed = start.elapsed().as_millis();

    // Post-hook
    if let Some(registry) = hook_registry {
        let ctx = HookContext::for_tool_result(tool_name, args, &result, elapsed);
        registry.read().dispatch(
            HookPoint::ToolPostExecute,
            component_id,
            None,
            ctx,
        );
    }

    Ok(result)
}
```

---

## 9. Crate 構造

### 9.1 新規 crate: `orcs-hook`

```
crates/orcs-hook/
├── Cargo.toml
└── src/
    ├── lib.rs          // pub mod re-exports
    ├── point.rs        // HookPoint enum
    ├── context.rs      // HookContext struct
    ├── action.rs       // HookAction enum
    ├── hook.rs         // Hook trait
    ├── fql.rs          // FqlPattern parser & matcher
    ├── registry.rs     // HookRegistry
    └── config.rs       // HooksConfig, HookDef
```

### 9.2 依存関係

```
orcs-types ──────────────────────┐
orcs-event ──────────────────────┤
orcs-auth ───────────────────────┤
                                 ▼
                            orcs-hook (NEW)
                                 │
          ┌──────────────────────┼──────────────────┐
          ▼                      ▼                  ▼
     orcs-runtime           orcs-lua           orcs-component
     (注入ポイント)        (Lua hook API)     (型参照のみ)
```

`orcs-hook` は SDK 層 (types, event, auth) に依存し、
Runtime 層 (runtime, lua) が `orcs-hook` に依存する。

### 9.3 Cargo.toml

```toml
[package]
name = "orcs-hook"
version = "0.1.0"
edition = "2021"

[dependencies]
orcs-types = { path = "../orcs-types" }
orcs-event = { path = "../orcs-event" }
orcs-auth = { path = "../orcs-auth" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
```

---

## 10. `orcs.get()` + `hook_context()` パターン

ユーザ指定のイディオム:

```
{FQL}:PreSpawnHook: ...
=> Injection => orcs.get(arg..., hook_context(<-)...)
```

これを以下のように具体化する。

### 10.1 `orcs.get()` — コンテキスト付きリソース取得

Lua から Runtime リソースを取得する際、hook_context を自動注入する:

```lua
-- 現行: orcs.exec("ls -la")
-- 新規: hook-aware 版が透過的に動作

-- orcs.get() はリソース取得の統合API
-- 内部で hook_context を自動構築し、Pre/Post hook を発火する

local result = orcs.get("exec", "ls -la", {
    -- hook_context は自動注入されるが、明示的にメタデータを渡すことも可能
    metadata = { audit_trail = true },
})

-- orcs.get() の内部動作:
-- 1. hook_context を構築: { tool="exec", args={"ls -la"}, caller=current_component, ... }
-- 2. ToolPreExecute hooks を発火 (hook_context を渡す)
-- 3. 実際の tool_exec を実行
-- 4. ToolPostExecute hooks を発火
-- 5. result を返却
```

### 10.2 既存 API との互換

`orcs.exec()`, `orcs.read()` 等の既存 API はそのまま動作する。
内部実装を `orcs.get()` パイプラインに統一する:

```lua
-- orcs.exec(cmd) は内部的に以下と等価になる:
function orcs.exec(cmd)
    return orcs.get("exec", cmd)
end

-- orcs.read(path) は:
function orcs.read(path)
    return orcs.get("read", path)
end
```

ただし、これは **Rust 側のラッパー層** で実現し、
Lua 側の API シグネチャは変更しない（後方互換性維持）。

### 10.3 `hook_context(<-)` — 上流コンテキストの逆注入

`<-` は「呼び出し元のコンテキストを自動的に引き込む」意味:

```rust
// Rust 側実装 (tools.rs 内)
fn build_hook_context_from_caller(
    tool_name: &str,
    args: &Value,
    // ← これらは ChildContextImpl から自動注入
    component_id: &ComponentId,
    channel_id: &ChannelId,
    principal: &Principal,
    capabilities: Capability,
) -> HookContext {
    HookContext {
        hook_point: HookPoint::ToolPreExecute,
        component_id: component_id.clone(),
        channel_id: channel_id.clone(),
        principal: principal.clone(),
        timestamp_ms: now_ms(),
        payload: json!({
            "tool": tool_name,
            "args": args,
            "capabilities": capabilities.bits(),
        }),
        metadata: HashMap::new(),
    }
}
```

Lua child が `orcs.exec()` を呼ぶとき、ChildContextImpl が保持する
`parent_id`, `channel_id`, `capabilities` 等が hook_context に自動注入される。
Child は hook の存在を意識しない。

---

## 11. Builtin Hook 実装例

### 11.1 Audit Trail Hook

```lua
-- hooks/audit.lua
return {
    id = "audit-trail",
    fql = "*::*",
    point = "request.pre_dispatch",
    priority = 10,  -- 最初に実行

    handler = function(ctx)
        local entry = {
            timestamp = ctx.timestamp_ms,
            component = ctx.component_id,
            operation = ctx.payload.operation,
            principal = ctx.principal,
        }
        orcs.log("info", "[AUDIT] " .. orcs.json_encode(entry))
        return ctx  -- pass-through
    end,
}
```

### 11.2 Capability Narrowing Hook

```lua
-- hooks/narrow-caps.lua
return {
    id = "narrow-child-capabilities",
    fql = "builtin::llm",
    point = "child.pre_spawn",
    priority = 100,

    handler = function(ctx)
        -- LLM が spawn する child から EXECUTE 権限を除去
        local caps = ctx.payload.capabilities
        if caps then
            -- EXECUTE bit (0b00001000 = 8) を除去
            ctx.payload.capabilities = caps & ~8
        end
        return { action = "continue", ctx = ctx }
    end,
}
```

### 11.3 Request Rate Limiting Hook

```lua
-- hooks/rate-limit.lua
local request_counts = {}
local WINDOW_MS = 60000  -- 1分
local MAX_REQUESTS = 100

return {
    id = "rate-limiter",
    fql = "*::*",
    point = "request.pre_dispatch",
    priority = 20,

    handler = function(ctx)
        local key = ctx.component_id .. ":" .. ctx.payload.operation
        local now = ctx.timestamp_ms
        local entry = request_counts[key] or { count = 0, window_start = now }

        if now - entry.window_start > WINDOW_MS then
            entry = { count = 0, window_start = now }
        end

        entry.count = entry.count + 1
        request_counts[key] = entry

        if entry.count > MAX_REQUESTS then
            return { action = "abort", reason = "rate limit exceeded: " .. key }
        end

        return ctx
    end,
}
```

---

## 12. Performance Considerations

### 12.1 Zero-Cost When Unused

- `hook_registry: Option<Arc<RwLock<HookRegistry>>>` — None のとき完全スキップ
- Hook なしの場合、既存パスと同一（分岐コストのみ）

### 12.2 Dispatch 最適化

- HookPoint ごとに Vec をプレソート (priority 順) して保持
- FqlPattern のマッチングはプレコンパイル (正規表現不使用、文字列比較のみ)
- RwLock: read は並行可、write は registration/unregistration 時のみ

### 12.3 Lua Hook のオーバーヘッド

- Lua VM は Component 単位で既に存在 → 追加 VM コスト無し
- hook handler は Lua function reference → 関数呼出しコストのみ
- 大量の hook を高頻度で発火する場合は priority フィルタで制御

---

## 13. Implementation Phases

### Phase 1: Core Types + Registry (orcs-hook crate)

- [ ] `HookPoint` enum
- [ ] `FqlPattern` parser & matcher
- [ ] `HookContext` struct
- [ ] `HookAction` enum
- [ ] `Hook` trait
- [ ] `HookRegistry` struct
- [ ] `HooksConfig` + `HookDef` (serde)
- [ ] Unit tests

### Phase 2: Runtime Integration

- [ ] `OrcsConfig` に `hooks` フィールド追加
- [ ] `ChannelRunnerBuilder` に `.with_hook_registry()` 追加
- [ ] `ChannelRunner::process_event()` に pre/post hook 注入
- [ ] `ChannelRunner::handle_signal()` に pre/post hook 注入
- [ ] `ChannelRunner::run()` の init/shutdown に hook 注入
- [ ] `ChildContextImpl` に hook registry 伝播
- [ ] `ChildSpawner` に pre/post spawn hook 注入

### Phase 3: Lua Integration

- [ ] `orcs.hook()` 関数登録 (LuaEnv / orcs_helpers)
- [ ] HookContext ↔ Lua table 変換
- [ ] HookAction ← Lua return value パース
- [ ] `orcs.get()` 統合 API (オプション)
- [ ] TOML `[[hooks]]` → Lua hook 自動ロード

### Phase 4: Tool Hooks + Advanced

- [ ] `tool_with_hooks()` wrapper (tools.rs)
- [ ] 既存 tool 関数の hook-aware 化
- [ ] Auth hooks (pre_check, post_check, on_grant)
- [ ] EventBus hooks (broadcast, register/unregister)
- [ ] `orcs.unhook()` — 動的登録解除
- [ ] Hook の hot-reload (スクリプト変更検知)

---

## 14. Security Considerations

1. **Hook は Capability の範囲内でのみ動作する**
   - hook handler 自体が `orcs.exec()` 等を呼ぶ場合、
     通常の capability / sandbox チェックが適用される

2. **Pre-hook による Abort は監査ログに記録する**
   - hook が操作をブロックした場合、理由を含めてログ出力

3. **Hook 登録は Config または Component init 時のみ**
   - Runtime 中の動的登録は `Capability::SPAWN` を要求
   - Lua `orcs.hook()` は Component の init() 内でのみ推奨

4. **Hook Chain の無限ループ防止**
   - hook 内から `orcs.get()` を呼んだ場合、再帰的に hook が発火する可能性
   - → `HookContext` に `depth` カウンタを持ち、最大 depth (デフォルト: 4) で打ち切り

---

## 15. Open Questions

1. **Async hooks**: 現在の設計は同期的。`async fn execute()` にすべきか？
   - ChannelRunner は tokio::select! ループなので async は自然だが、
     Lua callback は同期。mlua の async 呼出しとの兼ね合い。
   - **調査結果 (2026-02-10)**: コードベースの並行パターンを調査した結果、
     `SharedChannelHandles` 等の sync quick-access リソースは `std::sync::RwLock` を使用。
     HookRegistry も同パターンで `std::sync::RwLock` を採用する (Section 5.1 参照)。
   - **決定**: Phase 1 は同期。Phase 4 で async hook オプションを追加する場合は
     `Hook` trait に `async_execute()` を追加する形で後方互換を維持。

2. **Hook 間のデータ共有**: `metadata` bag で十分か、より構造化が必要か？
   - **提案**: Phase 1 は `HashMap<String, Value>` で十分。
     必要に応じて typed metadata API を追加。

3. **Hook の順序保証**: 同一 priority の場合の実行順序は？
   - **提案**: 登録順 (FIFO)。ドキュメント化して明示。

4. **Remote hooks**: gRPC/HTTP 経由の外部 hook は将来サポートするか？
   - **提案**: Phase 1 ではスコープ外。Hook trait の設計が
     将来の async/remote 拡張を阻害しないことのみ確認。

---

## 16. 実装時注意事項

レビュー (2026-02-10) で検出された設計上の注意点。実装者は各 Phase で該当項目を確認すること。

### 16.1 Error Handling (TODO — Phase 2+)

Hook handler の実行エラー（Lua runtime error, panic 等）の扱いが未定義。
Phase 1 では `execute()` の戻り値は `HookAction` のまま実装し、
Phase 2 で `Result<HookAction, HookError>` への移行を判断する。

Phase 1 の暫定対応:
- Lua hook error → `tracing::warn!` + skip this hook, continue chain
- Rust hook panic → `std::panic::catch_unwind` + skip this hook, continue chain
- エラー発生した hook の ID と error message をログに記録

### 16.2 FQL Shorthand パース (Phase 3)

`"{FQL}:{HookPoint}"` の shorthand パースで、最後の `:` で分割する規則は
child_path に `:` を含むケースで破綻する可能性がある。

実装時の選択肢:
- **A案**: 区切り文字を `@` に変更 (`"builtin::llm@request.pre_dispatch"`)
- **B案**: 最後の `:` の直後が known HookPoint prefix
  (`component.`, `request.`, `signal.`, `child.`, `channel.`, `tool.`, `auth.`, `bus.`)
  に一致する場合のみ分割
- **C案**: FQL と HookPoint を常に別引数で渡す（shorthand 廃止）

Phase 3 実装時に確定する。FqlPattern 自体のパーサーは `ComponentId::fqn()` の
`"namespace::name"` フォーマットに完全準拠させること（独自パーサーを書かず、
可能なら `ComponentId::matches()` を内部利用）。

### 16.3 Hook ライフサイクル管理 (Phase 2-3)

Lua `orcs.hook()` で動的登録された hook は、Component shutdown 時に
自動 unregister しないと二重登録のリスクがある。

対応方針:
- `RegisteredHook` に `owner: Option<ComponentId>` を保持 (Section 5.2 に反映済み)
- `ChannelRunner::run()` の shutdown sequence で `registry.unregister_by_owner(comp_id)` を呼ぶ
- Config 由来 hook は `owner: None` で永続

### 16.4 Post-hook の Replace セマンティクス (Phase 2)

post-hook チェーンで `Replace(value)` が返された場合:
- payload を差し替え後、**チェーンを継続**する (Section 5.2 dispatch に反映済み)
- 後続の post-hook（metrics, logging 等）は差し替え後の値を参照する
- これにより metrics hook が常に最終値を観測できる

### 16.5 HookContext Clone コスト (Phase 4 最適化)

dispatch チェーンで `current_ctx.clone()` が毎回発生する。
Phase 1 では clone で問題ないが、高頻度 hook（全 event dispatch 等）で
パフォーマンス劣化が観測された場合は以下を検討:

- benchmark ポイント: hook 0個 / 10個 / 100個 での dispatch latency
- 最適化候補: `Cow<HookContext>` または `&mut HookContext` 渡し

### 16.6 TOML handler_inline の制限 (Phase 3)

TOML multi-line string 内の Lua コードはエスケープ・シンタックスハイライトの問題がある。
`handler_inline` はワンライナー専用とし、複数行の hook は `script` 参照を推奨する旨を
ドキュメント化すること。
