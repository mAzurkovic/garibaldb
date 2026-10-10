//! The cancel flag of a connection and the registry that finds it.
//!
//! A client that wants to stop a running statement opens a second connection
//! and sends `Cancel` with the connection id and the secret from `Ready`. The
//! registry finds the connection and sets its flag. The running statement
//! reads the flag between rows. See [FR66].
//!
//! The secret is the only guard, because [NFR20] asks for no authentication.
//! A wrong secret cancels nothing.

use std::collections::HashMap;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use protocol::{DbError, ErrorCode};

use crate::catalog::named;

/// The cancel flag of one connection. A clone points at the same flag.
#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    flag: Arc<AtomicBool>,
}

impl CancelHandle {
    pub fn new() -> Self {
        CancelHandle::default()
    }

    /// Sets the flag. `SeqCst` is the simplest ordering that is correct
    /// across threads.
    pub fn stop(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Reads the flag. `SeqCst` for the same reason as `stop`.
    pub fn stopped(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Clears the flag, which the session does before each statement so one
    /// cancel stops one statement.
    pub fn clear(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Fails when the connection has been asked to stop.
    ///
    /// The operators call this between rows, which is as often as a statement
    /// can stop: nothing below them waits for longer than one page read.
    pub fn check(&self) -> Result<(), DbError> {
        match self.stopped() {
            true => Err(named(ErrorCode::Cancelled, "the statement was cancelled")),
            false => Ok(()),
        }
    }
}

/// The secret and the handle of every live connection. See [FR66].
#[derive(Debug, Default)]
pub struct CancelRegistry {
    map: Mutex<HashMap<u64, (u64, CancelHandle)>>,
}

impl CancelRegistry {
    pub fn new() -> Self {
        CancelRegistry::default()
    }

    /// Adds the connection and returns its handle. The caller keeps a clone.
    pub fn register(&self, conn_id: u64, secret: u64) -> CancelHandle {
        let handle = CancelHandle::new();
        self.lock().insert(conn_id, (secret, handle.clone()));
        handle
    }

    /// Removes the connection. The session calls it when the client leaves.
    pub fn unregister(&self, conn_id: u64) {
        self.lock().remove(&conn_id);
    }

    /// Sets the flag of the connection. Returns false for an unknown id or a
    /// wrong secret.
    pub fn cancel(&self, conn_id: u64, secret: u64) -> bool {
        match self.lock().get(&conn_id) {
            Some((want, handle)) if *want == secret => {
                handle.stop();
                true
            }
            _ => false,
        }
    }

    /// Recovers the guard of a poisoned mutex, because one panicking session
    /// must not stop cancel for every other connection. The map holds no
    /// invariant that a panic can break.
    fn lock(&self) -> MutexGuard<'_, HashMap<u64, (u64, CancelHandle)>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A random secret for one connection. `RandomState` is seeded by the
/// operating system, so the value needs no dependency and no clock.
pub fn new_secret() -> u64 {
    RandomState::new().build_hasher().finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_handle_is_clear() {
        assert!(!CancelHandle::new().stopped());
    }

    #[test]
    fn a_stopped_handle_fails_a_check_until_it_is_cleared() {
        let handle = CancelHandle::new();
        handle.check().expect("a clear handle stops nothing");

        handle.stop();
        let e = handle.check().err().unwrap();
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert!(e.message.contains("cancelled"), "{e}");

        // One cancel stops one statement.
        handle.clear();
        handle.check().expect("the next statement runs");
        assert!(!handle.stopped());
    }

    #[test]
    fn stop_sets_the_flag() {
        let handle = CancelHandle::new();
        handle.stop();
        assert!(handle.stopped());
    }

    #[test]
    fn a_clone_shares_the_flag() {
        let handle = CancelHandle::new();
        let clone = handle.clone();
        handle.stop();
        assert!(clone.stopped());
    }

    #[test]
    fn the_right_secret_sets_the_flag() {
        let reg = CancelRegistry::new();
        let handle = reg.register(1, 42);
        assert!(reg.cancel(1, 42));
        assert!(handle.stopped());
    }

    #[test]
    fn a_wrong_secret_leaves_the_flag_clear() {
        let reg = CancelRegistry::new();
        let handle = reg.register(1, 42);
        assert!(!reg.cancel(1, 43));
        assert!(!handle.stopped());
    }

    #[test]
    fn an_unknown_id_returns_false() {
        let reg = CancelRegistry::new();
        reg.register(1, 42);
        assert!(!reg.cancel(2, 42));
    }

    #[test]
    fn an_unregistered_id_returns_false() {
        let reg = CancelRegistry::new();
        reg.register(1, 42);
        reg.unregister(1);
        assert!(!reg.cancel(1, 42));
    }

    #[test]
    fn two_secrets_differ() {
        assert_ne!(new_secret(), new_secret());
    }

    #[test]
    fn another_thread_sees_the_flag_that_the_registry_sets() {
        let reg = Arc::new(CancelRegistry::new());
        let handle = reg.register(1, 42);
        let worker = std::thread::spawn(move || {
            while !handle.stopped() {
                std::hint::spin_loop();
            }
            true
        });
        assert!(reg.cancel(1, 42));
        assert!(worker.join().unwrap());
    }
}
