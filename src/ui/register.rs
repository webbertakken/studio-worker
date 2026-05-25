//! Status of the in-window register call.  Lives in its own module
//! so multiple tabs (e.g. About, Config) can read it without a
//! circular dependency with `tabs::status`.
//!
//! The actual call to `runtime::register` is owned by `App` so it can
//! pin the tokio Handle + apply the resulting `Config` write back to
//! disk in one place.

use std::sync::Arc;

use parking_lot::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RegistrationStatus {
    #[default]
    Idle,
    InFlight,
    Success,
    Failed(String),
}

/// Shared `Arc<Mutex<…>>` slot the App holds and the spawned register
/// task writes into.
pub type SharedRegistration = Arc<Mutex<RegistrationStatus>>;

pub fn shared() -> SharedRegistration {
    Arc::new(Mutex::new(RegistrationStatus::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_idle() {
        assert_eq!(RegistrationStatus::default(), RegistrationStatus::Idle);
    }

    #[test]
    fn failed_carries_reason() {
        let s = RegistrationStatus::Failed("boom".into());
        match s {
            RegistrationStatus::Failed(r) => assert_eq!(r, "boom"),
            _ => panic!("expected Failed"),
        }
    }
}
