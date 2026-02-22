//! Event categories for subscription-based routing.
//!
//! Components subscribe to specific categories and only receive
//! requests/events for those categories.
//!
//! # Built-in Categories
//!
//! | Category | Purpose | Example Operations |
//! |----------|---------|-------------------|
//! | `Lifecycle` | System events | init, shutdown |
//! | `Hil` | Human-in-the-Loop | approve, reject, submit |
//! | `Echo` | Echo (example) | echo |
//!
//! # Subscription Flow
//!
//! ```text
//! Component::subscriptions() -> [Hil, Echo]
//!     │
//!     ▼
//! EventBus::register(component, categories)
//!     │
//!     ▼
//! Request { category: Hil, operation: "submit" }
//!     │
//!     ▼ (routed only to Hil subscribers)
//! HilComponent::on_request()
//! ```
//!
//! # Extension Categories
//!
//! For custom plugins, use `Extension`:
//!
//! ```
//! use orcs_event::EventCategory;
//!
//! let custom = EventCategory::Extension {
//!     namespace: "my-plugin".into(),
//!     kind: "data".into(),
//! };
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A subscription entry with optional operation-level filtering.
///
/// Wraps an [`EventCategory`] with an optional set of accepted operations.
/// This enables fine-grained subscription control: components can subscribe
/// to a category and only receive events for specific operations.
///
/// # Examples
///
/// ```
/// use orcs_event::{EventCategory, SubscriptionEntry};
///
/// // Accept all operations for Echo category
/// let entry = SubscriptionEntry::all(EventCategory::Echo);
/// assert!(entry.matches(&EventCategory::Echo, "any_op"));
///
/// // Accept only specific operations for Extension category
/// let ext = EventCategory::extension("lua", "Extension");
/// let entry = SubscriptionEntry::with_operations(
///     ext.clone(),
///     ["route_response".to_string()],
/// );
/// assert!(entry.matches(&ext, "route_response"));
/// assert!(!entry.matches(&ext, "llm_response"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionEntry {
    /// The event category to subscribe to.
    pub category: EventCategory,
    /// Optional set of accepted operations within this category.
    /// `None` means all operations are accepted (wildcard).
    pub operations: Option<HashSet<String>>,
}

impl SubscriptionEntry {
    /// Create a subscription accepting all operations for a category.
    #[must_use]
    pub fn all(category: EventCategory) -> Self {
        Self {
            category,
            operations: None,
        }
    }

    /// Create a subscription accepting specific operations only.
    #[must_use]
    pub fn with_operations(
        category: EventCategory,
        operations: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            category,
            operations: Some(operations.into_iter().collect()),
        }
    }

    /// Check if an event matches this subscription entry.
    ///
    /// Returns `true` if:
    /// - The category matches, AND
    /// - Either `operations` is `None` (wildcard), or the operation is in the set.
    #[must_use]
    pub fn matches(&self, category: &EventCategory, operation: &str) -> bool {
        if self.category != *category {
            return false;
        }
        match &self.operations {
            None => true,
            Some(ops) => ops.contains(operation),
        }
    }

    /// Returns the category of this entry.
    #[must_use]
    pub fn category(&self) -> &EventCategory {
        &self.category
    }
}

/// Event category for subscription-based routing.
///
/// Components declare which categories they subscribe to.
/// The EventBus routes messages only to subscribers of the
/// matching category.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventCategory {
    /// System lifecycle events (init, shutdown, pause, resume).
    ///
    /// All components implicitly subscribe to this category.
    Lifecycle,

    /// Human-in-the-Loop approval requests.
    ///
    /// Operations: submit, status, list
    Hil,

    /// Echo component (example/test).
    ///
    /// Operations: echo, check
    Echo,

    /// User input from IOBridge.
    ///
    /// Components subscribe to this category to receive user messages
    /// from the interactive console or other input sources.
    ///
    /// Payload should contain:
    /// - `message`: The user's input text (String)
    UserInput,

    /// Output events for IO display.
    ///
    /// Components emit this category when they want to output
    /// results to the user via IOBridge.
    ///
    /// Payload should contain:
    /// - `message`: The message to display (String)
    /// - `level`: Optional log level ("info", "warn", "error")
    Output,

    /// Extension category for plugins.
    ///
    /// Use this for custom components that don't fit built-in categories.
    Extension {
        /// Plugin/component namespace (e.g., "my-plugin").
        namespace: String,
        /// Event kind within the namespace (e.g., "data", "notification").
        kind: String,
    },
}

impl EventCategory {
    /// Creates an Extension category.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_event::EventCategory;
    ///
    /// let cat = EventCategory::extension("my-plugin", "data");
    /// assert!(matches!(cat, EventCategory::Extension { .. }));
    /// ```
    #[must_use]
    pub fn extension(namespace: impl Into<String>, kind: impl Into<String>) -> Self {
        Self::Extension {
            namespace: namespace.into(),
            kind: kind.into(),
        }
    }

    /// Returns `true` if this is the Lifecycle category.
    #[must_use]
    pub fn is_lifecycle(&self) -> bool {
        matches!(self, Self::Lifecycle)
    }

    /// Returns `true` if this is the Hil category.
    #[must_use]
    pub fn is_hil(&self) -> bool {
        matches!(self, Self::Hil)
    }

