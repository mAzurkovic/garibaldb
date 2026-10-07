//! Rows out of a B-tree, over every key or over a range of them.

use std::ops::Bound;

use protocol::{DbError, Value};

use crate::catalog::ColumnDef;
use crate::exec::operator::Operator;
use crate::store::btree::{BTree, Cursor};
use crate::store::page::FileId;
use crate::store::pool::BufferPool;
use crate::store::row::{self, Row};

/// Walks the rows of a table in key order.
///
/// Over every key it is a full scan. Over a range it is the one case the tree
/// can narrow, which is a condition on the primary key.
pub struct Scan<'a> {
    cursor: Cursor<'a>,
    pool: &'a BufferPool,
    file: FileId,
    columns: Vec<ColumnDef>,
}

impl<'a> Scan<'a> {
    pub fn new(
        tree: &BTree<'a>,
        pool: &'a BufferPool,
        file: FileId,
        columns: Vec<ColumnDef>,
        from: Bound<Value>,
        to: Bound<Value>,
    ) -> Result<Scan<'a>, DbError> {
        Ok(Scan {
            cursor: tree.cursor(from, to)?,
            pool,
            file,
            columns,
        })
    }
}

impl Operator for Scan<'_> {
    fn next(&mut self) -> Result<Option<Row>, DbError> {
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
