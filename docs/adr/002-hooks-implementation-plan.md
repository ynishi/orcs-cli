# ADR-002 実装計画

**Base document**: `002-hooks-system-design.md`
**Date**: 2026-02-10

---

## Phase 1: Core Types + Registry (`orcs-hook` crate)

Phase 1 はコンパイル可能で完全にテスト済みの `orcs-hook` crate を単体で完成させる。
Runtime 層への注入は Phase 2 以降。既存コードの変更はワークスペース Cargo.toml のみ。

### Step 1.1: Crate scaffold

**作業内容:**

```
crates/orcs-hook/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── point.rs
    ├── context.rs
    ├── action.rs
    ├── hook.rs
    ├── fql.rs
    ├── registry.rs
    ├── config.rs
    └── error.rs
```

**ファイル: `Cargo.toml`**

```toml
[package]
name = "orcs-hook"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
orcs-types = { workspace = true }
orcs-event = { workspace = true }
orcs-auth = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tokio = { workspace = true }
```

**ワークスペース変更:**

```toml
# Cargo.toml (workspace root)
members = [
    # Plugin SDK Layer
    "crates/orcs-types",
    "crates/orcs-event",
    "crates/orcs-auth",
    "crates/orcs-component",
    # Hook Layer (between SDK and Runtime)  ← NEW
    "crates/orcs-hook",
    # Runtime Layer
    "crates/orcs-runtime",
    # ...
]

[workspace.dependencies]
orcs-hook = { path = "crates/orcs-hook" }   # ← NEW
```

**完了条件:** `cargo check -p orcs-hook` が通る

---

### Step 1.2: `error.rs` — エラー型

```rust
// src/error.rs
use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum HookError {
    #[error("invalid FQL pattern: {0}")]
    InvalidFql(String),

    #[error("unknown hook point: {0}")]
    UnknownHookPoint(String),

    #[error("hook execution failed [{hook_id}]: {message}")]
    ExecutionFailed { hook_id: String, message: String },

    #[error("hook not found: {0}")]
    NotFound(String),

    #[error("depth limit exceeded (depth={depth}, max={max_depth})")]
    DepthExceeded { depth: u8, max_depth: u8 },
}
```

**完了条件:** コンパイル可

---

### Step 1.3: `point.rs` — HookPoint enum

ADR Section 4.1 準拠。`FromStr` / `Display` / `is_pre()` / `is_post()` を実装。

**テスト:**

- 全 26 variant の `FromStr` roundtrip (`"request.pre_dispatch"` → enum → `Display` → 同一文字列)
- `is_pre()` / `is_post()` が正しい variant で true を返す
- `AuthOnGrant`, `BusOnRegister`, `BusOnUnregister` は `is_pre()` も `is_post()` も false
- 不正文字列で `HookError::UnknownHookPoint` が返る

**完了条件:** 全テスト pass

---

### Step 1.4: `fql.rs` — FqlPattern パーサー & マッチャー

ADR Section 3 準拠。`ComponentId::fqn()` の `"namespace::name"` フォーマットに準拠。

**実装ポイント:**
- `PatternSegment` enum: `Exact(String)` / `Wildcard`
- パース: `"builtin::llm"` → scope=Exact("builtin"), target=Exact("llm")
- パース: `"builtin::llm/agent-1"` → + child_path=Exact("agent-1")
- パース: `"*::*"` → scope=Wildcard, target=Wildcard
- パース: `"builtin::llm/*"` → child_path=Wildcard
- `matches(&self, component_id: &ComponentId, child_id: Option<&str>) -> bool`
  - `ComponentId` の `.namespace()` / `.name()` で scope/target を比較
  - NOTE: `ComponentId` には現在 `namespace()` / `name()` public getter がない場合、
    `fqn()` を `"::"` で split して比較する（`fqn_eq()` を参考にする）

**テスト:**
- Exact match: `"builtin::llm"` matches `ComponentId::builtin("llm")`
- Wildcard scope: `"*::llm"` matches any namespace with name "llm"
- Wildcard target: `"builtin::*"` matches any builtin component
- Full wildcard: `"*::*"` matches everything
- Child path: `"builtin::llm/agent-1"` matches with `child_id=Some("agent-1")`
- Child wildcard: `"builtin::llm/*"` matches any child of llm
- No child match: `"builtin::llm/agent-1"` does NOT match `child_id=None`
- パースエラー: `""`, `":::"`, `"missing_sep"` → `HookError::InvalidFql`

**完了条件:** 全テスト pass + `cargo clippy` clean

