//! Identifier types for ORCS.
//!
//! All identifiers are UUID-based for network compatibility and
//! cloud readiness.

use serde::{Deserialize, Serialize};
use uuid::{uuid, Uuid};

/// ORCS namespace UUID for deterministic UUID v5 generation.
///
/// This UUID is used as the namespace for generating deterministic
/// UUIDs for builtin components via UUID v5 (SHA-1 based).
const ORCS_NAMESPACE: Uuid = uuid!("d81843c2-9146-43ca-b016-bee57a14762f");

/// Identifier for a Component in the ORCS architecture.
///
/// A Component is a functional domain boundary that communicates
/// via the EventBus. Examples include:
///
/// - `builtin::llm` - LLM integration (chat, complete, embed)
/// - `builtin::tool` - Tool execution (read, write, bash)
/// - `builtin::hil` - Human-in-the-loop (approve, reject)
/// - `plugin::my-tool` - User-defined extensions
///
/// # UUID Strategy
///
/// - **Builtin components**: Use UUID v5 (deterministic from name)
/// - **Custom components**: Use UUID v4 (random)
///
/// This ensures builtin components have consistent UUIDs across
/// processes and machines, enabling reliable routing and comparison.
///
/// # Equality Semantics
///
/// `PartialEq` compares all fields including UUID. For FQN-only
/// comparison (ignoring UUID), use [`fqn_eq`](Self::fqn_eq).
///
/// # Example
///
/// ```
/// use orcs_types::ComponentId;
///
/// // Builtin: deterministic UUID
/// let llm1 = ComponentId::builtin("llm");
/// let llm2 = ComponentId::builtin("llm");
/// assert_eq!(llm1, llm2);  // Same UUID, same component
///
/// // Custom: random UUID per instance
/// let p1 = ComponentId::new("plugin", "tool");
/// let p2 = ComponentId::new("plugin", "tool");
/// assert_ne!(p1, p2);      // Different UUIDs
/// assert!(p1.fqn_eq(&p2)); // But same FQN
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ComponentId {
    /// Globally unique identifier.
    pub uuid: Uuid,
    /// Namespace (e.g., "builtin", "plugin", "wasm").
    pub namespace: String,
    /// Component name within namespace.
    pub name: String,
}

