//! Reading a log back at startup.
//!
//! Forward from the start, checking every frame. The last commit frame is the
//! line: everything before it happened, everything after it did not.

use std::fs::File;

use protocol::DbError;

use crate::store::storage_error;
use crate::wal::index::FrameIndex;
use crate::wal::writer::{FrameKind, Read, read_frame};

/// What a log held when it was last closed, or crashed.
pub struct Recovered {
    pub index: FrameIndex,
    /// Where the log ends once the uncommitted tail is cut away.
    pub end: u64,
    pub next_lsn: u64,
}

/// Reads a log forward and settles what survived.
pub fn recover(file: &File) -> Result<Recovered, DbError> {
    let len = file
        .metadata()
        .map_err(|e| storage_error(format!("the log did not read: {e}")))?
        .len();

    let mut index = FrameIndex::default();
    let mut pending: Vec<(u32, u32, u64)> = Vec::new();
    let mut committed = Recovered {
        index: FrameIndex::default(),
        end: 0,
        next_lsn: 0,
    };
    let mut at = 0;
    loop {
        match read_frame(file, at, len)? {
            Read::End => break,
            // A frame half written, or one whose checksum disagrees. Nothing
            // after it can be trusted, so the log ends here.
            Read::Broken => {
                log::warn!("the log breaks at byte {at}, so recovery stops there");
                break;
            }
            Read::Frame(frame, _) => {
                at = frame.at + frame.len;
                match frame.kind {
                    FrameKind::Page => pending.push((frame.table, frame.page_no, frame.at)),
                    FrameKind::Commit => {
                        for (table, page_no, offset) in pending.drain(..) {
                            index.insert(table, page_no, offset);
                        }
                        committed.end = at;
                        committed.next_lsn = frame.lsn + 1;
                    }
                }
            }
        }
    }
    // Pages with no commit frame after them never happened.
    committed.index = index;
    Ok(committed)
}
