//! Pages, the buffer pool, and the records that live in a page.
//!
//! A page is 8 KB and a row never crosses one, so a value too large for a
//! record moves to a chain of its own pages. The pool holds a fixed count of
//! frames, so the memory the server uses does not grow with the data.
//!
//! Nothing outside a test reads this layer yet. Milestone 7 builds the B-tree
//! on it, and milestone 8 runs statements through that.
#![allow(dead_code)]

pub mod btree;
pub mod codec;
pub mod extsort;
pub mod overflow;
pub mod page;
pub mod pool;
pub mod row;

use protocol::{DbError, ErrorCode};

/// A failure of the storage layer.
///
/// `STORAGE_FULL` is the only code the protocol has for this layer, so the
/// message carries what actually went wrong.
pub fn storage_error(message: impl Into<String>) -> DbError {
    DbError {
        code: ErrorCode::StorageFull,
        message: message.into(),
        position: None,
    }
}