---

### Step 1.5: `context.rs` — HookContext

ADR Section 4.2 準拠。`depth` / `max_depth` フィールド含む。

**実装ポイント:**
- `HookContext::new()` — 全フィールド指定
- `HookContext::for_request()` — Request からの構築ヘルパー
- `HookContext::for_request_result()` — Request + Result からの構築ヘルパー
- `HookContext::for_child_spawn()` — ChildConfig からの構築ヘルパー
- `HookContext::for_tool()` — tool name + args からの構築ヘルパー
- `HookContext::for_tool_result()` — tool result からの構築ヘルパー
- `with_incremented_depth(&self) -> Self` — depth +1 した clone
- `is_depth_exceeded(&self) -> bool`
- デフォルト `max_depth = 4`

**テスト:**
- 各ヘルパーの正しい構築
- `depth` increment / exceeded check
- Serialize/Deserialize roundtrip

**完了条件:** 全テスト pass

---

### Step 1.6: `action.rs` — HookAction

ADR Section 4.3 準拠。**Default 実装なし**。

**テスト:**
- 各 variant の構築・パターンマッチ
- Serialize/Deserialize roundtrip

**完了条件:** コンパイル pass

---

### Step 1.7: `hook.rs` — Hook trait

ADR Section 4.4 準拠。

```rust
pub trait Hook: Send + Sync {
    fn id(&self) -> &str;
    fn fql_pattern(&self) -> &FqlPattern;
    fn hook_point(&self) -> HookPoint;
    fn priority(&self) -> i32 { 100 }
    fn execute(&self, ctx: HookContext) -> HookAction;
}
```

**テスト用 MockHook も作成** (`testing` module):

```rust
pub struct MockHook {
    pub id: String,
    pub fql: FqlPattern,
    pub point: HookPoint,
    pub priority: i32,
    pub action: HookAction,  // execute() で固定値を返す
}
```

**完了条件:** trait + MockHook コンパイル pass

---

### Step 1.8: `registry.rs` — HookRegistry

ADR Section 5.2 準拠。最重要コンポーネント。

**実装ポイント:**
- `register()` — Vec に追加後、priority でソート
- `register_owned()` — owner 付き登録
- `unregister()` / `unregister_by_owner()`
- `set_enabled()`
- `dispatch()` — チェーン実行:
  - FQL match フィルタ
  - depth check (ADR Section 16.5)
  - Pre-hook: Skip/Abort → chain stop
  - Post-hook: Replace → payload 差替え + チェーン継続
  - enabled=false → skip

**テスト（網羅的）:**
- `dispatch` with 0 hooks → `Continue`
- `dispatch` with 1 pass-through hook → `Continue` (ctx unchanged)
- `dispatch` with 1 modifying hook → `Continue` (ctx.payload changed)
- `dispatch` with skip hook → `Skip`
- `dispatch` with abort hook → `Abort`
- Priority ordering: priority=10 が priority=100 より先に実行される
- FQL filtering: 対象外の component に hook が発火しない
- enabled=false の hook がスキップされる
- depth exceeded → chain breaks with warn log
- `unregister_by_owner()` → owner の hook だけ除去
- Post-hook Replace → payload 差替え後にチェーン継続
- Pre-hook Replace → warn + 無視（Continue 扱い）
- 複数 hook のチェーン: hook A が payload を変更 → hook B が変更後の値を受け取る

**完了条件:** 全テスト pass

---

### Step 1.9: `config.rs` — HooksConfig / HookDef

ADR Section 6.2 準拠。TOML デシリアライゼーション対応。

**テスト:**
- TOML roundtrip
- `merge()`: 同一 ID は override、新規 ID は append
- `default_priority` = 100, `default_enabled` = true
- `script` と `handler_inline` の相互排他バリデーション（or 両方 None でもエラー）

**完了条件:** 全テスト pass

---

### Step 1.10: `lib.rs` — re-exports + crate doc

全 public types を re-export。crate-level doc にアーキテクチャ図を含む。

**完了条件:** `cargo doc --no-deps -p orcs-hook` が clean

---

### Phase 1 完了条件

```bash
cargo check -p orcs-hook
cargo test -p orcs-hook
cargo clippy -p orcs-hook -- -D warnings
cargo doc --no-deps -p orcs-hook
```

全 pass。既存 crate のテストに影響なし:

```bash
cargo test --workspace
```

---

## Phase 2: Runtime Integration

### Step 2.1: `OrcsConfig` に `hooks` フィールド追加

