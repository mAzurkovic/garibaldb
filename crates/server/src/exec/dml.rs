//! Running the statements that read and change rows.
//!
//! A write checks everything it can before it writes anything, because there
//! is no log to undo with yet. A walk that changes rows re-seeks after each
//! one, so it never leans on a cursor that its own write has moved.

use std::ops::Bound;

use protocol::{DataType, DbError, ErrorCode, Value};

use crate::catalog::registry::Registry;
use crate::catalog::{ColumnDef, Database, TableDef, ddl, named};
use crate::exec::operator::Operator;
use crate::exec::{eval, planner};
use crate::sql::ast::{Assignment, Expr, Selection, Statement};
use crate::store::btree::BTree;
use crate::store::page::FileId;
use crate::store::pool::BufferPool;
use crate::store::row::{self, Row};
use crate::wal::checkpoint;

/// What a statement did.
pub enum Answer<'a> {
    /// Rows to write out, and the columns that name them.
    Rows(Box<dyn Operator + 'a>),
    /// A statement that changed the schema or some rows.
    Changed { kind: &'static str, rows: u64 },
}

/// Runs one statement.
///
/// A statement that changed anything writes its pages before it answers.
/// Every statement stands alone until milestone 10, so one that reported
/// success has to have reached the file. Nothing here is atomic: the WAL in
/// milestone 9 is what makes a half-written change impossible.
pub fn run<'a>(
    statement: &Statement,
    db: &Database,
    registry: &'a Registry,
) -> Result<Answer<'a>, DbError> {
    let answer = dispatch(statement, db, registry)?;
    if let Answer::Changed { .. } = &answer {
        registry.pool().commit(&db.wal)?;
        if db.wal.end() > checkpoint::THRESHOLD {
            checkpoint::run(registry.pool(), &db.wal)?;
        }
    }
    Ok(answer)
}

fn dispatch<'a>(
    statement: &Statement,
    db: &Database,
    registry: &'a Registry,
) -> Result<Answer<'a>, DbError> {
    match statement {
        Statement::CreateDatabase { name } => {
            registry.create(name)?;
            Ok(Answer::Changed {
                kind: "CREATE DATABASE",
                rows: 0,
            })
        }
        Statement::DropDatabase { name } => {
            registry.drop_database(name)?;
            Ok(Answer::Changed {
                kind: "DROP DATABASE",
                rows: 0,
            })
        }
        Statement::CreateTable { name, columns } => {
            ddl::create_table(db, name, columns)?;
            Ok(Answer::Changed {
                kind: "CREATE TABLE",
                rows: 0,
            })
        }
        Statement::DropTable { name } => {
            ddl::drop_table(db, name)?;
            Ok(Answer::Changed {
                kind: "DROP TABLE",
                rows: 0,
            })
        }
        Statement::Insert {
            table,
            columns,
            rows,
        } => Ok(Answer::Changed {
            kind: "INSERT",
            rows: insert(db, registry, table, columns, rows)?,
        }),
        Statement::Select {
            table,
            selection,
            filter,
            order_by,
            limit,
        } => {
            if order_by.is_some() {
                return Err(named(
                    ErrorCode::SyntaxError,
                    "ORDER BY is not supported yet",
                ));
            }
            select(db, registry, table, selection, filter.as_ref(), *limit).map(Answer::Rows)
        }
        Statement::Update {
            table,
            assignments,
            filter,
        } => Ok(Answer::Changed {
            kind: "UPDATE",
            rows: update(db, registry, table, assignments, filter.as_ref())?,
        }),
        Statement::Delete { table, filter } => Ok(Answer::Changed {
            kind: "DELETE",
            rows: delete(db, registry, table, filter.as_ref())?,
        }),
        // A transaction is milestone 10's. Refusing BEGIN is what keeps a
        // client from believing it holds one.
        Statement::Begin { .. } | Statement::Commit | Statement::Rollback => Err(named(
            ErrorCode::TxnAborted,
            "the server holds no transaction yet",
        )),
    }
}

