//! A statement into a tree of operators.

use std::ops::Bound;

use protocol::{DbError, Value};

use crate::catalog::TableDef;
use crate::exec::filter::{Filter, Limit, Project};
use crate::exec::operator::Operator;
use crate::exec::scan::Scan;
use crate::sql::ast::{CompareOp, Expr, Selection};
use crate::store::btree::BTree;
use crate::store::page::FileId;
use crate::store::pool::BufferPool;

/// The plan of a read: a scan, then the condition, then the count, then the
/// columns asked for.
pub fn plan<'a>(
    table: &TableDef,
    selection: &Selection,
    condition: Option<&Expr>,
    limit: Option<u64>,
    pool: &'a BufferPool,
    file: FileId,
) -> Result<Box<dyn Operator + 'a>, DbError> {
    let tree = BTree::open(pool, file, table);
    let (from, to) = key_bounds(table, condition);
    let mut plan: Box<dyn Operator + 'a> = Box::new(Scan::new(
        &tree,
        pool,
        file,
        table.columns.clone(),
        from,
        to,
    )?);
    if let Some(condition) = condition {
        plan = Box::new(Filter::new(plan, condition.clone()));
    }
    if let Some(rows) = limit {
        plan = Box::new(Limit::new(plan, rows));
    }
    Ok(Box::new(Project::new(plan, selection)?))
}

/// What the condition says about the primary key, as a range the tree can
/// narrow to.
///
/// Only a hint: the filter applies the whole condition anyway, so a bound this
/// misses costs a scan and never a wrong answer. Two bounds on one side keep
/// the last seen, which is sound because each one on its own holds.
fn key_bounds(table: &TableDef, condition: Option<&Expr>) -> (Bound<Value>, Bound<Value>) {
    let mut from = Bound::Unbounded;
    let mut to = Bound::Unbounded;
    let Some(condition) = condition else {
        return (from, to);
    };
    let key = &table.columns[table.pk_index].name;
    for part in conjuncts(condition) {
        let Expr::Compare { left, op, right } = part else {
            continue;
        };
        match (&**left, &**right) {
            (Expr::Column(name), Expr::Literal(value)) if name == key => {
                narrow(&mut from, &mut to, *op, value.clone());
            }
            // The literal may stand on the left, which reads the other way.
            (Expr::Literal(value), Expr::Column(name)) if name == key => {
                narrow(&mut from, &mut to, mirror(*op), value.clone());
            }
            _ => {}
        }
    }
    (from, to)
}

/// The conditions that an `AND` joins. Anything else is one condition, because
/// a bound taken out of an `OR` would leave rows behind.
fn conjuncts(condition: &Expr) -> Vec<&Expr> {
    match condition {
        Expr::And(left, right) => {
            let mut parts = conjuncts(left);
            parts.extend(conjuncts(right));
            parts
        }
        single => vec![single],
    }
}

fn narrow(from: &mut Bound<Value>, to: &mut Bound<Value>, op: CompareOp, value: Value) {
    match op {
        CompareOp::Eq => {
            *from = Bound::Included(value.clone());
            *to = Bound::Included(value);
        }
        CompareOp::Gt => *from = Bound::Excluded(value),
        CompareOp::Ge => *from = Bound::Included(value),
        CompareOp::Lt => *to = Bound::Excluded(value),
        CompareOp::Le => *to = Bound::Included(value),
        // Everything but one key, which no range leaves out.
        CompareOp::Ne => {}
    }
}

/// The comparison read from the other side.
fn mirror(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::Le => CompareOp::Ge,
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::Ge => CompareOp::Le,
        CompareOp::Eq => CompareOp::Eq,
        CompareOp::Ne => CompareOp::Ne,
    }
}

#[cfg(test)]
mod tests {
    use protocol::DataType;

    use super::*;
    use crate::catalog::ColumnDef;
    use crate::sql::ast::Statement;
    use crate::sql::parser;

