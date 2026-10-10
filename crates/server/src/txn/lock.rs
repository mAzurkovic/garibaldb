//! The write lock of one database.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use protocol::{DbError, ErrorCode};

use crate::catalog::{Database, named};

/// One lock for each database, taken by a write and held to its commit or
/// rollback, so two writers never overlap and the commits of a database fall
/// in one order.
///
/// A flag and a condvar, not a `Mutex<()>`: the holder outlives the statement
/// that took it, and a `MutexGuard` cannot be stored without a lifetime on
/// everything that holds one. The standard mutex has no timed lock either,
/// and a wait has to end.
#[derive(Default)]
pub struct WriteLock {
    held: Mutex<bool>,
    freed: Condvar,
}

impl WriteLock {
    /// Takes the lock, waiting no longer than `timeout`.
    pub fn acquire(&self, timeout: Duration) -> Result<(), DbError> {
        let deadline = Instant::now() + timeout;
        let mut held = self.held.lock().expect("the write lock holds");
        // A condvar wakes a waiter without promising the lock is free, and in
        // no order, so the wait runs until the flag is clear or the time is
        // up. A waiter can lose the lock to a newer one more than once, which
        // the timeout bounds.
        while *held {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(named(
                    ErrorCode::LockTimeout,
                    "another transaction holds the write lock of this database",
                ));
            }
            held = self
                .freed
                .wait_timeout(held, left)
                .expect("the write lock holds")
                .0;
        }
        *held = true;
        Ok(())
    }

    fn release(&self) {
        *self.held.lock().expect("the write lock holds") = false;
        self.freed.notify_one();
    }
}

/// The write lock of a database, held until this drops.
///
/// It holds the database and not a borrow of the lock, so a transaction can
/// keep it across statements without a lifetime.
pub struct WriteGuard(Arc<Database>);

impl WriteGuard {
    /// Takes the write lock of a database.
    pub fn take(db: &Arc<Database>, timeout: Duration) -> Result<WriteGuard, DbError> {
        db.lock.acquire(timeout)?;
        Ok(WriteGuard(Arc::clone(db)))
    }
}

impl Drop for WriteGuard {
    fn drop(&mut self) {
        self.0.lock.release();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::catalog::testing::Dir;

    const PATIENCE: Duration = Duration::from_secs(5);
    const BRIEF: Duration = Duration::from_millis(50);

    #[test]
    fn one_holder_at_a_time() {
        let lock = WriteLock::default();
        lock.acquire(PATIENCE).unwrap();

        let e = lock.acquire(BRIEF).err().unwrap();
        assert_eq!(e.code, ErrorCode::LockTimeout);
        assert!(e.message.contains("holds the write lock"), "{e}");

        lock.release();
        lock.acquire(BRIEF).expect("the lock is free again");
    }

    #[test]
    fn a_waiter_takes_the_lock_the_moment_it_is_freed() {
        let lock = Arc::new(WriteLock::default());
        lock.acquire(PATIENCE).unwrap();

        let (took, waiting) = mpsc::channel();
        let waiter = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let answer = lock.acquire(PATIENCE);
                took.send(()).expect("the test is listening");
                answer
            })
        };
        assert!(
            waiting.recv_timeout(BRIEF).is_err(),
            "the lock was held, so nothing could take it"
        );

        lock.release();
        waiter.join().expect("the waiter ends").unwrap();
    }

    #[test]
    fn a_guard_frees_the_lock_when_it_drops() {
        let dir = Dir::new("guard");
        let (_registry, held) = dir.shop();

        let guard = WriteGuard::take(&held.db, PATIENCE).unwrap();
        let e = WriteGuard::take(&held.db, BRIEF).err().unwrap();
        assert_eq!(e.code, ErrorCode::LockTimeout);

        drop(guard);
        WriteGuard::take(&held.db, BRIEF).expect("the lock is free again");
    }
}
