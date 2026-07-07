//! The worker's single-job reservation gate.
//!
//! A worker owns one GPU, so exactly one generation may run at a time.
//! Three independent code paths race for it: the WS session (studio
//! offers), the always-on local API (`POST /image`), and the
//! auto-updater (which must not `restart_self` mid-job).  Before this
//! gate the WS session guarded itself with a bare `Arc<AtomicBool>`
//! CAS while the local API ignored it entirely — so a local job and a
//! studio job could run concurrently and OOM each other, and an update
//! could kill an in-flight job.
//!
//! [`JobGate`] wraps that shared flag and hands out an RAII
//! [`JobReservation`] whose `Drop` releases the slot, so no code path
//! can forget to clear it on an early return or a panic.  The gate is
//! cheap to clone (an `Arc`) and the reservation is `Send`, so it
//! moves cleanly into a spawned task for the lifetime of a job.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A cloneable handle to the worker's one-job-at-a-time flag.
#[derive(Clone, Default)]
pub struct JobGate {
    busy: Arc<AtomicBool>,
}

impl JobGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt an existing shared flag (the runtime already threads one
    /// `Arc<AtomicBool>` through the WS session + auto-updater).
    pub fn from_shared(busy: Arc<AtomicBool>) -> Self {
        Self { busy }
    }

    /// The underlying flag, for readers that only need to observe
    /// busyness (e.g. a heartbeat) without reserving.
    pub fn shared(&self) -> Arc<AtomicBool> {
        self.busy.clone()
    }

    /// True while a job holds the slot.
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Atomically claim the slot.  `Some(reservation)` means the caller
    /// owns the worker until the reservation drops; `None` means a job
    /// is already running and the caller must back off (reject the
    /// offer / return 503 / skip the update).
    pub fn try_reserve(&self) -> Option<JobReservation> {
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| JobReservation {
                busy: self.busy.clone(),
            })
    }
}

/// RAII proof that the holder owns the worker's single job slot.
/// Releasing on `Drop` means every exit path — success, error, or
/// panic — frees the slot without an explicit store.
pub struct JobReservation {
    busy: Arc<AtomicBool>,
}

impl Drop for JobReservation {
    fn drop(&mut self) {
        self.busy.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_is_exclusive_until_dropped() {
        let gate = JobGate::new();
        assert!(!gate.is_busy());
        let reservation = gate.try_reserve().expect("first reserve wins");
        assert!(gate.is_busy());
        assert!(
            gate.try_reserve().is_none(),
            "a second reserve must fail while the first is held"
        );
        drop(reservation);
        assert!(!gate.is_busy(), "dropping the reservation frees the slot");
        assert!(gate.try_reserve().is_some(), "the slot is claimable again");
    }

    #[test]
    fn clones_share_one_slot() {
        // The local API, session, and updater hold separate clones of
        // the same gate — a reservation on one must block the others.
        let gate = JobGate::new();
        let other = gate.clone();
        let _held = gate.try_reserve().unwrap();
        assert!(other.is_busy());
        assert!(other.try_reserve().is_none());
    }

    #[test]
    fn from_shared_adopts_an_existing_flag() {
        let flag = Arc::new(AtomicBool::new(false));
        let gate = JobGate::from_shared(flag.clone());
        let _held = gate.try_reserve().unwrap();
        assert!(
            flag.load(Ordering::SeqCst),
            "reserving must set the adopted flag so existing readers see it"
        );
    }

    #[test]
    fn reservation_releases_on_panic_unwind() {
        // A job that panics mid-flight must still free the worker.
        let gate = JobGate::new();
        let gate_for_thread = gate.clone();
        let _ = std::thread::spawn(move || {
            let _held = gate_for_thread.try_reserve().unwrap();
            panic!("job blew up");
        })
        .join();
        assert!(
            !gate.is_busy(),
            "the slot must be free after a panicking holder unwinds"
        );
    }
}