impl ComponentId {
    /// Creates a new [`ComponentId`] with a random UUID v4.
    ///
    /// Use this for custom/plugin components where each instance
    /// should have a unique identity.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::ComponentId;
    ///
    /// let plugin = ComponentId::new("plugin", "my-tool");
    /// assert_eq!(plugin.namespace, "plugin");
    /// assert_eq!(plugin.name, "my-tool");
    /// assert_eq!(plugin.fqn(), "plugin::my-tool");
    /// ```
    #[must_use]
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uuid: Uuid::new_v4(),
            namespace: namespace.into(),
            name: name.into(),
        }
    }

    /// Creates a builtin component ID with a deterministic UUID v5.
    ///
    /// The UUID is derived from the ORCS namespace UUID and the
    /// component name using SHA-1. This ensures:
    ///
    /// - Same name always produces same UUID
    /// - Different names produce different UUIDs
    /// - UUIDs are consistent across processes/machines
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::ComponentId;
    ///
    /// let llm1 = ComponentId::builtin("llm");
    /// let llm2 = ComponentId::builtin("llm");
    /// let tool = ComponentId::builtin("tool");
    ///
    /// assert_eq!(llm1.uuid, llm2.uuid);  // Same name = same UUID
    /// assert_ne!(llm1.uuid, tool.uuid);  // Different name = different UUID
    /// assert!(llm1.is_builtin());
    /// ```
    #[must_use]
    pub fn builtin(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            uuid: Uuid::new_v5(&ORCS_NAMESPACE, name.as_bytes()),
            namespace: "builtin".to_string(),
            name,
        }
    }

    /// Returns the fully qualified name in `namespace::name` format.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::ComponentId;
    ///
    /// let id = ComponentId::builtin("llm");
    /// assert_eq!(id.fqn(), "builtin::llm");
    /// ```
    #[must_use]
    pub fn fqn(&self) -> String {
        format!("{}::{}", self.namespace, self.name)
    }

    /// Compares two [`ComponentId`]s by FQN only, ignoring UUID.
    ///
    /// This is useful when you want to check if two components
    /// represent the same logical component, even if they were
    /// created as separate instances.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::ComponentId;
    ///
    /// let p1 = ComponentId::new("plugin", "tool");
    /// let p2 = ComponentId::new("plugin", "tool");
    ///
    /// assert_ne!(p1, p2);       // Different UUIDs
    /// assert!(p1.fqn_eq(&p2));  // Same FQN
    /// ```
    #[must_use]
    pub fn fqn_eq(&self, other: &Self) -> bool {
        self.namespace == other.namespace && self.name == other.name
    }

    /// Checks if this component matches the given namespace and name.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::ComponentId;
    ///
    /// let llm = ComponentId::builtin("llm");
    ///
    /// assert!(llm.matches("builtin", "llm"));
    /// assert!(!llm.matches("builtin", "tool"));
    /// ```
    #[must_use]
    pub fn matches(&self, namespace: &str, name: &str) -> bool {
        self.namespace == namespace && self.name == name
    }

    /// Returns `true` if this is a builtin component.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::ComponentId;
    ///
    /// let builtin = ComponentId::builtin("llm");
    /// let plugin = ComponentId::new("plugin", "tool");
    ///
    /// assert!(builtin.is_builtin());
    /// assert!(!plugin.is_builtin());
    /// ```
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        self.namespace == "builtin"
    }

    /// Creates a child component ID with a deterministic UUID v5.
    ///
    /// The UUID is derived from the ORCS namespace UUID and the
    /// child name using SHA-1.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::ComponentId;
    ///
    /// let child = ComponentId::child("worker-1");
    /// assert_eq!(child.namespace, "child");
    /// assert_eq!(child.name, "worker-1");
    /// ```
    #[must_use]
    pub fn child(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            uuid: Uuid::new_v5(&ORCS_NAMESPACE, format!("child:{}", name).as_bytes()),
            namespace: "child".to_string(),
            name,
        }
    }

    /// Returns `true` if this is a child component.
    #[must_use]
    pub fn is_child(&self) -> bool {
        self.namespace == "child"
    }
}

impl std::fmt::Display for ComponentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}::{}@{}", self.namespace, self.name, self.uuid)
    }
}

/// Identifier for a Principal (actor) in the ORCS system.
///
/// A Principal represents "who" is performing an action, separate from
/// "what permission they have". This separation enables:
///
/// - **Dynamic privilege**: Same principal can be Standard or Elevated
/// - **Audit trails**: Actions are attributed to identifiable actors
/// - **Network identity**: Principals are unique across distributed systems
///
/// # Principal vs Component
///
/// - [`PrincipalId`]: Who is acting (human user, system process)
/// - [`ComponentId`]: What is acting (LLM, Tool, Plugin)
///
/// A Component may act on behalf of a Principal, but they are distinct
/// concepts. A human user (Principal) may trigger an LLM Component,
/// but the LLM Component itself is not the Principal.
///
/// # Example
///
/// ```
/// use orcs_types::PrincipalId;
///
/// let user = PrincipalId::new();
/// let admin = PrincipalId::new();
///
/// assert_ne!(user, admin);  // Different principals
/// println!("User: {}", user);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PrincipalId(pub Uuid);

impl PrincipalId {
    /// Creates a new [`PrincipalId`] with a random UUID v4.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::PrincipalId;
    ///
    /// let principal = PrincipalId::new();
    /// println!("Principal: {}", principal);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the inner UUID.
    #[must_use]
    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for PrincipalId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for PrincipalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "principal:{}", self.0)
    }
}

