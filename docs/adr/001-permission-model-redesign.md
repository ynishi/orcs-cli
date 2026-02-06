# ADR-001: Permission Model Redesign

**Status**: Draft
**Date**: 2026-02-06

---

## 1. Background

### 1.1 Current State

現在の権限モデルは2層に分かれているが、統合が不完全:

| 層 | 型 | 役割 | 適用範囲 |
|---|---|---|---|
| **物理境界** | `SandboxPolicy` (trait) | ファイルシステムのパス境界 | file tools のみ |
| **論理権限** | `ChildContext` → `Session` + `PermissionChecker` | コマンド実行・子プロセス生成の認可 | exec, spawn のみ |

### 1.2 Problem

**ファイル操作ツールに論理権限チェックが無い。**

| Lua 関数 | Sandbox | Auth | 問題 |
|---|---|---|---|
| `orcs.read(path)` | validate_read | **なし** | Read権限のないChildでも読める |
| `orcs.write(path, content)` | validate_write | **なし** | Write権限のないChildでも書ける |
| `orcs.grep(pattern, path)` | validate_read | **なし** | 同上 |
| `orcs.glob(pattern, dir?)` | validate_read | **なし** | 同上 |
| `orcs.mkdir(path)` | validate_write | **なし** | 同上 |
| `orcs.remove(path)` | validate_write + validate_read | **なし** | 破壊的操作が無認可 |
| `orcs.mv(src, dst)` | validate_read + validate_write | **なし** | 同上 |
| `orcs.exec(cmd)` | なし | **check_command_permission** | H2で修正済み（deny-by-default） |
| `orcs.llm(prompt)` | cwd=sandbox_root | **なし** | LLM呼び出しが無認可 |
| `orcs.spawn_child` | なし | **can_spawn_child_auth** | OK |
| `orcs.spawn_runner` | なし | **can_spawn_runner_auth** | OK |

**その他の問題:**
- `orcs.exec` の permission-checked 版が `component.rs` と `child.rs` で重複実装
- `orcs.exec` が cwd を sandbox root に設定していない
- `orcs.send_to_child` に認可チェックなし
- Child の `register_context_functions` が Component より機能不足（check_command, grant_command, request_approval 未登録）

---

## 2. Design Goal

### 2.1 Permission = SandboxPolicy ∩ Capability（Deny wins）

```
有効権限 = SandboxPolicy（物理境界） ∩ Capability（論理権限）
         = Where × What

Where: SandboxPolicy が「どこにアクセスできるか」を制御
What:  Capability が「何ができるか」を制御
```

### 2.2 ABAC Inheritance Model

```
Channel (Component)
├── permissions: {Read, Write, Execute, Spawn}
│
├── Child-A (via spawn_child)
│   └── permissions: {Read, Write}        ← 親のサブセット
│
└── SubAgent-B (via spawn_runner)
    └── permissions: {Read}               ← さらに縮小
        └── Child-B-1
            └── permissions: {Read}       ← 親から継承（拡大不可）
```

**不変条件: 子は親より広い権限を持てない。**

---

## 3. Proposed Design

### 3.1 Capability 型（orcs-component crate）

```rust
bitflags::bitflags! {
    /// Logical capabilities that a context can grant.
    ///
    /// Checked AFTER SandboxPolicy boundary validation.
    /// Effective permission = Sandbox ∩ Capability.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Capability: u16 {
        /// Read files (orcs.read, orcs.grep, orcs.glob)
        const READ    = 0b0000_0001;
        /// Write files (orcs.write, orcs.mkdir)
        const WRITE   = 0b0000_0010;
        /// Delete/move files (orcs.remove, orcs.mv)
        const DELETE  = 0b0000_0100;
        /// Execute commands (orcs.exec)
        const EXECUTE = 0b0000_1000;
        /// Spawn children/runners (orcs.spawn_child, orcs.spawn_runner)
        const SPAWN   = 0b0001_0000;
        /// Call LLM (orcs.llm)
        const LLM     = 0b0010_0000;

        /// Convenience: all file operations
        const FILE_ALL = Self::READ.bits() | Self::WRITE.bits() | Self::DELETE.bits();
        /// Convenience: full access
        const ALL      = Self::READ.bits() | Self::WRITE.bits() | Self::DELETE.bits()
                       | Self::EXECUTE.bits() | Self::SPAWN.bits() | Self::LLM.bits();
    }
}
```