/// Adds rows. Every row is checked before any is written, so a statement that
/// is refused leaves the table as it was.
fn insert(
    db: &Database,
    registry: &Registry,
    name: &str,
    named_columns: &[String],
    given: &[Vec<Value>],
) -> Result<u64, DbError> {
    let table = table_of(db, name)?;
    let file = registry.table_file(db, &table)?;
    let pool = registry.pool();
    let at = positions(&table, named_columns)?;

    let mut rows = Vec::with_capacity(given.len());
    for values in given {
        if values.len() != named_columns.len() {
            return Err(named(
                ErrorCode::SyntaxError,
                format!(
                    "the row holds {} values for {} columns",
                    values.len(),
                    named_columns.len()
                ),
            ));
        }
        let mut row = vec![Value::Null; table.columns.len()];
        for (index, value) in at.iter().zip(values) {
            row[*index] = value.clone();
        }
        check(&table.columns, &row)?;
        rows.push(row);
    }

    // A key has to be new to the table and to the other rows of this
    // statement. The list is one statement long, so the scan over it is
    // short; a statement of many thousands of rows would want a set.
    let tree = BTree::open(pool, file, &table);
    let mut keys: Vec<Value> = Vec::with_capacity(rows.len());
    for row in &rows {
        let key = &row[table.pk_index];
        let taken = tree.get(key)?.is_some() || keys.iter().any(|seen| same(seen, key));
        if taken {
            return Err(duplicate());
        }
        keys.push(key.clone());
    }

    for row in &rows {
        let bytes = row::store(pool, file, &table.columns, row)?;
        tree.insert(&bytes)?;
    }
    Ok(rows.len() as u64)
}

/// The plan of a read.
fn select<'a>(
    db: &Database,
    registry: &'a Registry,
    name: &str,
    selection: &Selection,
    condition: Option<&Expr>,
    limit: Option<u64>,
) -> Result<Box<dyn Operator + 'a>, DbError> {
    let table = table_of(db, name)?;
    let file = registry.table_file(db, &table)?;
    planner::plan(&table, selection, condition, limit, registry.pool(), file)
}

/// Changes every row the condition takes.
fn update(
    db: &Database,
    registry: &Registry,
    name: &str,
    assignments: &[Assignment],
    condition: Option<&Expr>,
) -> Result<u64, DbError> {
    let table = table_of(db, name)?;
    let file = registry.table_file(db, &table)?;
    let pool = registry.pool();
    let tree = BTree::open(pool, file, &table);

    // Every value assigned is a literal, so its type is checked once here
    // rather than once for each row. A row that was good stays good.
    let mut sets = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let at = position(&table, &assignment.column)?;
        check_value(&table.columns[at], &assignment.value)?;
        sets.push((at, assignment.value.clone()));
    }

    let mut changed = 0;
    let mut last = None;
    while let Some((key, row)) = next_match(&tree, pool, file, &table, condition, last.as_ref())? {
        let mut new = row;
        for (at, value) in &sets {
            new[*at] = value.clone();
        }
        let moved = &new[table.pk_index];
        if !same(moved, &key) && tree.get(moved)?.is_some() {
            return Err(duplicate());
        }
        let bytes = row::store(pool, file, &table.columns, &new)?;
        let old = taken(&tree, &key)?;
        row::free(pool, file, &table.columns, &old)?;
        tree.insert(&bytes)?;
        changed += 1;
        last = Some(key);
    }
    Ok(changed)
}

/// Deletes every row the condition takes, and the chains they held.
fn delete(
    db: &Database,
    registry: &Registry,
    name: &str,
    condition: Option<&Expr>,
) -> Result<u64, DbError> {
    let table = table_of(db, name)?;
    let file = registry.table_file(db, &table)?;
    let pool = registry.pool();
    let tree = BTree::open(pool, file, &table);

    let mut gone = 0;
    let mut last = None;
    while let Some((key, _)) = next_match(&tree, pool, file, &table, condition, last.as_ref())? {
        let old = taken(&tree, &key)?;
        row::free(pool, file, &table.columns, &old)?;
        gone += 1;
        last = Some(key);
    }
    Ok(gone)
}

