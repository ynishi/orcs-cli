//! Capability-based permission model.
//!
//! Defines the logical capabilities that control *what* operations
//! a context can perform.
//!
//! # Three-Layer Permission Model
//!
//! ```text
//! Effective Permission = Capability(What) ∩ SandboxPolicy(Where) ∩ Session(Who+When)
//! ```
//!
//! - **Capability** controls *what* operations are allowed (logical permissions).
//! - **SandboxPolicy** controls *where* operations are allowed (resource boundary).
//! - **Session** controls *who* is acting and *when* (privilege level).
//!
//! All layers must permit an operation for it to succeed. Deny wins.
//!
//! # Inheritance
//!
//! Capabilities are inherited from parent to child with narrowing only:
//!
//! ```text
//! Component (ALL)
//! ├── Child-A (READ | WRITE)       ← subset of parent
//! └── SubAgent-B (READ)            ← subset of parent
//!     └── Child-B-1 (READ)         ← inherited, cannot exceed parent
//! ```
//!
//! A child can never hold a capability that its parent does not hold.
//!
//! # Example
//!
//! ```
//! use orcs_auth::Capability;
//!
//! // Full access (default)
//! let all = Capability::ALL;
//! assert!(all.contains(Capability::READ));
//! assert!(all.contains(Capability::EXECUTE));
//!
//! // Read-only agent
//! let read_only = Capability::READ;
//! assert!(read_only.contains(Capability::READ));
//! assert!(!read_only.contains(Capability::WRITE));
//!
//! // Inheritance: child = parent ∩ requested
//! let parent = Capability::READ | Capability::WRITE;
//! let requested = Capability::READ | Capability::EXECUTE;
//! let effective = parent & requested;
//! assert_eq!(effective, Capability::READ);
//! ```

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    /// Logical capabilities that a context can grant to child entities.
    ///
    /// Each capability gates a set of operations. Operations require
    /// the corresponding capability to be present in the context.
    ///
    /// | Capability | Operations |
    /// |------------|------------|
    /// | [`READ`](Self::READ) | `orcs.read`, `orcs.grep`, `orcs.glob` |
    /// | [`WRITE`](Self::WRITE) | `orcs.write`, `orcs.mkdir` |
    /// | [`DELETE`](Self::DELETE) | `orcs.remove`, `orcs.mv` |
    /// | [`EXECUTE`](Self::EXECUTE) | `orcs.exec` |
    /// | [`SPAWN`](Self::SPAWN) | `orcs.spawn_child`, `orcs.spawn_runner` |
    /// | [`LLM`](Self::LLM) | `orcs.llm` |
    /// | [`HTTP`](Self::HTTP) | `orcs.http` |
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct Capability: u16 {
        /// Read files: `orcs.read`, `orcs.grep`, `orcs.glob`
        const READ    = 0b0000_0001;
        /// Write files: `orcs.write`, `orcs.mkdir`
        const WRITE   = 0b0000_0010;
        /// Delete/move files: `orcs.remove`, `orcs.mv`
        const DELETE  = 0b0000_0100;
        /// Execute commands: `orcs.exec`
        const EXECUTE = 0b0000_1000;
        /// Spawn children/runners: `orcs.spawn_child`, `orcs.spawn_runner`
        const SPAWN   = 0b0001_0000;
        /// Call LLM: `orcs.llm`
        const LLM     = 0b0010_0000;
        /// HTTP requests: `orcs.http`
        const HTTP    = 0b0100_0000;
    }
}

impl Capability {
    /// All file operations: READ | WRITE | DELETE.
    pub const FILE_ALL: Self = Self::READ.union(Self::WRITE).union(Self::DELETE);

    /// All capabilities.
    pub const ALL: Self = Self::FILE_ALL
        .union(Self::EXECUTE)
        .union(Self::SPAWN)
        .union(Self::LLM)
        .union(Self::HTTP);

