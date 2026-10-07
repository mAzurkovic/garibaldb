//! A condition against one row.
//!
//! Three outcomes, not two: a comparison that meets a null is neither true
//! nor false, and a row like that does not appear in a result.

use std::cmp::Ordering;

use protocol::{DbError, ErrorCode, Value};

use crate::catalog::{ColumnDef, named};
use crate::sql::ast::{CompareOp, Expr};
use crate::store::row::Row;

/// Whether a row satisfies a condition. None is neither true nor false.
pub fn truth(condition: &Expr, columns: &[ColumnDef], row: &Row) -> Result<Option<bool>, DbError> {
    match condition {
        Expr::Compare { left, op, right } => {
            let left = value(left, columns, row)?;
            let right = value(right, columns, row)?;
            Ok(left.compare(&right).map(|order| satisfies(*op, order)))
        }
        Expr::And(left, right) => Ok(both(
            truth(left, columns, row)?,
            truth(right, columns, row)?,
        )),
        Expr::Or(left, right) => Ok(either(
            truth(left, columns, row)?,
            truth(right, columns, row)?,
        )),
        Expr::Not(inner) => Ok(truth(inner, columns, row)?.map(|held| !held)),
        Expr::IsNull { operand, negated } => {
            let empty = value(operand, columns, row)? == Value::Null;
            Ok(Some(empty != *negated))
        }
        Expr::Column(_) | Expr::Literal(_) => match value(condition, columns, row)? {
            Value::Boolean(held) => Ok(Some(held)),
            Value::Null => Ok(None),
            _ => Err(named(
                ErrorCode::TypeMismatch,
                "a condition has to be true or false",
            )),
        },
    }
}

/// What one side of a comparison holds.
fn value(expr: &Expr, columns: &[ColumnDef], row: &Row) -> Result<Value, DbError> {
    match expr {
        Expr::Column(name) => {
            let at = columns
                .iter()
                .position(|column| column.name == *name)
                .ok_or_else(|| {
                    named(ErrorCode::UnknownColumn, format!("no column named {name}"))
                })?;
            Ok(row[at].clone())
        }
        Expr::Literal(value) => Ok(value.clone()),
        // A condition in parentheses stands where a value does.
        nested => Ok(match truth(nested, columns, row)? {
            Some(held) => Value::Boolean(held),
            None => Value::Null,
        }),
    }
}

fn satisfies(op: CompareOp, order: Ordering) -> bool {
    match op {
        CompareOp::Eq => order == Ordering::Equal,
        CompareOp::Ne => order != Ordering::Equal,
        CompareOp::Lt => order == Ordering::Less,
        CompareOp::Le => order != Ordering::Greater,
        CompareOp::Gt => order == Ordering::Greater,
        CompareOp::Ge => order != Ordering::Less,
    }
}

