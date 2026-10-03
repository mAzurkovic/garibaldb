//! The tree that the parser builds.
//!
//! A node holds no behaviour. Milestone 8 adds the executor that walks it.

use std::fmt;

use protocol::{DataType, Value};

/// One of the six comparisons. See [FR33].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CompareOp {
    pub fn as_str(self) -> &'static str {
        match self {
            CompareOp::Eq => "=",
            CompareOp::Ne => "!=",
            CompareOp::Lt => "<",
            CompareOp::Le => "<=",
            CompareOp::Gt => ">",
            CompareOp::Ge => ">=",
        }
    }
}

impl fmt::Display for CompareOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A condition. See [FR32] to [FR35].
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column(String),
    Literal(Value),
    Compare {
        left: Box<Expr>,
        op: CompareOp,
        right: Box<Expr>,
    },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    /// `IS NULL`, or `IS NOT NULL` when negated. See [FR35].
    IsNull {
        operand: Box<Expr>,
        negated: bool,
    },
}

/// One column of a `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: String,
    pub ty: DataType,
    pub primary_key: bool,
    pub not_null: bool,
}

/// Which columns a `SELECT` returns. See [FR31].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    All,
    Columns(Vec<String>),
}

/// One `col = value` of an `UPDATE`.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: String,
    pub value: Value,
}

/// The sort of a `SELECT`. See [FR37].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    pub column: String,
    pub descending: bool,
}

/// One statement. The eleven forms that the grammar holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateDatabase {
        name: String,
    },
    DropDatabase {
        name: String,
    },
    CreateTable {
        name: String,
        columns: Vec<ColumnSpec>,
    },
    DropTable {
        name: String,
    },
    Insert {
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    Select {
        table: String,
        selection: Selection,
        filter: Option<Expr>,
        order_by: Option<OrderBy>,
        limit: Option<u64>,
    },
    Update {
        table: String,
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
    /// `BEGIN`, or `BEGIN READ ONLY` when read only.
    Begin {
        read_only: bool,
    },
    Commit,
    Rollback,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_comparison_shows_its_operator() {
        let cases = [
            (CompareOp::Eq, "="),
            (CompareOp::Ne, "!="),
            (CompareOp::Lt, "<"),
            (CompareOp::Le, "<="),
            (CompareOp::Gt, ">"),
            (CompareOp::Ge, ">="),
        ];
        for (op, text) in cases {
            assert_eq!(op.as_str(), text);
            assert_eq!(op.to_string(), text);
        }
    }
}
