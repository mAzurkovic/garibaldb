//! Rows in the order a statement asked for.

use protocol::DbError;

use crate::cancel::CancelHandle;
use crate::catalog::ColumnDef;
use crate::exec::operator::Operator;
use crate::store::extsort::{ExternalSort, Sorted};
use crate::store::row::Row;

/// Hands back its input in order.
///
/// The first row drains the whole input, because the last row to arrive can
/// be the first of the order. Rows past the budget go to `tmp/`, so a sort of
/// more rows than memory holds costs disk and not memory.
pub struct Sort<'a> {
    state: State<'a>,
    columns: Vec<ColumnDef>,
    cancel: CancelHandle,
}

/// Where a sort has got to. An error leaves it finished, because a statement
/// that failed is over.
enum State<'a> {
    Filling(Box<dyn Operator + 'a>, ExternalSort),
    Draining(Sorted),
    Done,
}

impl<'a> Sort<'a> {
    pub fn new(
        input: Box<dyn Operator + 'a>,
        cancel: CancelHandle,
        sort: ExternalSort,
    ) -> Sort<'a> {
        Sort {
            columns: input.schema().to_vec(),
            state: State::Filling(input, sort),
            cancel,
        }
    }
}

impl Operator for Sort<'_> {
    fn next(&mut self) -> Result<Option<Row>, DbError> {
        loop {
            // Taken out first, so a failure part way leaves nothing half
            // filled for a later call to read.
            match std::mem::replace(&mut self.state, State::Done) {
                State::Filling(mut input, mut sort) => {
                    // The input reads the flag as well, so a fill stops at
                    // the row it is on either way.
                    while let Some(row) = input.next()? {
                        sort.add(&row)?;
                    }
                    self.state = State::Draining(sort.finish(&self.cancel)?);
                }
                State::Draining(mut rows) => {
                    self.cancel.check()?;
                    let row = rows.next(&self.columns)?;
                    self.state = State::Draining(rows);
                    return Ok(row);
                }
                State::Done => return Ok(None),
            }
        }
    }

    fn schema(&self) -> &[ColumnDef] {
        &self.columns
    }
}
