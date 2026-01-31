//! Status types for component state reporting.
//!
//! Components report their status via the [`Statusable`](super::Statusable) trait.
//! This enables managers to monitor child component health and progress.
//!
//! # Status Lifecycle
//!
//! ```text
//! Initializing → Idle ⇄ Running → Completed
//!                  ↓         ↓
//!                Paused    Error
//!                  ↓         ↓
//!               Aborted ← ───┘
//! ```
//!
//! # Example
//!
//! ```
//! use orcs_component::{Status, StatusDetail, Progress};
//!
//! let status = Status::Running;
//! let detail = StatusDetail {
//!     message: Some("Processing files...".into()),
//!     progress: Some(Progress {
//!         current: 42,
//!         total: Some(100),
//!         unit: Some("files".into()),
//!     }),
//!     metadata: Default::default(),
//! };
//!
//! assert!(status.is_active());
//! ```

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Component execution status.
///
/// Represents the current state of a component or child.
///
/// # State Categories
///
/// | Category | States | Can Receive Work |
/// |----------|--------|------------------|
/// | Active | `Running`, `AwaitingApproval` | Yes (in progress) |
/// | Ready | `Idle` | Yes |
/// | Terminal | `Completed`, `Error`, `Aborted` | No |
/// | Setup | `Initializing`, `Paused` | No (temporarily) |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Status {
    /// Component is initializing.
    ///
    /// Setup phase before accepting work.
    #[default]
    Initializing,

    /// Component is idle, waiting for work.
    ///
    /// Ready to accept requests.
    Idle,

    /// Component is actively processing.
    Running,

    /// Component is paused (can resume).
    ///
    /// Temporarily stopped, can continue with Resume signal.
    Paused,

    /// Component is waiting for HIL approval.
    ///
    /// Blocked on human decision.
    AwaitingApproval,

    /// Component completed successfully.
    ///
    /// Terminal state - no more work will be done.
    Completed,

    /// Component encountered an error.
    ///
    /// Terminal state - may be recoverable with retry.
    Error,

    /// Component was aborted (by signal).
    ///
    /// Terminal state - explicitly stopped.
    Aborted,
}

impl Status {
    /// Returns `true` if the component is actively working.
    ///
    /// Active states: `Running`, `AwaitingApproval`
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_component::Status;
    ///
    /// assert!(Status::Running.is_active());
    /// assert!(Status::AwaitingApproval.is_active());
    /// assert!(!Status::Idle.is_active());
    /// ```
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::AwaitingApproval)
    }

    /// Returns `true` if the component is in a terminal state.
    ///
    /// Terminal states: `Completed`, `Error`, `Aborted`
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_component::Status;
    ///
    /// assert!(Status::Completed.is_terminal());
    /// assert!(Status::Error.is_terminal());
    /// assert!(Status::Aborted.is_terminal());
    /// assert!(!Status::Running.is_terminal());
    /// ```
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Error | Self::Aborted)
    }

    /// Returns `true` if the component can accept new work.
    ///
    /// Ready state: `Idle`
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_component::Status;
    ///
    /// assert!(Status::Idle.is_ready());
    /// assert!(!Status::Running.is_ready());
    /// ```
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Idle)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initializing => write!(f, "initializing"),
            Self::Idle => write!(f, "idle"),
            Self::Running => write!(f, "running"),
            Self::Paused => write!(f, "paused"),
            Self::AwaitingApproval => write!(f, "awaiting_approval"),
            Self::Completed => write!(f, "completed"),
            Self::Error => write!(f, "error"),
            Self::Aborted => write!(f, "aborted"),
        }
    }
}

/// Detailed status information.
///
/// Optional extended information for debugging and UI display.
///
/// # Example
///
/// ```
/// use orcs_component::{StatusDetail, Progress};
///
/// let detail = StatusDetail {
///     message: Some("Compiling crate...".into()),
///     progress: Some(Progress {
///         current: 5,
///         total: Some(10),
///         unit: Some("crates".into()),
///     }),
///     metadata: Default::default(),
/// };
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatusDetail {
    /// Human-readable status message.
    pub message: Option<String>,

    /// Progress information (if applicable).
    pub progress: Option<Progress>,

    /// Additional metadata (for debugging/analytics).
    pub metadata: HashMap<String, serde_json::Value>,
}

impl StatusDetail {
    /// Creates a new [`StatusDetail`] with just a message.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_component::StatusDetail;
    ///
    /// let detail = StatusDetail::with_message("Processing...");
    /// assert_eq!(detail.message, Some("Processing...".into()));
    /// ```
    #[must_use]
    pub fn with_message(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            progress: None,
            metadata: HashMap::new(),
        }
    }

    /// Creates a new [`StatusDetail`] with progress.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_component::{StatusDetail, Progress};
    ///
    /// let detail = StatusDetail::with_progress(Progress::new(5, Some(10)));
    /// assert!(detail.progress.is_some());
    /// ```
    #[must_use]
    pub fn with_progress(progress: Progress) -> Self {
        Self {
            message: None,
            progress: Some(progress),
            metadata: HashMap::new(),
        }
    }
}