### 3.2 ChildContext に Capability を追加

```rust
pub trait ChildContext: Send + Sync + Debug {
    // --- 既存メソッド（変更なし） ---
    fn parent_id(&self) -> &str;
    fn emit_output(&self, message: &str);
    fn emit_output_with_level(&self, message: &str, level: &str);
    // ... (spawn_child, child_count, etc.)

    // --- 新規: Capability ベースの権限チェック ---

    /// このコンテキストが持つ Capability set を返す。
    /// デフォルト: ALL（後方互換）
    fn capabilities(&self) -> Capability {
        Capability::ALL
    }

    /// 特定の Capability を持つか。
    fn has_capability(&self, cap: Capability) -> bool {
        self.capabilities().contains(cap)
    }

    // --- 既存の exec 固有メソッド（維持） ---
    fn check_command_permission(&self, _cmd: &str) -> CommandPermission {
        CommandPermission::Allowed
    }
    fn grant_command(&self, _pattern: &str) {}
}
```

### 3.3 ChildContextImpl への組み込み

```rust
pub struct ChildContextImpl {
    // ... 既存フィールド ...
    session: Option<Arc<Session>>,
    checker: Option<Arc<dyn PermissionChecker>>,

    // 新規
    capabilities: Capability,
}

impl ChildContextImpl {
    pub fn new(parent_id: &str, output_tx: Sender<Event>, spawner: Arc<Mutex<ChildSpawner>>) -> Self {
        Self {
            // ...
            capabilities: Capability::ALL,  // デフォルトは全権限
        }
    }

    /// Capability を制限するビルダーメソッド
    pub fn with_capabilities(mut self, caps: Capability) -> Self {
        self.capabilities = caps;
        self
    }
}

impl ChildContext for ChildContextImpl {
    fn capabilities(&self) -> Capability {
        self.capabilities
    }

    fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
        // SPAWN capability check
        if !self.capabilities.contains(Capability::SPAWN) {
            return Err(SpawnError::PermissionDenied("SPAWN capability required".into()));
        }
        // ... 既存ロジック ...
    }
}
```

### 3.4 Tool 関数への Capability チェック統合

**方針: `register_tool_functions` に ChildContext（Optional）を渡す。**

```rust
/// ファイルツール登録。
///
/// ChildContext がある場合は Capability チェックを追加。
/// ない場合はフォールバック動作（deny-by-default or sandbox-only）を選択。
pub fn register_tool_functions(
    lua: &Lua,
    sandbox: Arc<dyn SandboxPolicy>,
    ctx: Option<Arc<Mutex<Box<dyn ChildContext>>>>,  // 新規パラメータ
) -> Result<(), LuaError> { ... }
```

各ツール関数内:

```rust
// orcs.read の例
fn tool_read_with_cap(
    path: &str,
    sandbox: &dyn SandboxPolicy,
    ctx: Option<&ContextWrapper>,
) -> Result<ReadResult, String> {
    // 1. Capability check（論理権限）
    if let Some(wrapper) = ctx {
        let guard = wrapper.0.lock().map_err(|e| e.to_string())?;
        if !guard.has_capability(Capability::READ) {
            return Err("permission denied: READ capability required".into());
        }
    }
    // 2. Sandbox check（物理境界）
    let canonical = sandbox.validate_read(path).map_err(|e| e.to_string())?;
    // 3. 実行
    let content = std::fs::read_to_string(&canonical).map_err(|e| e.to_string())?;
    Ok(ReadResult { content, size: ... })
}
```

**ツールと必要な Capability のマッピング:**