**対象ファイル:** `crates/orcs-runtime/src/config/types.rs`

**変更内容:**
- `OrcsConfig` に `pub hooks: HooksConfig` 追加 (serde default)
- `merge()` に `self.hooks.merge(&other.hooks)` 追加
- `orcs-runtime/Cargo.toml` に `orcs-hook = { workspace = true }` 依存追加

**テスト:**
- 既存 TOML テストが pass するまま（hooks 未指定 = default = 空リスト）
- `[[hooks]]` 入り TOML のパース
- merge テスト（hooks accumulate）

---

### Step 2.2: `ChannelRunnerBuilder` に `.with_hook_registry()` 追加

**対象ファイル:** `crates/orcs-runtime/src/channel/runner/base.rs`

**変更内容:**
- `ChannelRunnerBuilder` に `hook_registry: Option<Arc<std::sync::RwLock<HookRegistry>>>` フィールド追加
- `.with_hook_registry()` builder メソッド追加
- `build()` で `ChannelRunner` にフィールドを伝播

**完了条件:** 既存テスト全 pass（hook_registry=None のまま）

---

### Step 2.3: Request hook 注入 (`process_event`, `handle_rpc_request`)

**対象ファイル:** `crates/orcs-runtime/src/channel/runner/base.rs`

**変更内容:**
- `process_event()` の `component.lock().on_request()` の前後に hook dispatch 挿入
- `handle_rpc_request()` にも同様
- ADR Section 8.2 のパターン準拠

**テスト:**
- MockHook で `RequestPreDispatch` が発火確認
- Pre-hook Abort → `on_request()` が呼ばれない
- Post-hook で result が観測できる

---

### Step 2.4: Signal hook 注入

**対象ファイル:** `crates/orcs-runtime/src/channel/runner/common.rs`, `base.rs`

**変更内容:**
- `handle_signal()` 内の `dispatch_signal_to_component()` 前後に hook dispatch

---

### Step 2.5: Component init/shutdown hook 注入

**対象ファイル:** `crates/orcs-runtime/src/channel/runner/base.rs`

**変更内容:**
- `run()` 内の `component.init()` 前後
- `run()` 内の `component.shutdown()` 前後
- shutdown 時に `registry.unregister_by_owner(comp_id)` 呼出し (ADR 16.3)

---

### Step 2.6: Child spawn/run hook 注入

**対象ファイル:** `crates/orcs-runtime/src/channel/runner/child_context.rs`, `child_spawner.rs`

**変更内容:**
- `ChildContextImpl` に `hook_registry` フィールド追加
- `spawn_child()` に pre/post spawn hook
- `ChildSpawner::run_child()` に pre/post run hook
- 子 `ChildContextImpl` 生成時に hook_registry を伝播

---

### Step 2.7: Engine-level channel hook

**対象ファイル:** `crates/orcs-runtime/src/engine/engine.rs`

**変更内容:**
- `OrcsEngine` に `hook_registry` 保持
- `spawn_runner*()` 各 variant の前後に hook dispatch
- `shutdown_parallel()` の後に post_destroy hook
- builder 経由で `ChannelRunnerBuilder` に hook_registry を渡す

---

### Phase 2 完了条件

```bash
cargo test --workspace
```

全 pass。hook_registry=None の場合の動作が既存と同一（Zero-cost when unused）。

---

## Phase 3: Lua Integration

### Step 3.1: `orcs.hook()` 関数登録

**対象ファイル:** `crates/orcs-lua/src/orcs_helpers.rs` (or new `hook_helpers.rs`)

**変更内容:**
- Lua `orcs.hook(descriptor, handler)` 関数を登録
- Shorthand パース: `"builtin::llm:request.pre_dispatch"` を FQL + HookPoint に分離
  - ADR 16.2 に記載の方式で実装（B案推奨: known prefix マッチ）
- Table 形式: `{ fql, point, handler, priority?, id? }`
- `LuaHook` struct 実装（Hook trait for Lua Function）

### Step 3.2: HookContext ↔ Lua table 変換

**変更内容:**
- `HookContext → mlua::Table` 変換 (`to_lua_table()`)
- `mlua::Table / mlua::Value → HookAction` 変換 (`parse_hook_return()`)
  - `return ctx` → `Continue(ctx)`
  - `return { action = "continue", ctx = ctx }` → `Continue(ctx)`
  - `return { action = "skip", result = ... }` → `Skip(value)`
  - `return { action = "abort", reason = ... }` → `Abort { reason }`
  - `return { action = "replace", result = ... }` → `Replace(value)`

