//! The operators that take rows from the one below them.

use protocol::{DbError, ErrorCode};

use crate::catalog::{ColumnDef, named};
use crate::exec::eval;
use crate::exec::operator::Operator;
use crate::sql::ast::{Expr, Selection};
use crate::store::row::Row;

/// Keeps the rows whose condition is true. A condition that is neither true
/// nor false drops the row, which is what a comparison with a null gives.
pub struct Filter<'a> {
    input: Box<dyn Operator + 'a>,
    condition: Expr,
}

impl<'a> Filter<'a> {
    pub fn new(input: Box<dyn Operator + 'a>, condition: Expr) -> Filter<'a> {
        Filter { input, condition }
    }
}

impl Operator for Filter<'_> {
    fn next(&mut self) -> Result<Option<Row>, DbError> {
        while let Some(row) = self.input.next()? {
            if eval::truth(&self.condition, self.input.schema(), &row)? == Some(true) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    fn schema(&self) -> &[ColumnDef] {
        self.input.schema()
    }
}

/// Keeps the named columns, in the order they were named.
pub struct Project<'a> {
    input: Box<dyn Operator + 'a>,
    at: Vec<usize>,
    columns: Vec<ColumnDef>,
}

impl<'a> Project<'a> {
    pub fn new(
        input: Box<dyn Operator + 'a>,
        selection: &Selection,
    ) -> Result<Project<'a>, DbError> {
        let below = input.schema();
        let at: Vec<usize> = match selection {
            Selection::All => (0..below.len()).collect(),
            Selection::Columns(names) => names
                .iter()
                .map(|name| {
                    below
                        .iter()
                        .position(|column| column.name == *name)
                        .ok_or_else(|| {
                            named(ErrorCode::UnknownColumn, format!("no column named {name}"))
                        })
                })
                .collect::<Result<_, DbError>>()?,
        };
        let columns = at.iter().map(|index| below[*index].clone()).collect();
        Ok(Project { input, at, columns })
    }
}

impl Operator for Project<'_> {
    fn next(&mut self) -> Result<Option<Row>, DbError> {
        Ok(self
            .input
            .next()?
            .map(|row| self.at.iter().map(|index| row[*index].clone()).collect()))
    }

    fn schema(&self) -> &[ColumnDef] {
        &self.columns
    }
}

/// Stops after a count of rows.
pub struct Limit<'a> {
    input: Box<dyn Operator + 'a>,
    left: u64,
}

impl<'a> Limit<'a> {
    pub fn new(input: Box<dyn Operator + 'a>, rows: u64) -> Limit<'a> {
        Limit { input, left: rows }
    }
}

impl Operator for Limit<'_> {
    fn next(&mut self) -> Result<Option<Row>, DbError> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        self.input.next()
    }

    fn schema(&self) -> &[ColumnDef] {
        self.input.schema()
    }
}
