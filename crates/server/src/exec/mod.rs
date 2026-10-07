//! Running a statement: an iterator over the B-tree, one row at a time.
//!
//! Each operator hands back one row from `next`, and the session writes it to
//! the socket as it arrives, so nothing holds a whole result.

pub mod dml;
pub mod eval;
pub mod filter;
pub mod operator;
pub mod planner;
pub mod scan;
