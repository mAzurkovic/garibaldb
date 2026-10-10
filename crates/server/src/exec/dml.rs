//! Running the statements that read and change rows.
//!
//! A write checks everything it can before it writes anything. A walk that
//! changes rows re-seeks after each one, so it never leans on a cursor that
//! its own write has moved.
//!
//! Every change runs inside a transaction: the one its connection opened, or
//! one that lasts the statement. The write lock of the database is held for
//! as long as that transaction, so two writers never overlap.

use std::ops::Bound;
use std::sync::Arc;

use protocol::{DataType, DbError, ErrorCode, Value};

use crate::cancel::CancelHandle;
use crate::catalog::registry::Registry;
use crate::catalog::{ColumnDef, Database, TableDef, ddl, named};
use crate::exec::operator::Operator;
use crate::exec::planner::Reading;
use crate::exec::{eval, planner};
use crate::sql::ast::{Assignment, Expr, Statement};
use crate::store::btree::{BTree, Direction};
use crate::store::page::FileId;
use crate::store::pool::{BufferPool, Mark};
use crate::store::row::{self, Row};
use crate::txn::manager::{Lease, Transaction, TxnKind};
use crate::wal::checkpoint;

/// What a statement did.
pub enum Answer<'a> {
    /// Rows to write out, and the columns that name them.
    Rows {
        plan: Box<dyn Operator + 'a>,
        /// The mark the rows are read at, held until the last one has gone
        /// out so that a checkpoint cannot take away the frames they come
        /// from partway through.
        _snapshot: Option<Lease>,
    },
    /// A statement that changed the schema or some rows.
    Changed { kind: &'static str, rows: u64 },
}

/// What a statement needs: the write lock, or a mark to read at.
enum Does {
    /// A change to the schema, which no transaction may hold.
    Schema,
    Change,
    Read,
}

/// How a transaction ends.
#[derive(Clone, Copy)]
enum Ending {
    Commit,
    Rollback,
}

/// Runs one statement, in the transaction its connection holds or in one of
/// its own.
pub fn run<'a>(
    statement: &Statement,
    db: &Arc<Database>,
    registry: &'a Registry,
    txn: &mut Option<Transaction>,
    cancel: &CancelHandle,
) -> Result<Answer<'a>, DbError> {
    match statement {
        Statement::Begin { read_only } => begin(db, registry, txn, *read_only),
        Statement::Commit => finish(db, registry, txn, Ending::Commit),
        Statement::Rollback => finish(db, registry, txn, Ending::Rollback),
        _ => match txn.as_mut() {
            Some(open) => inside(statement, db, registry, open, cancel),
            None => alone(statement, db, registry, cancel),
        },
    }
}

/// Opens a transaction. A connection holds one at a time, so a second
/// `BEGIN` is refused rather than taken for the first one.
fn begin<'a>(
    db: &Arc<Database>,
    registry: &Registry,
    txn: &mut Option<Transaction>,
    read_only: bool,
) -> Result<Answer<'a>, DbError> {
    if txn.is_some() {
        return Err(named(
            ErrorCode::TxnAlreadyOpen,
            "this connection already holds a transaction",
        ));
    }
    let kind = match read_only {
        true => TxnKind::ReadOnly,
        false => TxnKind::ReadWrite,
    };
    *txn = Some(db.begin(kind, registry.limits().lock_timeout)?);
    Ok(Answer::Changed {
        kind: "BEGIN",
        rows: 0,
    })
}

/// Ends the open transaction. A commit that fails rolls back instead, so the
/// pages it did not settle never reach a later statement.
fn finish<'a>(
    db: &Arc<Database>,
    registry: &Registry,
    txn: &mut Option<Transaction>,
    ending: Ending,
) -> Result<Answer<'a>, DbError> {
    let Some(open) = txn.take() else {
        return Err(named(
            ErrorCode::SyntaxError,
            "no transaction is open on this connection",
        ));
    };
    let pool = registry.pool();
    let kind = match ending {
        Ending::Commit => "COMMIT",
        Ending::Rollback => "ROLLBACK",
    };
    match ending {
        Ending::Rollback => open.rollback(pool, db)?,
        // A statement of this transaction failed partway, so its changes are
        // still in the pool and no one may add to them.
        Ending::Commit if open.is_aborted() => {
            open.rollback(pool, db)?;
            return Err(aborted());
        }
        Ending::Commit => match open.commit(pool, db) {
            // Still under the write lock, because the transaction holds it
            // until it drops at the end of this call.
            Ok(()) => settle(registry, db)?,
            Err(e) => {
                open.rollback(pool, db)?;
                return Err(e);
            }
        },
    }
    Ok(Answer::Changed { kind, rows: 0 })
}