/// Progress information for long-running operations.
///
/// # Example
///
/// ```
/// use orcs_component::Progress;
///
/// // Determinate progress (known total)
/// let progress = Progress::new(50, Some(100))
///     .with_unit("files");
/// assert_eq!(progress.percentage(), Some(50.0));
///
/// // Indeterminate progress (unknown total)
/// let progress = Progress::new(42, None);
/// assert_eq!(progress.percentage(), None);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    /// Current progress value.
    pub current: u64,

    /// Total value (if known).
    pub total: Option<u64>,

    /// Unit of measurement (e.g., "files", "tokens", "bytes").
    pub unit: Option<String>,
}

impl Progress {
    /// Creates a new [`Progress`].
    ///
    /// # Arguments
    ///
    /// * `current` - Current progress value
    /// * `total` - Total value (None for indeterminate)
    #[must_use]
    pub fn new(current: u64, total: Option<u64>) -> Self {
        Self {
            current,
            total,
            unit: None,
        }
    }

    /// Sets the unit of measurement.
    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = Some(unit.into());
        self
    }

    /// Returns the progress as a percentage (0.0 - 100.0).
    ///
    /// Returns `None` if total is unknown or zero.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_component::Progress;
    ///
    /// let progress = Progress::new(25, Some(100));
    /// assert_eq!(progress.percentage(), Some(25.0));
    ///
    /// let progress = Progress::new(42, None);
    /// assert_eq!(progress.percentage(), None);
    /// ```
    #[must_use]
    pub fn percentage(&self) -> Option<f64> {
        self.total.and_then(|total| {
            if total == 0 {
                None
            } else {
                Some((self.current as f64 / total as f64) * 100.0)
            }
        })
    }

    /// Returns `true` if progress is complete.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_component::Progress;
    ///
    /// let progress = Progress::new(100, Some(100));
    /// assert!(progress.is_complete());
    ///
    /// let progress = Progress::new(50, Some(100));
    /// assert!(!progress.is_complete());
    /// ```
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.total.is_some_and(|total| self.current >= total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_active() {
        assert!(Status::Running.is_active());
        assert!(Status::AwaitingApproval.is_active());
        assert!(!Status::Idle.is_active());
        assert!(!Status::Completed.is_active());
    }

    #[test]
    fn status_is_terminal() {
        assert!(Status::Completed.is_terminal());
        assert!(Status::Error.is_terminal());
        assert!(Status::Aborted.is_terminal());
        assert!(!Status::Running.is_terminal());
        assert!(!Status::Idle.is_terminal());
    }

    #[test]
    fn status_is_ready() {
        assert!(Status::Idle.is_ready());
        assert!(!Status::Running.is_ready());
        assert!(!Status::Initializing.is_ready());
    }

    #[test]
    fn status_default() {
        assert_eq!(Status::default(), Status::Initializing);
    }

    #[test]
    fn status_display() {
        assert_eq!(format!("{}", Status::Running), "running");
        assert_eq!(format!("{}", Status::AwaitingApproval), "awaiting_approval");
    }

    #[test]
    fn progress_percentage() {
        let progress = Progress::new(50, Some(100));
        assert_eq!(progress.percentage(), Some(50.0));

        let progress = Progress::new(25, Some(100));
        assert_eq!(progress.percentage(), Some(25.0));

        let progress = Progress::new(42, None);
        assert_eq!(progress.percentage(), None);

        let progress = Progress::new(42, Some(0));
        assert_eq!(progress.percentage(), None);
    }

    #[test]
    fn progress_is_complete() {
        assert!(Progress::new(100, Some(100)).is_complete());
        assert!(Progress::new(150, Some(100)).is_complete());
        assert!(!Progress::new(50, Some(100)).is_complete());
        assert!(!Progress::new(50, None).is_complete());
    }

    #[test]
    fn progress_with_unit() {
        let progress = Progress::new(10, Some(100)).with_unit("files");
        assert_eq!(progress.unit, Some("files".into()));
    }

    #[test]
    fn status_detail_with_message() {
        let detail = StatusDetail::with_message("Processing...");
        assert_eq!(detail.message, Some("Processing...".into()));
        assert!(detail.progress.is_none());
    }

    #[test]
    fn status_detail_with_progress() {
        let detail = StatusDetail::with_progress(Progress::new(5, Some(10)));
        assert!(detail.message.is_none());
        assert!(detail.progress.is_some());
    }
}