/// `AND` over three outcomes: false wins over not knowing.
fn both(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

/// `OR` over three outcomes: true wins over not knowing.
fn either(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use protocol::DataType;

    use super::*;
    use crate::sql::ast::Statement;
    use crate::sql::parser;

    /// The columns a condition is read against.
    fn columns() -> Vec<ColumnDef> {
        [
            ("id", DataType::Integer),
            ("label", DataType::Text),
            ("sold", DataType::Boolean),
        ]
        .into_iter()
        .map(|(name, ty)| ColumnDef {
            name: name.to_string(),
            ty,
            not_null: false,
        })
        .collect()
    }

    /// Whether a condition written as SQL holds for a row.
    fn holds(condition: &str, row: &Row) -> Option<bool> {
        let sql = format!("SELECT * FROM t WHERE {condition}");
        let Statement::Select {
            filter: Some(filter),
            ..
        } = parser::parse(&sql).expect("the statement parses")
        else {
            panic!("expected a select with a condition");
        };
        truth(&filter, &columns(), row).expect("the condition reads")
    }

    fn row(id: Value, label: Value, sold: Value) -> Row {
        vec![id, label, sold]
    }

    fn apple() -> Row {
        row(
            Value::Integer(1),
            Value::Text("apple".to_string()),
            Value::Boolean(true),
        )
    }

    #[test]
    fn every_comparison_reads_against_a_row() {
        let cases = [
            ("id = 1", Some(true)),
            ("id = 2", Some(false)),
            ("id != 2", Some(true)),
            ("id < 2", Some(true)),
            ("id <= 1", Some(true)),
            ("id > 0", Some(true)),
            ("id >= 2", Some(false)),
            ("label = 'apple'", Some(true)),
            ("label < 'banana'", Some(true)),
            ("sold = true", Some(true)),
        ];
        for (condition, want) in cases {
            assert_eq!(holds(condition, &apple()), want, "{condition}");
        }
    }

    #[test]
    fn a_literal_may_stand_on_either_side() {
        assert_eq!(holds("1 = id", &apple()), Some(true));
        assert_eq!(holds("2 > id", &apple()), Some(true));
    }

    #[test]
    fn a_comparison_with_nothing_is_neither_true_nor_false() {
        let empty = row(Value::Null, Value::Null, Value::Null);
        for condition in ["id = 1", "id != 1", "id < 1", "label = 'apple'"] {
            assert_eq!(holds(condition, &empty), None, "{condition}");
        }
    }

    #[test]
    fn and_takes_false_over_not_knowing() {
        let empty = row(Value::Null, Value::Null, Value::Null);
        assert_eq!(holds("id = 1 AND 1 = 2", &empty), Some(false));
        assert_eq!(holds("1 = 2 AND id = 1", &empty), Some(false));
        assert_eq!(holds("id = 1 AND 1 = 1", &empty), None);
        assert_eq!(holds("id = 1 AND id = 1", &apple()), Some(true));
    }

    #[test]
    fn or_takes_true_over_not_knowing() {
        let empty = row(Value::Null, Value::Null, Value::Null);
        assert_eq!(holds("id = 1 OR 1 = 1", &empty), Some(true));
        assert_eq!(holds("1 = 1 OR id = 1", &empty), Some(true));
        assert_eq!(holds("id = 1 OR 1 = 2", &empty), None);
        assert_eq!(holds("id = 2 OR id = 3", &apple()), Some(false));
    }

    #[test]
    fn not_turns_what_is_known_and_leaves_the_rest() {
        let empty = row(Value::Null, Value::Null, Value::Null);
        assert_eq!(holds("NOT id = 1", &apple()), Some(false));
        assert_eq!(holds("NOT id = 2", &apple()), Some(true));
        assert_eq!(holds("NOT id = 1", &empty), None);
    }

    #[test]
    fn asking_whether_a_value_is_there_is_always_answered() {
        let empty = row(Value::Null, Value::Null, Value::Null);
        assert_eq!(holds("id IS NULL", &empty), Some(true));
        assert_eq!(holds("id IS NOT NULL", &empty), Some(false));
        assert_eq!(holds("id IS NULL", &apple()), Some(false));
        assert_eq!(holds("id IS NOT NULL", &apple()), Some(true));
    }

    #[test]
    fn a_column_that_holds_a_truth_stands_as_a_condition() {
        assert_eq!(holds("sold", &apple()), Some(true));
        assert_eq!(
            holds(
                "sold",
                &row(Value::Integer(1), Value::Null, Value::Boolean(false))
            ),
            Some(false)
        );
        assert_eq!(
            holds("sold", &row(Value::Integer(1), Value::Null, Value::Null)),
            None
        );
        assert_eq!(holds("true", &apple()), Some(true));
    }

    #[test]
    fn a_condition_in_parentheses_stands_where_a_value_does() {
        assert_eq!(holds("(id = 1) = true", &apple()), Some(true));
        assert_eq!(holds("(id = 2) = true", &apple()), Some(false));
        let empty = row(Value::Null, Value::Null, Value::Null);
        assert_eq!(holds("(id = 1) IS NULL", &empty), Some(true));
    }

    #[test]
    fn a_condition_that_is_not_a_truth_is_an_error() {
        let sql = "SELECT * FROM t WHERE id";
        let Statement::Select {
            filter: Some(filter),
            ..
        } = parser::parse(sql).unwrap()
        else {
            panic!("expected a select");
        };
        let e = truth(&filter, &columns(), &apple()).err().unwrap();
        assert_eq!(e.code, ErrorCode::TypeMismatch);
    }

    #[test]
    fn a_column_the_table_has_not_got_is_an_error() {
        let sql = "SELECT * FROM t WHERE nothing = 1";
        let Statement::Select {
            filter: Some(filter),
            ..
        } = parser::parse(sql).unwrap()
        else {
            panic!("expected a select");
        };
        let e = truth(&filter, &columns(), &apple()).err().unwrap();
        assert_eq!(e.code, ErrorCode::UnknownColumn);
        assert!(e.message.contains("nothing"), "{e}");
    }
}
