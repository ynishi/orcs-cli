//! Privilege level types.

use std::time::{Duration, Instant};

/// The current privilege level of a session.
///
/// This implements a sudo-like model where all actors start with
/// limited permissions and must explicitly elevate to perform
/// privileged operations.
///
/// # Design Rationale
///
/// ## Why Not Always Elevated?
///
/// Even human users operate in Standard mode by default:
///
/// - **Prevents accidents**: `git reset --hard` requires explicit elevation
/// - **Audit clarity**: Elevated actions are intentional and logged
/// - **Network safety**: Compromised sessions have limited damage potential
///
/// ## Time-Limited Elevation
///
/// Elevated privileges automatically expire to minimize the window
/// of elevated access. This follows the principle of least privilege.
///
/// # Example
///
/// ```
/// use orcs_auth::PrivilegeLevel;
/// use std::time::{Duration, Instant};
///
/// // Standard mode (default)
/// let standard = PrivilegeLevel::Standard;
/// assert!(!standard.is_elevated());
///
/// // Elevated mode (explicit, time-limited)
/// let until = Instant::now() + Duration::from_secs(300);
/// let elevated = PrivilegeLevel::Elevated { until };
/// assert!(elevated.is_elevated());
/// ```
#[derive(Debug, Clone, Default)]
pub enum PrivilegeLevel {
    /// Normal operations only.
    ///
    /// In this mode, the following are **not allowed**:
    ///
    /// - Global signals (Veto)
    /// - Destructive file operations (`rm -rf`, overwrite without backup)
    /// - Destructive git operations (`reset --hard`, `push --force`)
    /// - Modifying system configuration
    ///
    /// This is the default mode for all principals.
    #[default]
    Standard,

    /// Elevated privileges with expiration.
    ///
    /// Grants full access to all operations until the specified time.
    /// After expiration, the session automatically drops to Standard.
    ///
    /// # Fields
    ///
    /// * `until` - When elevation expires (automatically serializes as duration from now)
    Elevated {
        /// Expiration time for elevated privileges.
        until: Instant,
    },
}

impl PrivilegeLevel {
    /// Creates a new Standard privilege level.
    #[must_use]
    pub fn standard() -> Self {
        Self::Standard
    }

    /// Creates a new Elevated privilege level with the given duration.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::PrivilegeLevel;
    /// use std::time::Duration;
    ///
    /// let elevated = PrivilegeLevel::elevated_for(Duration::from_secs(60));
    /// assert!(elevated.is_elevated());
    /// ```
    #[must_use]
    pub fn elevated_for(duration: Duration) -> Self {
        Self::Elevated {
            until: Instant::now() + duration,
        }
    }

    /// Returns `true` if currently elevated (and not expired).
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::PrivilegeLevel;
    /// use std::time::Duration;
    ///
    /// let standard = PrivilegeLevel::Standard;
    /// assert!(!standard.is_elevated());
    ///
    /// let elevated = PrivilegeLevel::elevated_for(Duration::from_secs(60));
    /// assert!(elevated.is_elevated());
    /// ```
    #[must_use]
    pub fn is_elevated(&self) -> bool {
        match self {
            Self::Standard => false,
            Self::Elevated { until } => Instant::now() < *until,
        }
    }

    /// Returns `true` if this is Standard mode or elevation has expired.
    #[must_use]
    pub fn is_standard(&self) -> bool {
        !self.is_elevated()
    }

    /// Returns the remaining elevation time, or `None` if not elevated.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_auth::PrivilegeLevel;
    /// use std::time::Duration;
    ///
    /// let elevated = PrivilegeLevel::elevated_for(Duration::from_secs(60));
    /// let remaining = elevated.remaining();
    /// assert!(remaining.is_some());
    /// assert!(remaining.unwrap() <= Duration::from_secs(60));
    /// ```
    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        match self {
            Self::Standard => None,
            Self::Elevated { until } => {
                let now = Instant::now();
                if now < *until {
                    Some(*until - now)
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_is_not_elevated() {
        let level = PrivilegeLevel::Standard;
        assert!(!level.is_elevated());
        assert!(level.is_standard());
        assert!(level.remaining().is_none());
    }

    #[test]
    fn elevated_is_elevated() {
        let level = PrivilegeLevel::elevated_for(Duration::from_secs(60));
        assert!(level.is_elevated());
        assert!(!level.is_standard());
        assert!(level.remaining().is_some());
    }

    #[test]
    fn expired_elevation_is_standard() {
        let level = PrivilegeLevel::Elevated {
            until: Instant::now() - Duration::from_secs(1),
        };
        assert!(!level.is_elevated());
        assert!(level.is_standard());
        assert!(level.remaining().is_none());
    }

    #[test]
    fn default_is_standard() {
        let level = PrivilegeLevel::default();
        assert!(level.is_standard());
    }

    #[test]
    fn remaining_decreases() {
        let level = PrivilegeLevel::elevated_for(Duration::from_secs(60));
        let remaining = level
            .remaining()
            .expect("elevated level should have remaining duration");
        assert!(remaining <= Duration::from_secs(60));
    }
}