/// The first row after `last` that the condition takes, and its key.
///
/// A fresh cursor each time, because the write that followed the last row may
/// have split a page or moved a slot under the old one.
fn next_match(
    tree: &BTree<'_>,
    pool: &BufferPool,
    file: FileId,
    table: &TableDef,
    condition: Option<&Expr>,
    last: Option<&Value>,
) -> Result<Option<(Value, Row)>, DbError> {
    let from = match last {
        None => Bound::Unbounded,
        Some(key) => Bound::Excluded(key.clone()),
    };
    let mut cursor = tree.cursor(from, Bound::Unbounded)?;
    while let Some(bytes) = cursor.next()? {
        let row = row::load(pool, file, &table.columns, &bytes)?;
        let takes = match condition {
            None => true,
            Some(condition) => eval::truth(condition, &table.columns, &row)? == Some(true),
        };
        if takes {
            return Ok(Some((row[table.pk_index].clone(), row)));
        }
    }
    Ok(None)
}

/// Takes a row out of the tree, which the walk just read and so is there.
fn taken(tree: &BTree<'_>, key: &Value) -> Result<Vec<u8>, DbError> {
    tree.delete(key)?.ok_or_else(|| {
        named(
            ErrorCode::StorageFull,
            "a row went before it could be changed",
        )
    })
}

fn table_of(db: &Database, name: &str) -> Result<TableDef, DbError> {
    db.catalog
        .lock()
        .expect("the catalog lock holds")
        .table(name)
        .cloned()
}

/// Where each named column sits in the table.
fn positions(table: &TableDef, names: &[String]) -> Result<Vec<usize>, DbError> {
    let mut at = Vec::with_capacity(names.len());
    for name in names {
        let index = position(table, name)?;
        if at.contains(&index) {
            return Err(named(
                ErrorCode::SyntaxError,
                format!("the column {name} is named twice"),
            ));
        }
        at.push(index);
    }
    Ok(at)
}

fn position(table: &TableDef, name: &str) -> Result<usize, DbError> {
    table
        .columns
        .iter()
        .position(|column| column.name == name)
        .ok_or_else(|| {
            named(
                ErrorCode::UnknownColumn,
                format!("no column named {name} in {}", table.name),
            )
        })
}

/// Whether a row may be stored: a value of the right type in each column, and
/// nothing empty where a value is required.
fn check(columns: &[ColumnDef], row: &Row) -> Result<(), DbError> {
    for (column, value) in columns.iter().zip(row) {
        check_value(column, value)?;
    }
    Ok(())
}

fn check_value(column: &ColumnDef, value: &Value) -> Result<(), DbError> {
    let fits = match (column.ty, value) {
        (_, Value::Null) => {
            return match column.not_null {
                true => Err(named(
                    ErrorCode::NotNullViolation,
                    format!("the column {} holds a value for every row", column.name),
                )),
                false => Ok(()),
            };
        }
        (DataType::Integer, Value::Integer(_)) => true,
        (DataType::Text, Value::Text(_)) => true,
        (DataType::Boolean, Value::Boolean(_)) => true,
        (DataType::Decimal { p, s }, Value::Decimal(decimal)) => decimal.fits(p, s),
        _ => false,
    };
    match fits {
        true => Ok(()),
        false => Err(named(
            ErrorCode::TypeMismatch,
            format!("the column {} holds {}", column.name, column.ty),
        )),
    }
}

/// Two keys that stand for the same key. A decimal compares by value, so
/// `12.20` and `12.2` are one key.
fn same(left: &Value, right: &Value) -> bool {
    left.compare(right) == Some(std::cmp::Ordering::Equal)
}