    /// Computes the effective capabilities for a child.
    ///
    /// Returns the intersection of parent and requested capabilities.
    /// A child can never exceed its parent's capabilities.
    ///
    /// # Arguments
    ///
    /// * `parent` - Parent's capabilities
    /// * `requested` - Requested capabilities for the child
    ///
    /// # Returns
    ///
    /// `parent & requested` — the effective capability set.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::Capability;
    ///
    /// let parent = Capability::READ | Capability::WRITE;
    /// let requested = Capability::READ | Capability::EXECUTE;
    /// let effective = Capability::inherit(parent, requested);
    /// assert_eq!(effective, Capability::READ);
    /// ```
    #[must_use]
    pub fn inherit(parent: Self, requested: Self) -> Self {
        parent & requested
    }

    /// Returns a human-readable list of capability names.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::Capability;
    ///
    /// let caps = Capability::READ | Capability::WRITE;
    /// let names = caps.names();
    /// assert!(names.contains(&"READ"));
    /// assert!(names.contains(&"WRITE"));
    /// ```
    #[must_use]
    pub fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.contains(Self::READ) {
            names.push("READ");
        }
        if self.contains(Self::WRITE) {
            names.push("WRITE");
        }
        if self.contains(Self::DELETE) {
            names.push("DELETE");
        }
        if self.contains(Self::EXECUTE) {
            names.push("EXECUTE");
        }
        if self.contains(Self::SPAWN) {
            names.push("SPAWN");
        }
        if self.contains(Self::LLM) {
            names.push("LLM");
        }
        if self.contains(Self::HTTP) {
            names.push("HTTP");
        }
        names
    }

    /// Parses a capability name string (case-insensitive).
    ///
    /// Unlike [`Flags::from_name`] (exact match), this accepts
    /// lowercase input and aliases like "EXEC" for "EXECUTE".
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::Capability;
    ///
    /// assert_eq!(Capability::parse("read"), Some(Capability::READ));
    /// assert_eq!(Capability::parse("EXECUTE"), Some(Capability::EXECUTE));
    /// assert_eq!(Capability::parse("exec"), Some(Capability::EXECUTE));
    /// assert_eq!(Capability::parse("http"), Some(Capability::HTTP));
    /// assert_eq!(Capability::parse("unknown"), None);
    /// ```
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_uppercase().as_str() {
            "READ" => Some(Self::READ),
            "WRITE" => Some(Self::WRITE),
            "DELETE" => Some(Self::DELETE),
            "EXECUTE" | "EXEC" => Some(Self::EXECUTE),
            "SPAWN" => Some(Self::SPAWN),
            "LLM" => Some(Self::LLM),
            "HTTP" => Some(Self::HTTP),
            "ALL" => Some(Self::ALL),
            "FILE_ALL" => Some(Self::FILE_ALL),
            _ => None,
        }
    }

    /// Parses a list of capability names into a combined set.
    ///
    /// Returns the combined capabilities and a list of unknown names.
    /// Callers should decide how to handle unknown names (error, warn, etc.)
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::Capability;
    ///
    /// let (caps, unknown) = Capability::parse_list(&["READ", "WRITE"]);
    /// assert_eq!(caps, Capability::READ | Capability::WRITE);
    /// assert!(unknown.is_empty());
    ///
    /// let (caps, unknown) = Capability::parse_list(&["READ", "bad"]);
    /// assert_eq!(caps, Capability::READ);
    /// assert_eq!(unknown, vec!["bad"]);
    /// ```
    #[must_use]
    pub fn parse_list<'a>(names: &[&'a str]) -> (Self, Vec<&'a str>) {
        let mut caps = Self::empty();
        let mut unknown = Vec::new();
        for name in names {
            match Self::parse(name) {
                Some(c) => caps |= c,
                None => unknown.push(*name),
            }
        }
        (caps, unknown)
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = self.names();
        if names.is_empty() {
            write!(f, "(none)")
        } else {
            write!(f, "{}", names.join(" | "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_contains_every_capability() {
        assert!(Capability::ALL.contains(Capability::READ));
        assert!(Capability::ALL.contains(Capability::WRITE));
        assert!(Capability::ALL.contains(Capability::DELETE));
        assert!(Capability::ALL.contains(Capability::EXECUTE));
        assert!(Capability::ALL.contains(Capability::SPAWN));
        assert!(Capability::ALL.contains(Capability::LLM));
        assert!(Capability::ALL.contains(Capability::HTTP));
    }

    #[test]
    fn file_all_contains_file_ops() {
        assert!(Capability::FILE_ALL.contains(Capability::READ));
        assert!(Capability::FILE_ALL.contains(Capability::WRITE));
        assert!(Capability::FILE_ALL.contains(Capability::DELETE));
        assert!(!Capability::FILE_ALL.contains(Capability::EXECUTE));
        assert!(!Capability::FILE_ALL.contains(Capability::SPAWN));
        assert!(!Capability::FILE_ALL.contains(Capability::LLM));
        assert!(!Capability::FILE_ALL.contains(Capability::HTTP));
    }

    #[test]
    fn inherit_narrows_capabilities() {
        let parent = Capability::READ | Capability::WRITE | Capability::EXECUTE;
        let requested = Capability::READ | Capability::DELETE;
        let effective = Capability::inherit(parent, requested);

        assert_eq!(effective, Capability::READ);
    }

    #[test]
    fn inherit_cannot_exceed_parent() {
        let parent = Capability::READ;
        let requested = Capability::ALL;
        let effective = Capability::inherit(parent, requested);

        assert_eq!(effective, Capability::READ);
    }

    #[test]
    fn inherit_all_from_all() {
        let effective = Capability::inherit(Capability::ALL, Capability::ALL);
        assert_eq!(effective, Capability::ALL);
    }

    #[test]
    fn empty_capability() {
        let empty = Capability::empty();
        assert!(!empty.contains(Capability::READ));
        assert_eq!(empty.names(), Vec::<&str>::new());
        assert_eq!(empty.to_string(), "(none)");
    }

    #[test]
    fn names_returns_set_capabilities() {
        let caps = Capability::READ | Capability::EXECUTE;
        let names = caps.names();
        assert_eq!(names, vec!["READ", "EXECUTE"]);
    }

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(Capability::parse("read"), Some(Capability::READ));
        assert_eq!(Capability::parse("READ"), Some(Capability::READ));
        assert_eq!(Capability::parse("Read"), Some(Capability::READ));
        assert_eq!(Capability::parse("exec"), Some(Capability::EXECUTE));
        assert_eq!(Capability::parse("EXECUTE"), Some(Capability::EXECUTE));
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert_eq!(Capability::parse("NETWORK"), None);
        assert_eq!(Capability::parse(""), None);
    }

    #[test]
    fn parse_list_combines() {
        let (caps, unknown) = Capability::parse_list(&["READ", "WRITE"]);
        assert_eq!(caps, Capability::READ | Capability::WRITE);
        assert!(unknown.is_empty());
    }

    #[test]
    fn parse_list_reports_unknown() {
        let (caps, unknown) = Capability::parse_list(&["READ", "bad", "WRITE", "nope"]);
        assert_eq!(caps, Capability::READ | Capability::WRITE);
        assert_eq!(unknown, vec!["bad", "nope"]);
    }

    #[test]
    fn display_formatting() {
        assert_eq!(Capability::READ.to_string(), "READ");
        assert_eq!(
            (Capability::READ | Capability::WRITE).to_string(),
            "READ | WRITE"
        );
        assert_eq!(Capability::empty().to_string(), "(none)");
    }

    #[test]
    fn serde_roundtrip() {
        let caps = Capability::READ | Capability::EXECUTE;
        let json = serde_json::to_string(&caps).expect("serialize");
        let parsed: Capability = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, caps);
    }

    #[test]
    fn bitwise_operations() {
        let a = Capability::READ | Capability::WRITE;
        let b = Capability::WRITE | Capability::DELETE;

        // Union
        assert_eq!(
            a | b,
            Capability::READ | Capability::WRITE | Capability::DELETE
        );
        // Intersection
        assert_eq!(a & b, Capability::WRITE);
        // Difference
        assert_eq!(a - b, Capability::READ);
    }
}