/// A statement inside an open transaction.
fn inside<'a>(
    statement: &Statement,
    db: &Database,
    registry: &'a Registry,
    open: &mut Transaction,
    cancel: &CancelHandle,
) -> Result<Answer<'a>, DbError> {
    if open.is_aborted() {
        return Err(aborted());
    }
    match does(statement) {
        Does::Schema => Err(named(
            ErrorCode::SchemaChangeInTxn,
            "a schema change cannot run inside a transaction",
        )),
        Does::Read => dispatch(statement, db, registry, open.mark(), None, cancel),
        Does::Change => {
            open.check_writable()?;
            // A change that failed partway left pages in the pool that no
            // commit may settle, so the transaction ends here and the client
            // has to roll back.
            dispatch(statement, db, registry, open.mark(), None, cancel)
                .inspect_err(|_| open.abort())
        }
    }
}

/// A statement with no transaction around it, which is its own transaction.
fn alone<'a>(
    statement: &Statement,
    db: &Arc<Database>,
    registry: &'a Registry,
    cancel: &CancelHandle,
) -> Result<Answer<'a>, DbError> {
    match does(statement) {
        // A schema change is written by an atomic rename, not through the
        // log, so it takes no lock.
        Does::Schema => dispatch(statement, db, registry, Mark::Latest, None, cancel),
        Does::Change => {
            let txn = db.begin(TxnKind::ReadWrite, registry.limits().lock_timeout)?;
            let pool = registry.pool();
            match dispatch(statement, db, registry, Mark::Latest, None, cancel) {
                Ok(answer) => {
                    txn.commit(pool, db)?;
                    settle(registry, db)?;
                    Ok(answer)
                }
                // Nothing of a failed statement stays behind, not even in
                // the pool.
                Err(e) => {
                    txn.rollback(pool, db)?;
                    Err(e)
                }
            }
        }
        Does::Read => {
            let mark = db.wal.committed();
            let lease = Some(db.readers.lease(mark));
            dispatch(statement, db, registry, Mark::At(mark), lease, cancel)
        }
    }
}

/// What a statement needs of its database.
fn does(statement: &Statement) -> Does {
    match statement {
        Statement::CreateDatabase { .. }
        | Statement::DropDatabase { .. }
        | Statement::CreateTable { .. }
        | Statement::DropTable { .. } => Does::Schema,
        Statement::Insert { .. } | Statement::Update { .. } | Statement::Delete { .. } => {
            Does::Change
        }
        // `BEGIN`, `COMMIT` and `ROLLBACK` never reach here.
        _ => Does::Read,
    }
}

/// Moves the log into the table files once it has grown enough. The caller
/// holds the write lock, so nothing appends while the log is emptied.
fn settle(registry: &Registry, db: &Database) -> Result<(), DbError> {
    match checkpoint::due(&db.wal, &db.readers, registry.limits().checkpoint) {
        true => checkpoint::run(registry.pool(), &db.wal),
        false => Ok(()),
    }
}

fn aborted() -> DbError {
    named(
        ErrorCode::TxnAborted,
        "a statement of this transaction failed, so it has to be rolled back",
    )
}