fn duplicate() -> DbError {
    named(
        ErrorCode::DuplicateKey,
        "a row with this primary key is already there",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::registry::Connected;
    use crate::catalog::testing::Dir;
    use crate::sql::parser;

    /// A registry with one database, and that database open.
    fn shop(label: &str) -> (Dir, std::sync::Arc<Registry>, Connected) {
        let dir = Dir::new(label);
        let (registry, held) = dir.shop();
        (dir, registry, held)
    }

    /// Runs a statement and returns its rows, which a write leaves empty.
    fn rows(sql: &str, held: &Connected, registry: &Registry) -> Result<Vec<Row>, DbError> {
        let statement = parser::parse(sql).expect("the statement parses");
        match run(&statement, &held.db, registry)? {
            Answer::Changed { .. } => Ok(Vec::new()),
            Answer::Rows(mut plan) => {
                let mut found = Vec::new();
                while let Some(row) = plan.next()? {
                    found.push(row);
                }
                Ok(found)
            }
        }
    }

    /// Runs a statement and returns how many rows it changed.
    fn changed(sql: &str, held: &Connected, registry: &Registry) -> Result<u64, DbError> {
        let statement = parser::parse(sql).expect("the statement parses");
        match run(&statement, &held.db, registry)? {
            Answer::Changed { rows, .. } => Ok(rows),
            Answer::Rows(_) => panic!("{sql} reads rows"),
        }
    }

    /// The rows of a read, each written as its values joined by a space.
    fn shown(sql: &str, held: &Connected, registry: &Registry) -> Vec<String> {
        rows(sql, held, registry)
            .expect("the statement runs")
            .iter()
            .map(|row| {
                row.iter()
                    .map(|value| match value {
                        Value::Integer(n) => n.to_string(),
                        Value::Text(text) => text.clone(),
                        Value::Boolean(held) => held.to_string(),
                        Value::Decimal(d) => d.to_string(),
                        Value::Null => "NULL".to_string(),
                    })
                    .collect::<Vec<String>>()
                    .join(" ")
            })
            .collect()
    }

    /// A table of a key, a required label, and an optional price.
    fn item(held: &Connected, registry: &Registry) {
        changed(
            "CREATE TABLE item (id INTEGER PRIMARY KEY, label TEXT NOT NULL, price DECIMAL(10, 2))",
            held,
            registry,
        )
        .expect("the table is made");
    }

    #[test]
    fn a_schema_change_names_its_kind() {
        let (_dir, registry, held) = shop("ddl");
        for (sql, kind) in [
            ("CREATE DATABASE other", "CREATE DATABASE"),
            ("DROP DATABASE other", "DROP DATABASE"),
            ("CREATE TABLE t (a INTEGER PRIMARY KEY)", "CREATE TABLE"),
            ("DROP TABLE t", "DROP TABLE"),
        ] {
            let statement = parser::parse(sql).unwrap();
            let Answer::Changed { kind: got, rows } = run(&statement, &held.db, &registry).unwrap()
            else {
                panic!("{sql} reads rows");
            };
            assert_eq!((got, rows), (kind, 0), "{sql}");
        }
    }

    #[test]
    fn rows_go_in_and_come_back() {
        let (_dir, registry, held) = shop("round-trip");
        item(&held, &registry);
        assert_eq!(
            changed(
                "INSERT INTO item (id, label, price) VALUES (1, 'apple', 1.50)",
                &held,
                &registry
            )
            .unwrap(),
            1
        );
        assert_eq!(
            changed(
                "INSERT INTO item (id, label) VALUES (2, 'pear'), (3, 'plum')",
                &held,
                &registry
            )
            .unwrap(),
            2
        );
        assert_eq!(
            shown("SELECT * FROM item", &held, &registry),
            vec!["1 apple 1.50", "2 pear NULL", "3 plum NULL"]
        );
    }

    #[test]
    fn the_columns_asked_for_come_back_in_the_order_asked() {
        let (_dir, registry, held) = shop("project");
        item(&held, &registry);
        changed(
            "INSERT INTO item (id, label) VALUES (1, 'apple')",
            &held,
            &registry,
        )
        .unwrap();
        assert_eq!(
            shown("SELECT label, id FROM item", &held, &registry),
            vec!["apple 1"]
        );
        assert_eq!(
            rows("SELECT nothing FROM item", &held, &registry)
                .err()
                .unwrap()
                .code,
            ErrorCode::UnknownColumn
        );
    }

    #[test]
    fn a_condition_and_a_count_narrow_a_read() {
        let (_dir, registry, held) = shop("narrow");
        item(&held, &registry);
        for id in 1..=5 {
            changed(
                &format!("INSERT INTO item (id, label) VALUES ({id}, 'a')"),
                &held,
                &registry,
            )
            .unwrap();
        }
        assert_eq!(
            shown("SELECT id FROM item WHERE id >= 3", &held, &registry),
            vec!["3", "4", "5"]
        );
        assert_eq!(
            shown("SELECT id FROM item WHERE id = 2", &held, &registry),
            vec!["2"]
        );
        assert_eq!(
            shown("SELECT id FROM item LIMIT 2", &held, &registry),
            vec!["1", "2"]
        );
        assert_eq!(
            shown("SELECT id FROM item WHERE id > 1 LIMIT 2", &held, &registry),
            vec!["2", "3"]
        );
        assert!(shown("SELECT id FROM item WHERE id > 9", &held, &registry).is_empty());
    }

    #[test]
    fn a_key_the_table_holds_is_refused_and_nothing_is_written() {
        let (_dir, registry, held) = shop("duplicate");
        item(&held, &registry);
        changed(
            "INSERT INTO item (id, label) VALUES (1, 'apple')",
            &held,
            &registry,
        )
        .unwrap();
        assert_eq!(
            changed(
                "INSERT INTO item (id, label) VALUES (1, 'again')",
                &held,
                &registry
            )
            .err()
            .unwrap()
            .code,
            ErrorCode::DuplicateKey
        );
        // A key that repeats inside one statement, caught before any write.
        assert_eq!(
            changed(
                "INSERT INTO item (id, label) VALUES (2, 'two'), (2, 'two again')",
                &held,
                &registry
            )
            .err()
            .unwrap()
            .code,
            ErrorCode::DuplicateKey
        );
        assert_eq!(shown("SELECT id FROM item", &held, &registry), vec!["1"]);
    }

    #[test]
    fn a_row_that_breaks_a_rule_of_its_table_is_refused() {
        let (_dir, registry, held) = shop("rules");
        item(&held, &registry);
        let cases = [
            (
                "INSERT INTO item (id) VALUES (1)",
                ErrorCode::NotNullViolation,
            ),
            (
                "INSERT INTO item (id, label) VALUES (1, NULL)",
                ErrorCode::NotNullViolation,
            ),
            (
                "INSERT INTO item (id, label) VALUES ('one', 'a')",
                ErrorCode::TypeMismatch,
            ),
            (
                "INSERT INTO item (id, label) VALUES (1, 2)",
                ErrorCode::TypeMismatch,
            ),
            (
                "INSERT INTO item (id, label, price) VALUES (1, 'a', 1.555)",
                ErrorCode::TypeMismatch,
            ),
            (
                "INSERT INTO item (id, nothing) VALUES (1, 'a')",
                ErrorCode::UnknownColumn,
            ),
            (
                "INSERT INTO item (id, id) VALUES (1, 2)",
                ErrorCode::SyntaxError,
            ),
            (
                "INSERT INTO item (id, label) VALUES (1)",
                ErrorCode::SyntaxError,
            ),
            ("INSERT INTO other (id) VALUES (1)", ErrorCode::UnknownTable),
        ];
        for (sql, code) in cases {
            assert_eq!(
                changed(sql, &held, &registry).err().unwrap().code,
                code,
                "{sql}"
            );
        }
        assert!(shown("SELECT id FROM item", &held, &registry).is_empty());
    }

    #[test]
    fn a_change_reaches_every_row_that_matches_and_no_other() {
        let (_dir, registry, held) = shop("update");
        item(&held, &registry);
        changed(
            "INSERT INTO item (id, label) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            &held,
            &registry,
        )
        .unwrap();
        assert_eq!(
            changed(
                "UPDATE item SET label = 'x' WHERE id >= 2",
                &held,
                &registry
            )
            .unwrap(),
            2
        );
        assert_eq!(
            shown("SELECT id, label FROM item", &held, &registry),
            vec!["1 a", "2 x", "3 x"]
        );
        // No condition reaches all of them.
        assert_eq!(
            changed("UPDATE item SET label = 'y'", &held, &registry).unwrap(),
            3
        );
        assert_eq!(
            shown("SELECT label FROM item", &held, &registry),
            vec!["y", "y", "y"]
        );
    }

    #[test]
    fn a_change_that_breaks_a_rule_is_refused_before_any_row_moves() {
        let (_dir, registry, held) = shop("update-rules");
        item(&held, &registry);
        changed(
            "INSERT INTO item (id, label) VALUES (1, 'a'), (2, 'b')",
            &held,
            &registry,
        )
        .unwrap();
        let cases = [
            ("UPDATE item SET label = NULL", ErrorCode::NotNullViolation),
            ("UPDATE item SET id = 'one'", ErrorCode::TypeMismatch),
            ("UPDATE item SET nothing = 1", ErrorCode::UnknownColumn),
            ("UPDATE other SET id = 1", ErrorCode::UnknownTable),
        ];
        for (sql, code) in cases {
            assert_eq!(
                changed(sql, &held, &registry).err().unwrap().code,
                code,
                "{sql}"
            );
        }
        assert_eq!(
            shown("SELECT id, label FROM item", &held, &registry),
            vec!["1 a", "2 b"]
        );
    }

    #[test]
    fn a_primary_key_moves_to_a_key_nothing_holds() {
        let (_dir, registry, held) = shop("move-key");
        item(&held, &registry);
        changed(
            "INSERT INTO item (id, label) VALUES (1, 'a'), (2, 'b')",
            &held,
            &registry,
        )
        .unwrap();
        assert_eq!(
            changed("UPDATE item SET id = 9 WHERE id = 1", &held, &registry).unwrap(),
            1
        );
        assert_eq!(
            shown("SELECT id, label FROM item", &held, &registry),
            vec!["2 b", "9 a"]
        );
        // Onto a key another row holds, it is refused.
        assert_eq!(
            changed("UPDATE item SET id = 2 WHERE id = 9", &held, &registry)
                .err()
                .unwrap()
                .code,
            ErrorCode::DuplicateKey
        );
    }

    #[test]
    fn a_delete_reaches_every_row_that_matches() {
        let (_dir, registry, held) = shop("delete");
        item(&held, &registry);
        changed(
            "INSERT INTO item (id, label) VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            &held,
            &registry,
        )
        .unwrap();
        assert_eq!(
            changed("DELETE FROM item WHERE id = 2", &held, &registry).unwrap(),
            1
        );
        assert_eq!(
            shown("SELECT id FROM item", &held, &registry),
            vec!["1", "3"]
        );
        assert_eq!(changed("DELETE FROM item", &held, &registry).unwrap(), 2);
        assert!(shown("SELECT id FROM item", &held, &registry).is_empty());
        assert_eq!(
            changed("DELETE FROM other", &held, &registry)
                .err()
                .unwrap()
                .code,
            ErrorCode::UnknownTable
        );
    }

    #[test]
    fn a_value_too_large_for_a_record_lives_in_a_chain_and_goes_with_its_row() {
        let (_dir, registry, held) = shop("chain");
        changed(
            "CREATE TABLE note (id INTEGER PRIMARY KEY, body TEXT NOT NULL)",
            &held,
            &registry,
        )
        .unwrap();
        let long = "x".repeat(40_000);
        changed(
            &format!("INSERT INTO note (id, body) VALUES (1, '{long}')"),
            &held,
            &registry,
        )
        .unwrap();
        let read = rows("SELECT body FROM note", &held, &registry).unwrap();
        assert_eq!(read, vec![vec![Value::Text(long.clone())]]);

        // A change swaps one chain for another, and a delete takes it away.
        changed(
            "UPDATE note SET body = 'short' WHERE id = 1",
            &held,
            &registry,
        )
        .unwrap();
        assert_eq!(
            shown("SELECT body FROM note", &held, &registry),
            vec!["short"]
        );
        changed("DELETE FROM note", &held, &registry).unwrap();
        assert!(shown("SELECT id FROM note", &held, &registry).is_empty());
    }

    #[test]
    fn sorting_waits_for_the_sort_that_can_do_it() {
        let (_dir, registry, held) = shop("order-by");
        item(&held, &registry);
        let e = rows("SELECT id FROM item ORDER BY label", &held, &registry)
            .err()
            .unwrap();
        assert_eq!(e.code, ErrorCode::SyntaxError);
        assert!(e.message.contains("not supported yet"), "{e}");
    }

    #[test]
    fn a_transaction_statement_is_refused() {
        let (_dir, registry, held) = shop("transactions");
        for sql in ["BEGIN", "BEGIN READ ONLY", "COMMIT", "ROLLBACK"] {
            let e = changed(sql, &held, &registry).err().unwrap();
            assert_eq!(e.code, ErrorCode::TxnAborted, "{sql}");
            assert!(e.message.contains("no transaction yet"), "{e}");
        }
    }

    #[test]
    fn a_read_names_the_columns_it_hands_back() {
        let (_dir, registry, held) = shop("schema");
        item(&held, &registry);
        let statement = parser::parse("SELECT label, id FROM item").unwrap();
        let Answer::Rows(plan) = run(&statement, &held.db, &registry).unwrap() else {
            panic!("expected rows");
        };
        let names: Vec<&str> = plan.schema().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["label", "id"]);

        let statement = parser::parse("SELECT * FROM item").unwrap();
        let Answer::Rows(plan) = run(&statement, &held.db, &registry).unwrap() else {
            panic!("expected rows");
        };
        let names: Vec<&str> = plan.schema().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "label", "price"]);
    }

    #[test]
    fn a_column_of_truths_holds_one_and_reads_back() {
        let (_dir, registry, held) = shop("boolean");
        changed(
            "CREATE TABLE flag (id INTEGER PRIMARY KEY, sold BOOLEAN)",
            &held,
            &registry,
        )
        .unwrap();
        changed(
            "INSERT INTO flag (id, sold) VALUES (1, true), (2, false), (3, NULL)",
            &held,
            &registry,
        )
        .unwrap();
        assert_eq!(
            shown("SELECT id, sold FROM flag", &held, &registry),
            vec!["1 true", "2 false", "3 NULL"]
        );
        assert_eq!(
            shown("SELECT id FROM flag WHERE sold", &held, &registry),
            vec!["1"]
        );
        assert_eq!(
            changed(
                "INSERT INTO flag (id, sold) VALUES (4, 1)",
                &held,
                &registry
            )
            .err()
            .unwrap()
            .code,
            ErrorCode::TypeMismatch
        );
    }

    #[test]
    fn the_log_keeps_what_came_before_and_is_not_emptied_by_each_statement() {
        let (_dir, registry, held) = shop("log-growth");
        item(&held, &registry);
        let after_create = held.db.wal.end();
        assert!(after_create > 0, "the table reached the log");

        changed(
            "INSERT INTO item (id, label) VALUES (1, 'a')",
            &held,
            &registry,
        )
        .unwrap();
        assert!(
            held.db.wal.end() > after_create,
            "a checkpoint runs on size, not on every statement"
        );
    }

    #[test]
    fn a_read_of_a_table_that_is_not_there_is_an_error() {
        let (_dir, registry, held) = shop("no-table");
        assert_eq!(
            rows("SELECT * FROM item", &held, &registry)
                .err()
                .unwrap()
                .code,
            ErrorCode::UnknownTable
        );
    }

    #[test]
    fn two_keys_of_the_same_value_written_differently_are_one_key() {
        let (_dir, registry, held) = shop("decimal-key");
        changed(
            "CREATE TABLE priced (price DECIMAL(10, 2) PRIMARY KEY, label TEXT)",
            &held,
            &registry,
        )
        .unwrap();
        changed(
            "INSERT INTO priced (price, label) VALUES (1.50, 'a')",
            &held,
            &registry,
        )
        .unwrap();
        assert_eq!(
            changed(
                "INSERT INTO priced (price, label) VALUES (1.5, 'b')",
                &held,
                &registry
            )
            .err()
            .unwrap()
            .code,
            ErrorCode::DuplicateKey
        );
        // And a change that lands on the same value is the same clash.
        changed(
            "INSERT INTO priced (price, label) VALUES (2.00, 'b')",
            &held,
            &registry,
        )
        .unwrap();
        assert_eq!(
            changed(
                "UPDATE priced SET price = 1.5 WHERE price = 2.00",
                &held,
                &registry
            )
            .err()
            .unwrap()
            .code,
            ErrorCode::DuplicateKey
        );
    }
}
