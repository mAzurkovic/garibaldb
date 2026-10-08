//! The write-ahead log, and what it takes to come back from a crash.
//!
//! Redo only. A changed page is appended to the log and never written to its
//! table file until a checkpoint moves it, so a page that was never committed
//! can be dropped by ignoring the tail of the log. There is no undo log and
//! nothing to roll back.

pub mod checkpoint;
pub mod index;
pub mod recovery;
pub mod writer;
