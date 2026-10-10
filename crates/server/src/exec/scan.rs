//! Rows out of a B-tree, over every key or over a range of them.

use std::ops::Bound;

use protocol::{DbError, Value};

use crate::cancel::CancelHandle;
use crate::catalog::ColumnDef;
use crate::exec::operator::Operator;
use crate::store::btree::{BTree, Cursor, Direction};
use crate::store::page::FileId;
use crate::store::pool::BufferPool;
use crate::store::row::{self, Row};

/// Walks the rows of a table in key order, or in the reverse of it.
///
/// Over every key it is a full scan. Over a range it is the one case the tree
/// can narrow, which is a condition on the primary key. The direction is the
/// other thing the tree gives for nothing: an order on the key needs no sort.
pub struct Scan<'a> {
    cursor: Cursor<'a>,
    pool: &'a BufferPool,
    file: FileId,
    columns: Vec<ColumnDef>,
    cancel: CancelHandle,
}

impl<'a> Scan<'a> {
    pub fn new(
        tree: &BTree<'a>,
        range: (Bound<Value>, Bound<Value>),
        direction: Direction,
        cancel: CancelHandle,
    ) -> Result<Scan<'a>, DbError> {
        let (from, to) = range;
        Ok(Scan {
            cursor: tree.cursor(from, to, direction)?,
            pool: tree.pool(),
            file: tree.file(),
            columns: tree.columns().to_vec(),
            cancel,
        })
    }
}

impl Operator for Scan<'_> {
    fn next(&mut self) -> Result<Option<Row>, DbError> {
        self.cancel.check()?;
        match self.cursor.next()? {
            None => Ok(None),
            Some(bytes) => Ok(Some(row::load(
                self.pool,
                self.file,
                &self.columns,
                &bytes,
            )?)),
        }
    }

    fn schema(&self) -> &[ColumnDef] {
        &self.columns
    }
}