    /// Returns `true` if this is an Extension category.
    #[must_use]
    pub fn is_extension(&self) -> bool {
        matches!(self, Self::Extension { .. })
    }

    /// Returns `true` if this is the Output category.
    #[must_use]
    pub fn is_output(&self) -> bool {
        matches!(self, Self::Output)
    }

    /// Returns `true` if this is the UserInput category.
    #[must_use]
    pub fn is_user_input(&self) -> bool {
        matches!(self, Self::UserInput)
    }

    /// Returns the display name of this category.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Lifecycle => "Lifecycle".to_string(),
            Self::Hil => "Hil".to_string(),
            Self::Echo => "Echo".to_string(),
            Self::UserInput => "UserInput".to_string(),
            Self::Output => "Output".to_string(),
            Self::Extension { namespace, kind } => format!("{}:{}", namespace, kind),
        }
    }
}

impl std::fmt::Display for EventCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_creation() {
        let lifecycle = EventCategory::Lifecycle;
        let hil = EventCategory::Hil;
        let echo = EventCategory::Echo;

        assert!(lifecycle.is_lifecycle());
        assert!(hil.is_hil());
        assert!(!echo.is_extension());
    }

    #[test]
    fn category_extension() {
        let ext = EventCategory::extension("my-plugin", "data");

        assert!(ext.is_extension());
        assert!(!ext.is_lifecycle());

        if let EventCategory::Extension { namespace, kind } = ext {
            assert_eq!(namespace, "my-plugin");
            assert_eq!(kind, "data");
        } else {
            panic!("Expected Extension");
        }
    }

    #[test]
    fn category_display() {
        assert_eq!(EventCategory::Lifecycle.to_string(), "Lifecycle");
        assert_eq!(EventCategory::Hil.to_string(), "Hil");
        assert_eq!(EventCategory::Echo.to_string(), "Echo");
        assert_eq!(
            EventCategory::extension("plugin", "event").to_string(),
            "plugin:event"
        );
    }

    #[test]
    fn category_name() {
        assert_eq!(EventCategory::Lifecycle.name(), "Lifecycle");
        assert_eq!(EventCategory::extension("ns", "k").name(), "ns:k");
    }

    #[test]
    fn category_equality() {
        assert_eq!(EventCategory::Hil, EventCategory::Hil);
        assert_ne!(EventCategory::Hil, EventCategory::Echo);

        let ext1 = EventCategory::extension("a", "b");
        let ext2 = EventCategory::extension("a", "b");
        let ext3 = EventCategory::extension("a", "c");

        assert_eq!(ext1, ext2);
        assert_ne!(ext1, ext3);
    }

    #[test]
    fn category_hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(EventCategory::Hil);
        set.insert(EventCategory::Echo);
        set.insert(EventCategory::Hil); // Duplicate

        assert_eq!(set.len(), 2);
        assert!(set.contains(&EventCategory::Hil));
        assert!(set.contains(&EventCategory::Echo));
    }

    #[test]
    fn category_serialize() {
        let hil = EventCategory::Hil;
        let json =
            serde_json::to_string(&hil).expect("EventCategory::Hil should serialize to JSON");
        assert!(json.contains("Hil"));

        let ext = EventCategory::extension("ns", "kind");
        let json =
            serde_json::to_string(&ext).expect("EventCategory::Extension should serialize to JSON");
        assert!(json.contains("Extension"));
        assert!(json.contains("ns"));
        assert!(json.contains("kind"));
    }

    #[test]
    fn category_deserialize() {
        let json = r#""Hil""#;
        let cat: EventCategory = serde_json::from_str(json)
            .expect("'Hil' JSON string should deserialize to EventCategory::Hil");
        assert_eq!(cat, EventCategory::Hil);

        let json = r#"{"Extension":{"namespace":"ns","kind":"k"}}"#;
        let cat: EventCategory = serde_json::from_str(json)
            .expect("Extension JSON should deserialize to EventCategory::Extension");
        assert_eq!(cat, EventCategory::extension("ns", "k"));
    }

    // === SubscriptionEntry tests ===

    #[test]
    fn subscription_entry_all_matches_any_operation() {
        let entry = SubscriptionEntry::all(EventCategory::Echo);
        assert!(entry.matches(&EventCategory::Echo, "echo"));
        assert!(entry.matches(&EventCategory::Echo, "anything"));
        assert!(!entry.matches(&EventCategory::Hil, "echo"));
    }

    #[test]
    fn subscription_entry_with_operations_filters() {
        let ext = EventCategory::extension("lua", "Extension");
        let entry = SubscriptionEntry::with_operations(
            ext.clone(),
            ["route_response".to_string(), "skill_update".to_string()],
        );
        assert!(entry.matches(&ext, "route_response"));
        assert!(entry.matches(&ext, "skill_update"));
        assert!(!entry.matches(&ext, "llm_response"));
        assert!(!entry.matches(&EventCategory::Echo, "route_response"));
    }

    #[test]
    fn subscription_entry_empty_operations_rejects_all() {
        let entry =
            SubscriptionEntry::with_operations(EventCategory::Echo, std::iter::empty::<String>());
        assert!(!entry.matches(&EventCategory::Echo, "echo"));
    }

    #[test]
    fn subscription_entry_category_accessor() {
        let entry = SubscriptionEntry::all(EventCategory::Hil);
        assert_eq!(entry.category(), &EventCategory::Hil);
    }
}