| ツール | 必要な Capability |
|---|---|
| `orcs.read` | `READ` |
| `orcs.grep` | `READ` |
| `orcs.glob` | `READ` |
| `orcs.write` | `WRITE` |
| `orcs.mkdir` | `WRITE` |
| `orcs.remove` | `DELETE` |
| `orcs.mv` | `READ` + `DELETE`（src）+ `WRITE`（dst） |
| `orcs.exec` | `EXECUTE` + check_command_permission |
| `orcs.llm` | `LLM` |
| `orcs.spawn_child` | `SPAWN` |
| `orcs.spawn_runner` | `SPAWN` |

### 3.5 Capability の継承

`spawn_child` 時に子の Capability を指定可能にする:

```lua
-- Lua 側の API
orcs.spawn_child({
    id = "researcher",
    script = "...",
    capabilities = {"READ"},  -- 親の Capability のサブセット
})
```

```rust
// ChildConfig に capabilities フィールド追加
pub struct ChildConfig {
    pub id: String,
    pub script: Option<String>,
    pub path: Option<String>,
    pub capabilities: Option<Capability>,  // None = 親と同じ
}
```

`ChildContextImpl::spawn_child` での処理:

```rust
fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
    // 1. 自身が SPAWN を持つか
    if !self.capabilities.contains(Capability::SPAWN) {
        return Err(SpawnError::PermissionDenied("SPAWN capability required".into()));
    }

    // 2. 子の Capability を計算: 要求 ∩ 自身（拡大不可）
    let child_caps = match config.capabilities {
        Some(requested) => self.capabilities & requested,  // intersection
        None => self.capabilities,                          // inherit all
    };

    // 3. 子の ChildContext を child_caps で生成
    // ...
}
```

### 3.6 exec 統合: 重複排除

現状 `component.rs` と `child.rs` に同じ exec 実装が重複。統合する:

```rust
// crates/orcs-lua/src/exec.rs（新規モジュール）

/// Permission-checked exec を orcs テーブルに登録する。
///
/// ContextWrapper 経由で ChildContext の check_command_permission を使用。
/// sandbox_root を cwd として設定。
pub fn register_exec_function(
    lua: &Lua,
    sandbox_root: PathBuf,
) -> Result<(), mlua::Error> {
    let orcs_table: Table = lua.globals().get("orcs")?;

    let exec_fn = lua.create_function(move |lua, cmd: String| {
        // 1. Capability check
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError(
                "exec denied: no execution context".into()
            ))?;
        let ctx = wrapper.0.lock()
            .map_err(|e| mlua::Error::RuntimeError(format!("context lock: {e}")))?;

        if !ctx.has_capability(Capability::EXECUTE) {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("stderr", "permission denied: EXECUTE capability required")?;
            result.set("code", -1)?;
            return Ok(result);
        }

        // 2. Command permission check (BLOCKED_PATTERNS, elevation, grants)
        match ctx.check_command_permission(&cmd) {
            CommandPermission::Allowed => {}
            CommandPermission::Denied(reason) => { /* return deny table */ }
            CommandPermission::RequiresApproval { .. } => { /* return deny table */ }
        }

        // 3. Execute with sandbox cwd
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&sandbox_root)  // 常に sandbox root
            .output()
            .map_err(|e| mlua::Error::ExternalError(Arc::new(e)))?;

        // 4. Build result table
        // ...
    })?;

    orcs_table.set("exec", exec_fn)?;
    Ok(())
}
```

---

## 4. Migration Strategy

### Phase 1: 型導入（非破壊的）

1. `Capability` bitflags を `orcs-component` に追加
2. `ChildContext` に `capabilities()` / `has_capability()` をデフォルト実装で追加
3. `ChildContextImpl` に `capabilities` フィールド追加（デフォルト `ALL`）
4. `ChildConfig` に `capabilities` フィールド追加（`Option<Capability>`）

**この時点では動作は一切変わらない。** 全ての既存コードは `Capability::ALL` で動く。

### Phase 2: ツール関数への Capability チェック追加

1. `register_tool_functions` のシグネチャ変更（ctx パラメータ追加）
2. 各ツール関数（read/write/grep/glob/mkdir/remove/mv）に Capability チェック追加
3. `exec.rs` モジュール新設、component.rs / child.rs の重複排除
4. `orcs.llm` に LLM Capability チェック追加