/// Runs the statement itself, at the mark its reads take. A read carries the
/// lease of that mark out with its rows.
fn dispatch<'a>(
    statement: &Statement,
    db: &Database,
    registry: &'a Registry,
    mark: Mark,
    lease: Option<Lease>,
    cancel: &CancelHandle,
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
            let table = table_of(db, table)?;
            let reading = Reading {
                pool: registry.pool(),
                file: registry.table_file(db, &table)?,
                mark,
                tmp: registry.sort_dir(db),
                sort_bytes: registry.limits().sort,
                cancel: cancel.clone(),
            };
            let plan = planner::plan(
                &table,
                selection,
                filter.as_ref(),
                order_by.as_ref(),
                *limit,
                &reading,
            )?;
            Ok(Answer::Rows {
                plan,
                _snapshot: lease,
            })
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
        // `run` answers these, because they change what the connection
        // holds and not what a database holds.
        Statement::Begin { .. } | Statement::Commit | Statement::Rollback => Err(named(
            ErrorCode::SyntaxError,
            "a transaction statement runs on the connection",
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
    let tree = BTree::open(pool, file, &table, Mark::Latest);
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
    let tree = BTree::open(pool, file, &table, Mark::Latest);

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
    let tree = BTree::open(pool, file, &table, Mark::Latest);

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
    let mut cursor = tree.cursor(from, Bound::Unbounded, Direction::Ascending)?;
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
    use crate::cancel::CancelHandle;
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
        match run(
            &statement,
            &held.db,
            registry,
            &mut None,
            &CancelHandle::new(),
        )? {
            Answer::Changed { .. } => Ok(Vec::new()),
            Answer::Rows { mut plan, .. } => {
                let mut found = Vec::new();
                while let Some(row) = plan.next()? {
                    found.push(row);
                }
                Ok(found)
            }
        }
    }

    /// Runs a statement and returns how many rows it changed, or how many it
    /// read.
    fn changed(sql: &str, held: &Connected, registry: &Registry) -> Result<u64, DbError> {
        on(sql, held, registry, &mut None)
    }

    /// Runs a statement on a connection that may hold a transaction.
    fn on(
        sql: &str,
        held: &Connected,
        registry: &Registry,
        txn: &mut Option<Transaction>,
    ) -> Result<u64, DbError> {
        let statement = parser::parse(sql).expect("the statement parses");
        match run(&statement, &held.db, registry, txn, &CancelHandle::new())? {
            Answer::Changed { rows, .. } => Ok(rows),
            Answer::Rows { mut plan, .. } => {
                let mut rows = 0;
                while plan.next()?.is_some() {
                    rows += 1;
                }
                Ok(rows)
            }
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
            let Answer::Changed { kind: got, rows } = run(
                &statement,
                &held.db,
                &registry,
                &mut None,
                &CancelHandle::new(),
            )
            .unwrap() else {
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

    /// Rows of a sorted table, each written as its values joined by a space.
    /// A few rows of nonsense order, so a sort has work to do.
    fn stocked(held: &Connected, registry: &Registry) {
        item(held, registry);
        for (id, label, price) in [
            (3, "plum", "0.75"),
            (1, "apple", "1.50"),
            (4, "fig", "2.25"),
            (2, "pear", "1.5"),
        ] {
            changed(
                &format!("INSERT INTO item (id, label, price) VALUES ({id}, '{label}', {price})"),
                held,
                registry,
            )
            .unwrap();
        }
    }

    /// The files under the sort directory of a database.
    fn sort_files(registry: &Registry, held: &Connected) -> usize {
        std::fs::read_dir(registry.sort_dir(&held.db))
            .map(|read| read.count())
            .unwrap_or(0)
    }

    #[test]
    fn an_order_on_the_primary_key_needs_no_sort() {
        let (_dir, registry, held) = shop("order-by-key");
        stocked(&held, &registry);

        assert_eq!(
            shown("SELECT id FROM item ORDER BY id", &held, &registry),
            ["1", "2", "3", "4"]
        );
        assert_eq!(
            shown("SELECT id FROM item ORDER BY id DESC", &held, &registry),
            ["4", "3", "2", "1"]
        );
        assert_eq!(
            sort_files(&registry, &held),
            0,
            "the tree holds that order already"
        );
    }

    #[test]
    fn an_order_on_another_column_sorts() {
        let (_dir, registry, held) = shop("order-by-column");
        stocked(&held, &registry);

        assert_eq!(
            shown("SELECT label FROM item ORDER BY label", &held, &registry),
            ["apple", "fig", "pear", "plum"]
        );
        assert_eq!(
            shown(
                "SELECT label FROM item ORDER BY label DESC",
                &held,
                &registry
            ),
            ["plum", "pear", "fig", "apple"]
        );
        // A decimal by its value, so 1.5 and 1.50 fall together.
        assert_eq!(
            shown("SELECT id FROM item ORDER BY price", &held, &registry).len(),
            4
        );
        assert_eq!(
            sort_files(&registry, &held),
            0,
            "the runs went with the statement"
        );
    }

    #[test]
    fn an_order_can_name_a_column_the_selection_leaves_out() {
        let (_dir, registry, held) = shop("order-by-hidden");
        stocked(&held, &registry);
        assert_eq!(
            shown("SELECT id FROM item ORDER BY label", &held, &registry),
            ["1", "4", "2", "3"]
        );
    }

    #[test]
    fn a_limit_takes_the_first_rows_of_the_order() {
        let (_dir, registry, held) = shop("order-by-limit");
        stocked(&held, &registry);
        assert_eq!(
            shown(
                "SELECT label FROM item ORDER BY label LIMIT 2",
                &held,
                &registry
            ),
            ["apple", "fig"]
        );
        assert_eq!(
            shown(
                "SELECT id FROM item ORDER BY id DESC LIMIT 2",
                &held,
                &registry
            ),
            ["4", "3"]
        );
    }

    /// Plans a read with a flag of its own, and hands back both.
    fn planned<'a>(
        sql: &str,
        held: &Connected,
        registry: &'a Registry,
        cancel: &CancelHandle,
    ) -> Box<dyn Operator + 'a> {
        let statement = parser::parse(sql).expect("the statement parses");
        let answer = run(&statement, &held.db, registry, &mut None, cancel).expect("it plans");
        match answer {
            Answer::Rows { plan, .. } => plan,
            Answer::Changed { .. } => panic!("{sql} changes rows"),
        }
    }

    #[test]
    fn a_cancel_stops_a_scan_where_it_stands() {
        let (_dir, registry, held) = shop("cancel-scan");
        stocked(&held, &registry);
        let cancel = CancelHandle::new();
        let mut plan = planned("SELECT id FROM item", &held, &registry, &cancel);

        assert!(plan.next().unwrap().is_some());
        cancel.stop();

        for _ in 0..2 {
            let e = plan.next().expect_err("the flag is still set");
            assert_eq!(e.code, ErrorCode::Cancelled);
        }
    }

    #[test]
    fn a_cancel_stops_a_sort_before_it_hands_back_a_row() {
        let (_dir, registry, held) = shop("cancel-sort");
        stocked(&held, &registry);
        let cancel = CancelHandle::new();
        // Nothing is read first, so the cancel lands while the sort fills.
        let mut plan = planned(
            "SELECT id FROM item ORDER BY label",
            &held,
            &registry,
            &cancel,
        );
        cancel.stop();

        let e = plan.next().expect_err("the cancel stops it");
        assert_eq!(e.code, ErrorCode::Cancelled);
        assert_eq!(sort_files(&registry, &held), 0, "the runs went with it");
        // A sort that failed part way gave up its state, so it reports the
        // end rather than carrying on.
        cancel.clear();
        assert_eq!(plan.next().expect("the sort is done"), None);
    }

    #[test]
    fn an_order_on_a_column_that_is_not_there_is_an_error() {
        let (_dir, registry, held) = shop("order-by-unknown");
        item(&held, &registry);
        let e = rows("SELECT id FROM item ORDER BY colour", &held, &registry)
            .err()
            .unwrap();
        assert_eq!(e.code, ErrorCode::UnknownColumn);
        assert!(e.message.contains("colour"), "{e}");
    }

    #[test]
    fn a_null_sorts_last_and_a_condition_runs_before_the_order() {
        let (_dir, registry, held) = shop("order-by-nulls");
        item(&held, &registry);
        for (id, label, price) in [
            (1, "apple", "2.00"),
            (2, "pear", "NULL"),
            (3, "fig", "1.00"),
        ] {
            changed(
                &format!("INSERT INTO item (id, label, price) VALUES ({id}, '{label}', {price})"),
                &held,
                &registry,
            )
            .unwrap();
        }

        assert_eq!(
            shown("SELECT id FROM item ORDER BY price", &held, &registry),
            ["3", "1", "2"]
        );
        assert_eq!(
            shown("SELECT id FROM item ORDER BY price DESC", &held, &registry),
            ["2", "1", "3"]
        );
        assert_eq!(
            shown(
                "SELECT id FROM item WHERE id != 1 ORDER BY label",
                &held,
                &registry
            ),
            ["3", "2"],
            "the condition takes its rows out before the order"
        );
    }

    #[test]
    fn an_order_of_more_rows_than_the_budget_holds_spills_and_comes_back_in_order() {
        let (_dir, registry, held) = shop("order-by-spill");
        item(&held, &registry);
        // A few hundred rows against a budget of a few kilobytes, so the sort
        // writes runs and merges them.
        const ROWS: i64 = 300;
        for id in 0..ROWS {
            let label = format!("label of row {:04}", (id * 7919) % ROWS);
            changed(
                &format!("INSERT INTO item (id, label) VALUES ({id}, '{label}')"),
                &held,
                &registry,
            )
            .unwrap();
        }

        let order = shown("SELECT label FROM item ORDER BY label", &held, &registry);
        let mut want = order.clone();
        want.sort();
        assert_eq!(order, want);
        assert_eq!(order.len(), ROWS as usize);
        assert_eq!(sort_files(&registry, &held), 0, "tmp is empty again");
    }

    /// A database whose log takes every write and settles none of them,
    /// which is a disk that has gone away under the server.
    fn unsettling(label: &str) -> (Dir, std::sync::Arc<Registry>, Connected) {
        let dir = Dir::new(label);
        let registry = dir.registry();
        registry.create("shop").expect("the database is new");
        let wal = dir.0.join("shop").join("wal");
        std::fs::create_dir_all(&wal).expect("the log directory is made");
        std::os::unix::fs::symlink("/dev/null", wal.join("000.wal")).expect("the device links");
        let held = Registry::connect(&registry, "shop").expect("the database opens");
        (dir, registry, held)
    }

    #[test]
    fn a_commit_that_cannot_settle_keeps_nothing_and_still_reads() {
        let (_dir, registry, held) = unsettling("commit-fails");
        item(&held, &registry);

        let mut txn = None;
        on("BEGIN", &held, &registry, &mut txn).unwrap();
        on(
            "INSERT INTO item (id, label) VALUES (1, 'a')",
            &held,
            &registry,
            &mut txn,
        )
        .unwrap();
        let e = on("COMMIT", &held, &registry, &mut txn).err().unwrap();
        assert_eq!(e.code, ErrorCode::StorageFull);

        assert!(txn.is_none(), "the transaction is over either way");
        // Reads carry on after a write is refused, and the row is not there.
        assert_eq!(
            shown("SELECT id FROM item", &held, &registry),
            [] as [String; 0]
        );
    }

    #[test]
    fn a_transaction_statement_never_reaches_the_dispatcher() {
        let (_dir, registry, held) = shop("dispatch-guard");
        for sql in ["BEGIN", "BEGIN READ ONLY", "COMMIT", "ROLLBACK"] {
            let statement = parser::parse(sql).unwrap();
            let e = dispatch(
                &statement,
                &held.db,
                &registry,
                Mark::Latest,
                None,
                &CancelHandle::new(),
            )
            .err()
            .unwrap();
            assert_eq!(e.code, ErrorCode::SyntaxError, "{sql}");
            assert!(e.message.contains("runs on the connection"), "{e}");
        }
    }

    #[test]
    fn a_second_begin_on_one_connection_is_refused() {
        let (_dir, registry, held) = shop("second-begin");
        let mut txn = None;
        on("BEGIN", &held, &registry, &mut txn).unwrap();

        for sql in ["BEGIN", "BEGIN READ ONLY"] {
            let e = on(sql, &held, &registry, &mut txn).err().unwrap();
            assert_eq!(e.code, ErrorCode::TxnAlreadyOpen, "{sql}");
            assert!(e.message.contains("already holds"), "{e}");
        }
        assert!(txn.is_some(), "the transaction it held is still open");
    }

    #[test]
    fn an_end_with_no_transaction_open_is_refused() {
        let (_dir, registry, held) = shop("no-transaction");
        for sql in ["COMMIT", "ROLLBACK"] {
            let e = on(sql, &held, &registry, &mut None).err().unwrap();
            assert_eq!(e.code, ErrorCode::SyntaxError, "{sql}");
            assert!(e.message.contains("no transaction is open"), "{e}");
        }
    }

    #[test]
    fn a_transaction_keeps_its_rows_only_once_it_commits() {
        let (_dir, registry, held) = shop("commit-keeps");
        item(&held, &registry);
        let mut txn = None;
        on("BEGIN", &held, &registry, &mut txn).unwrap();
        on(
            "INSERT INTO item (id, label) VALUES (1, 'a')",
            &held,
            &registry,
            &mut txn,
        )
        .unwrap();
        // Its own changes are there for it to read.
        assert_eq!(
            on("SELECT * FROM item", &held, &registry, &mut txn).unwrap(),
            1
        );
        on("COMMIT", &held, &registry, &mut txn).unwrap();

        assert!(txn.is_none(), "the transaction ended");
        assert_eq!(shown("SELECT id FROM item", &held, &registry), ["1"]);
    }

    #[test]
    fn a_transaction_that_rolls_back_keeps_nothing() {
        let (_dir, registry, held) = shop("rollback-keeps-nothing");
        item(&held, &registry);
        changed(
            "INSERT INTO item (id, label) VALUES (1, 'a')",
            &held,
            &registry,
        )
        .unwrap();

        let mut txn = None;
        on("BEGIN", &held, &registry, &mut txn).unwrap();
        on(
            "INSERT INTO item (id, label) VALUES (2, 'b')",
            &held,
            &registry,
            &mut txn,
        )
        .unwrap();
        on("UPDATE item SET label = 'z'", &held, &registry, &mut txn).unwrap();
        on("ROLLBACK", &held, &registry, &mut txn).unwrap();

        assert!(txn.is_none());
        assert_eq!(
            shown("SELECT id, label FROM item", &held, &registry),
            ["1 a"]
        );
    }

    #[test]
    fn a_read_only_transaction_refuses_every_change() {
        let (_dir, registry, held) = shop("read-only-refuses");
        item(&held, &registry);
        let mut txn = None;
        on("BEGIN READ ONLY", &held, &registry, &mut txn).unwrap();

        for sql in [
            "INSERT INTO item (id, label) VALUES (1, 'a')",
            "UPDATE item SET label = 'b'",
            "DELETE FROM item",
        ] {
            let e = on(sql, &held, &registry, &mut txn).err().unwrap();
            assert_eq!(e.code, ErrorCode::ReadOnlyTxn, "{sql}");
        }
        // It still reads.
        on("SELECT * FROM item", &held, &registry, &mut txn).unwrap();
    }

    #[test]
    fn a_schema_change_inside_a_transaction_is_refused() {
        let (_dir, registry, held) = shop("ddl-in-transaction");
        for read_only in ["BEGIN", "BEGIN READ ONLY"] {
            let mut txn = None;
            on(read_only, &held, &registry, &mut txn).unwrap();
            for sql in [
                "CREATE TABLE t (a INTEGER PRIMARY KEY)",
                "DROP TABLE item",
                "CREATE DATABASE other",
                "DROP DATABASE other",
            ] {
                let e = on(sql, &held, &registry, &mut txn).err().unwrap();
                assert_eq!(e.code, ErrorCode::SchemaChangeInTxn, "{sql}");
            }
            on("ROLLBACK", &held, &registry, &mut txn).unwrap();
        }
    }

    #[test]
    fn a_statement_that_failed_partway_ends_its_transaction() {
        let (_dir, registry, held) = shop("aborted");
        item(&held, &registry);
        for id in 1..=2 {
            changed(
                &format!("INSERT INTO item (id, label) VALUES ({id}, 'a')"),
                &held,
                &registry,
            )
            .unwrap();
        }

        let mut txn = None;
        on("BEGIN", &held, &registry, &mut txn).unwrap();
        // The first row takes the key, the second one collides with it.
        let e = on("UPDATE item SET id = 7", &held, &registry, &mut txn)
            .err()
            .unwrap();
        assert_eq!(e.code, ErrorCode::DuplicateKey);

        let e = on("SELECT * FROM item", &held, &registry, &mut txn)
            .err()
            .unwrap();
        assert_eq!(e.code, ErrorCode::TxnAborted);
        let e = on("COMMIT", &held, &registry, &mut txn).err().unwrap();
        assert_eq!(e.code, ErrorCode::TxnAborted);

        assert!(txn.is_none(), "the commit ended it");
        assert_eq!(
            shown("SELECT id FROM item", &held, &registry),
            ["1", "2"],
            "the half-finished change is gone"
        );
    }

    #[test]
    fn a_failed_statement_of_its_own_changes_nothing() {
        let (_dir, registry, held) = shop("failed-alone");
        item(&held, &registry);
        for id in 1..=2 {
            changed(
                &format!("INSERT INTO item (id, label) VALUES ({id}, 'a')"),
                &held,
                &registry,
            )
            .unwrap();
        }

        let e = changed("UPDATE item SET id = 7", &held, &registry)
            .err()
            .unwrap();
        assert_eq!(e.code, ErrorCode::DuplicateKey);
        assert_eq!(shown("SELECT id FROM item", &held, &registry), ["1", "2"]);
    }

    #[test]
    fn a_reader_outside_a_transaction_holds_a_mark_while_its_rows_flow() {
        let (_dir, registry, held) = shop("reader-lease");
        item(&held, &registry);
        changed(
            "INSERT INTO item (id, label) VALUES (1, 'a')",
            &held,
            &registry,
        )
        .unwrap();

        let statement = parser::parse("SELECT * FROM item").unwrap();
        let answer = run(
            &statement,
            &held.db,
            &registry,
            &mut None,
            &CancelHandle::new(),
        )
        .unwrap();
        assert_eq!(
            held.db.readers.oldest(),
            Some(held.db.wal.committed()),
            "the rows have not gone out yet"
        );
        drop(answer);
        assert_eq!(held.db.readers.oldest(), None);
    }

    #[test]
    fn a_read_names_the_columns_it_hands_back() {
        let (_dir, registry, held) = shop("schema");
        item(&held, &registry);
        let statement = parser::parse("SELECT label, id FROM item").unwrap();
        let Answer::Rows { plan, .. } = run(
            &statement,
            &held.db,
            &registry,
            &mut None,
            &CancelHandle::new(),
        )
        .unwrap() else {
            panic!("expected rows");
        };
        let names: Vec<&str> = plan.schema().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["label", "id"]);

        let statement = parser::parse("SELECT * FROM item").unwrap();
        let Answer::Rows { plan, .. } = run(
            &statement,
            &held.db,
            &registry,
            &mut None,
            &CancelHandle::new(),
        )
        .unwrap() else {
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
        assert_eq!(
            held.db.wal.end(),
            0,
            "a schema change is written by a rename, not through the log"
        );

        changed(
            "INSERT INTO item (id, label) VALUES (1, 'a')",
            &held,
            &registry,
        )
        .unwrap();
        let after_insert = held.db.wal.end();
        assert!(after_insert > 0, "the rows reached the log");

        changed(
            "INSERT INTO item (id, label) VALUES (2, 'b')",
            &held,
            &registry,
        )
        .unwrap();
        assert!(
            held.db.wal.end() > after_insert,
            "a checkpoint runs on size, not on every statement"
        );
    }

    #[test]
    fn a_log_past_its_threshold_is_checkpointed_and_the_rows_do_not_change() {
        let (_dir, registry, held) = shop("checkpoint-on-size");
        item(&held, &registry);
        let threshold = registry.limits().checkpoint;

        // Each statement appends the page it changed, so a hundred of them
        // pass the threshold several times over. A checkpoint runs at the end
        // of the statement that passes it, so the log is never seen above it.
        const ROWS: u64 = 100;
        for id in 1..=ROWS {
            changed(
                &format!("INSERT INTO item (id, label) VALUES ({id}, 'a')"),
                &held,
                &registry,
            )
            .unwrap();
        }

        assert!(
            held.db.wal.end() <= threshold,
            "the log grew to {} with a threshold of {threshold}",
            held.db.wal.end()
        );
        // The pages went to their table file, not away.
        assert_eq!(
            shown("SELECT id FROM item", &held, &registry).len(),
            ROWS as usize
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
