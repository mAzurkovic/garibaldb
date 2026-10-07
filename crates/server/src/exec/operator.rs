//! One step of a plan.

use protocol::DbError;

use crate::catalog::ColumnDef;
use crate::store::row::Row;

/// One step of a plan. Each call hands back one row, so a result larger than
/// the memory of the server still flows through.
pub trait Operator {
    /// The next row, or none at the end.
    ///
    /// A failure is an error and not an end, because a short read would
    /// otherwise look like a table that holds nothing.
    fn next(&mut self) -> Result<Option<Row>, DbError>;

    /// The columns of the rows this hands back, which name them on the wire.
    fn schema(&self) -> &[ColumnDef];
}