### Phase 3: spawn 時の Capability 継承

1. `spawn_child` に Capability 指定 API 追加
2. `spawn_runner` にも同様の Capability 伝播
3. Lua 側 API: `capabilities` テーブルフィールド

### Phase 4: テスト・ドキュメント

1. Capability 単体テスト（bitflags 操作）
2. 各ツールの deny テスト（Capability なし → deny）
3. 継承テスト（子 ∩ 親 = 子の実効 Capability）
4. E2E: Read-only SubAgent が Write を試みて拒否されるシナリオ

---

## 5. Impact Analysis

### 変更が必要な crate

| Crate | 変更内容 |
|---|---|
| `orcs-component` | `Capability` bitflags 追加、`ChildContext` trait 拡張、`ChildConfig` 拡張 |
| `orcs-runtime` | `ChildContextImpl` に capabilities、auth モジュールへの統合 |
| `orcs-lua` | `tools.rs` に Capability チェック、`exec.rs` 新設、`component.rs` / `child.rs` 簡素化 |

### 後方互換性

- `Capability::ALL` デフォルトにより、**既存コードは変更なしで動作**
- `ChildContext` の新メソッドはデフォルト実装あり
- Phase 1 は完全に非破壊的

### リスク

| リスク | 対策 |
|---|---|
| bitflags 依存追加 | `orcs-component` の Cargo.toml に bitflags 追加（軽量、std-only） |
| ツール登録関数のシグネチャ変更 | 呼び出し元は限定的（3箇所） |
| exec 重複排除で behavior change | テストで検証 |

---

## 6. Open Questions

1. **orcs.llm は EXECUTE に含めるか、別の LLM Capability にするか？**
   - 提案: 別の `LLM` Capability。exec とは性質が異なる（コスト、副作用の種類）

2. **Capability の粒度は十分か？**
   - READ/WRITE/DELETE/EXECUTE/SPAWN/LLM の6種。
   - 将来的に NETWORK 等を追加する余地あり（bitflags なので拡張容易）

3. **ChildContext なしの場合のデフォルト動作は？**
   - 現状: ファイルツールは sandbox-only で許可、exec は deny
   - 提案: ChildContext なし → 全 Capability deny（完全 deny-by-default）
   - 代替: ChildContext なし → READ のみ許可（最小権限）

4. **exec の cwd を常に sandbox root にするか？**
   - 提案: Yes。現状は設定なし（プロセスの cwd 依存）で不安定

---

## Appendix: Current Permission Flow

```
ChannelRunnerBuilder::build()
  │
  ├─ Creates ChildContextImpl {
  │    capabilities: ALL,  // Phase 1 で追加
  │    session: Arc<Session>,
  │    checker: Arc<dyn PermissionChecker>,
  │    ...
  │  }
  │
  ├─ component.set_child_context(ctx)
  │    │
  │    ├─ register_child_context_functions(lua, ctx_arc)
  │    │    ├─ orcs.exec      → Capability::EXECUTE + check_command_permission
  │    │    ├─ orcs.spawn_*   → Capability::SPAWN + can_spawn_*_auth
  │    │    └─ orcs.check/grant_command
  │    │
  │    └─ register_tool_functions(lua, sandbox, Some(ctx_arc))  // Phase 2
  │         ├─ orcs.read   → Capability::READ  + validate_read
  │         ├─ orcs.write  → Capability::WRITE + validate_write
  │         ├─ orcs.remove → Capability::DELETE + validate_write + validate_read
  │         └─ ...
  │
  └─ On spawn_child(config { capabilities: Some(READ) }):
       │
       └─ child_caps = parent.capabilities & requested = ALL & READ = READ
          └─ child.set_context(ctx_with_caps(READ))
               └─ register_context_functions(lua, ctx)
                    ├─ orcs.exec   → EXECUTE not in READ → deny
                    ├─ orcs.read   → READ in READ → sandbox check → allow
                    └─ orcs.write  → WRITE not in READ → deny
```
