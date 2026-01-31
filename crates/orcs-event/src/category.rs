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

    /// Returns the display name of this category.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Lifecycle => "Lifecycle".to_string(),
            Self::Hil => "Hil".to_string(),
            Self::Echo => "Echo".to_string(),
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
        let json = serde_json::to_string(&hil).unwrap();
        assert!(json.contains("Hil"));

        let ext = EventCategory::extension("ns", "kind");
        let json = serde_json::to_string(&ext).unwrap();
        assert!(json.contains("Extension"));
        assert!(json.contains("ns"));
        assert!(json.contains("kind"));
    }

    #[test]
    fn category_deserialize() {
        let json = r#""Hil""#;
        let cat: EventCategory = serde_json::from_str(json).unwrap();
        assert_eq!(cat, EventCategory::Hil);

        let json = r#"{"Extension":{"namespace":"ns","kind":"k"}}"#;
        let cat: EventCategory = serde_json::from_str(json).unwrap();
        assert_eq!(cat, EventCategory::extension("ns", "k"));
    }
}