/// Identifier for a Channel in the ORCS architecture.
///
/// A Channel represents a parallel execution context managed by the
/// World. Channels form a tree structure where:
///
/// - **Primary Channel**: Root channel owned by Human
/// - **Agent Channels**: Spawned for LLM/Tool execution
/// - **Background Channels**: Isolated tasks (WASM plugins, etc.)
///
/// # Channel Tree
///
/// ```text
/// Primary (Human)
///   ├── Agent Channel
///   │     ├── LLM Component
///   │     └── Tool Component
///   │           └── Subprocess
///   └── Background Channel
///         └── WASM Plugin
/// ```
///
/// # Permission Inheritance
///
/// Child channels inherit permissions from their parent.
/// Children cannot have broader scope than their parent.
///
/// # Example
///
/// ```
/// use orcs_types::ChannelId;
///
/// let primary = ChannelId::new();
/// let agent = ChannelId::new();
///
/// assert_ne!(primary, agent);  // Each channel is unique
/// println!("Agent channel: {}", agent);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelId(pub Uuid);

#[allow(clippy::new_without_default)] // Default intentionally not implemented - see below
impl ChannelId {
    /// Creates a new [`ChannelId`] with a random UUID v4.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::ChannelId;
    ///
    /// let ch = ChannelId::new();
    /// println!("Created channel: {}", ch);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the inner UUID.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::ChannelId;
    ///
    /// let ch = ChannelId::new();
    /// let uuid = ch.uuid();
    /// assert_eq!(ch.0, uuid);
    /// ```
    #[must_use]
    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

// NOTE: ChannelId intentionally does NOT implement Default.
// Default::default() would generate a new UUID that is not registered in World,
// leading to subtle bugs. Use World::create_primary() or World::spawn() instead.

impl std::fmt::Display for ChannelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ch:{}", self.0)
    }
}

/// Identifier for a Request in the ORCS EventBus.
///
/// A Request represents a synchronous query from one Component to
/// another (or broadcast). Each Request expects exactly one Response.
///
/// # Request/Response Pattern
///
/// ```text
/// ┌─────────────┐  Request   ┌─────────────┐
/// │   Source    │ ─────────► │   Target    │
/// │  Component  │            │  Component  │
/// │             │ ◄───────── │             │
/// └─────────────┘  Response  └─────────────┘
/// ```
///
/// # Example
///
/// ```
/// use orcs_types::RequestId;
///
/// let req_id = RequestId::new();
/// println!("Request: {}", req_id);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub Uuid);

#[allow(clippy::new_without_default)] // Default intentionally not implemented - RequestId is generated internally by Request::new()
impl RequestId {
    /// Creates a new [`RequestId`] with a random UUID v4.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::RequestId;
    ///
    /// let id = RequestId::new();
    /// println!("Request ID: {}", id);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the inner UUID.
    #[must_use]
    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

// NOTE: RequestId intentionally does NOT implement Default.
// RequestId is generated internally by Request::new()/try_new().
// Direct construction should use RequestId::new() explicitly.

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "req:{}", self.0)
    }
}

/// Identifier for an Event in the ORCS EventBus.
///
/// An Event represents an asynchronous broadcast notification.
/// Unlike Requests, Events do not expect a Response.
///
/// # Event Pattern
///
/// ```text
/// ┌─────────────┐   Event    ┌─────────────┐
/// │   Source    │ ─────────► │ Subscriber  │
/// │  Component  │            │  Component  │
/// └─────────────┘            └─────────────┘
///                   Event    ┌─────────────┐
///                 ─────────► │ Subscriber  │
///                            │  Component  │
///                            └─────────────┘
/// ```
///
/// # Example
///
/// ```
/// use orcs_types::EventId;
///
/// let evt_id = EventId::new();
/// println!("Event: {}", evt_id);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub Uuid);

impl EventId {
    /// Creates a new [`EventId`] with a random UUID v4.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_types::EventId;
    ///
    /// let id = EventId::new();
    /// println!("Event ID: {}", id);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Returns the inner UUID.
    #[must_use]
    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "evt:{}", self.0)
    }
}

// Tests are in lib.rs as integration tests for public API