### Step 3.3: TOML hook 自動ロード

**変更内容:**
- `OrcsEngine` 起動時に `HooksConfig.hooks` を iterate
- `script` 指定: Lua ファイルをロード → `{ id, fql, point, handler }` table として評価
- `handler_inline` 指定: インライン Lua を `function(ctx) ... end` として評価
- 各 hook を `HookRegistry::register()` で登録

### Step 3.4: Hook の LuaComponent / LuaChild からのアクセス

**変更内容:**
- `LuaComponent::set_child_context()` 経由で hook_registry を Lua VM の upvalue に保持
- `orcs.hook()` 呼出し時に `register_owned(hook, component_id)` で owner 付き登録

---

### Phase 3 完了条件

```bash
cargo test --workspace
```

Lua integration test:
- `orcs.hook("*::*:request.pre_dispatch", fn)` が登録され、request 時に発火
- hook が payload を変更 → component が変更後の値を受け取る
- TOML `[[hooks]]` で `script` 指定の hook がロード・発火

---

## Phase 4: Tool Hooks + Advanced

### Step 4.1: `tool_with_hooks()` wrapper

**対象ファイル:** `crates/orcs-lua/src/tools.rs`

**変更内容:**
- ADR Section 8.4 の `tool_with_hooks()` generic wrapper 実装
- 各 tool 関数 (`tool_read`, `tool_write`, `tool_grep`, `tool_glob`, `tool_exec` 等) を
  hook wrapper 経由に変更

### Step 4.2: Auth hooks

**対象ファイル:** `crates/orcs-runtime/src/channel/runner/child_context.rs`

**変更内容:**
- `check_command()` に `AuthPreCheck` / `AuthPostCheck` hook
- `grant_command()` に `AuthOnGrant` hook

### Step 4.3: EventBus hooks

**対象ファイル:** `crates/orcs-runtime/src/engine/eventbus.rs`

**変更内容:**
- `broadcast()` / `broadcast_async()` に `BusPreBroadcast` / `BusPostBroadcast`
- `register()` に `BusOnRegister`
- `unregister()` に `BusOnUnregister`

### Step 4.4: `orcs.unhook()` / hot-reload

**変更内容:**
- `orcs.unhook(id)` → `registry.unregister(id)`
- スクリプト変更検知 → reload (optional, notify crate)

---

### Phase 4 完了条件

全 hook point が動作。Builtin hook 実装例（ADR Section 11）が実際に動作確認済み。

---

## 依存関係グラフ（実装順序）

```
Step 1.1 (scaffold)
  ↓
Step 1.2-1.6 (types: error, point, fql, context, action)  ← 並行可
  ↓
Step 1.7 (hook trait)  ← 1.2-1.6 に依存
  ↓
Step 1.8 (registry)  ← 全 types に依存
  ↓
Step 1.9 (config)  ← registry, hook trait に依存
  ↓
Step 1.10 (lib.rs)
  ↓
━━━ Phase 1 完了 ━━━
  ↓
Step 2.1 (OrcsConfig)
  ↓
Step 2.2 (ChannelRunnerBuilder)
  ↓
Step 2.3-2.6 (injection points)  ← 2.2 に依存、互いに並行可
  ↓
Step 2.7 (Engine-level)  ← 2.2 に依存
  ↓
━━━ Phase 2 完了 ━━━
  ↓
Step 3.1-3.4 (Lua)  ← Phase 2 に依存
  ↓
━━━ Phase 3 完了 ━━━
  ↓
Step 4.1-4.4 (Advanced)  ← Phase 3 に依存
  ↓
━━━ Phase 4 完了 ━━━
```

---

## リスク & 対策

| リスク | 影響 | 対策 |
|--------|------|------|
| HookContext の Clone コスト | 高頻度 dispatch で性能劣化 | Phase 1 で benchmark 追加。Phase 4 で `Cow` 検討 (ADR 16.5) |
| Lua hook error がランタイムを crash | 全系停止 | Phase 1 暫定: catch_unwind + warn log (ADR 16.1) |
| FQL shorthand の `:` 曖昧性 | パースエラー | B案（known prefix match）で実装 (ADR 16.2) |
| 二重登録（Component 再起動時） | hook 重複実行 | `owner` + `unregister_by_owner` (ADR 16.3) |
| orcs-hook → orcs-component の循環依存 | コンパイル不可 | orcs-hook は types/event/auth のみ依存。component は hook に依存しない |