    /// A table keyed by `id`, with a `label` beside it.
    fn table() -> TableDef {
        TableDef {
            id: 1,
            name: "item".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    ty: DataType::Integer,
                    not_null: true,
                },
                ColumnDef {
                    name: "label".to_string(),
                    ty: DataType::Text,
                    not_null: false,
                },
            ],
            pk_index: 0,
        }
    }

    /// The range that the condition of a `WHERE` narrows the key to.
    fn bounds(condition: Option<&str>) -> (Bound<Value>, Bound<Value>) {
        let sql = match condition {
            None => "SELECT * FROM item".to_string(),
            Some(condition) => format!("SELECT * FROM item WHERE {condition}"),
        };
        let Statement::Select { filter, .. } = parser::parse(&sql).expect("it parses") else {
            panic!("expected a select");
        };
        key_bounds(&table(), filter.as_ref())
    }

    fn at(n: i64) -> Bound<Value> {
        Bound::Included(Value::Integer(n))
    }

    fn past(n: i64) -> Bound<Value> {
        Bound::Excluded(Value::Integer(n))
    }

    #[test]
    fn no_condition_leaves_the_range_open() {
        assert_eq!(bounds(None), (Bound::Unbounded, Bound::Unbounded));
    }

    #[test]
    fn a_comparison_on_the_key_narrows_the_range() {
        let cases = [
            ("id = 5", (at(5), at(5))),
            ("id > 5", (past(5), Bound::Unbounded)),
            ("id >= 5", (at(5), Bound::Unbounded)),
            ("id < 5", (Bound::Unbounded, past(5))),
            ("id <= 5", (Bound::Unbounded, at(5))),
        ];
        for (condition, want) in cases {
            assert_eq!(bounds(Some(condition)), want, "{condition}");
        }
    }

    #[test]
    fn the_literal_may_stand_on_the_left() {
        let cases = [
            ("5 = id", (at(5), at(5))),
            ("5 < id", (past(5), Bound::Unbounded)),
            ("5 <= id", (at(5), Bound::Unbounded)),
            ("5 > id", (Bound::Unbounded, past(5))),
            ("5 >= id", (Bound::Unbounded, at(5))),
        ];
        for (condition, want) in cases {
            assert_eq!(bounds(Some(condition)), want, "{condition}");
        }
    }

    #[test]
    fn every_key_but_one_is_no_range_at_all() {
        assert_eq!(
            bounds(Some("id != 5")),
            (Bound::Unbounded, Bound::Unbounded)
        );
        assert_eq!(
            bounds(Some("5 != id")),
            (Bound::Unbounded, Bound::Unbounded)
        );
    }

    #[test]
    fn conditions_joined_by_and_narrow_both_ends() {
        assert_eq!(bounds(Some("id > 2 AND id < 8")), (past(2), past(8)));
        assert_eq!(
            bounds(Some("id >= 2 AND label = 'a' AND id <= 8")),
            (at(2), at(8))
        );
    }

    #[test]
    fn a_condition_on_another_column_narrows_nothing() {
        assert_eq!(
            bounds(Some("label = 'a'")),
            (Bound::Unbounded, Bound::Unbounded)
        );
        assert_eq!(
            bounds(Some("label = 'a' AND label = 'b'")),
            (Bound::Unbounded, Bound::Unbounded)
        );
    }

    #[test]
    fn a_condition_an_or_joins_narrows_nothing() {
        // One side of an `OR` says nothing about which keys the result holds.
        assert_eq!(
            bounds(Some("id = 1 OR id = 9")),
            (Bound::Unbounded, Bound::Unbounded)
        );
        assert_eq!(
            bounds(Some("NOT id = 1")),
            (Bound::Unbounded, Bound::Unbounded)
        );
        assert_eq!(
            bounds(Some("id IS NULL")),
            (Bound::Unbounded, Bound::Unbounded)
        );
    }

    #[test]
    fn a_comparison_of_two_columns_narrows_nothing() {
        assert_eq!(
            bounds(Some("id = label")),
            (Bound::Unbounded, Bound::Unbounded)
        );
    }
}
