//! The statements that change a schema.
//!
//! A schema change takes no transaction, because [FR55] keeps it out of one.
//! Each change saves the catalog, and a save that fails leaves the schema as
//! it was, which gives [FR69].

use std::fs;
use std::io;

use protocol::{DbError, ErrorCode};

use crate::catalog::registry::Registry;
use crate::catalog::{ColumnDef, Database, named};
use crate::sql::ast::{ColumnSpec, Statement};

/// Runs one statement and names its kind for the `Complete` that follows.
///
/// A statement that reads or writes a row answers `UNKNOWN_TABLE`, because
/// the executor arrives in milestone 8 and a transaction in milestone 10.
pub fn run(
    statement: &Statement,
    db: &Database,
    registry: &Registry,
) -> Result<&'static str, DbError> {
    match statement {
        Statement::CreateDatabase { name } => {
            registry.create(name)?;
            Ok("CREATE DATABASE")
        }
        Statement::DropDatabase { name } => {
            registry.drop_database(name)?;
            Ok("DROP DATABASE")
        }
        Statement::CreateTable { name, columns } => {
            create_table(db, name, columns)?;
            Ok("CREATE TABLE")
        }
        Statement::DropTable { name } => {
            drop_table(db, name)?;
            Ok("DROP TABLE")
        }
        _ => Err(named(
            ErrorCode::UnknownTable,
            "the server runs no statement on a table yet",
        )),
    }
}

/// Adds a table. See [FR6] to [FR9] and [FR12].
fn create_table(db: &Database, name: &str, specs: &[ColumnSpec]) -> Result<(), DbError> {
    check_columns(name, specs)?;
    let pk_index = primary_key(name, specs)?;
    let columns = specs
        .iter()
        .map(|spec| ColumnDef {
            name: spec.name.clone(),
            ty: spec.ty,
            // A primary key holds a value for every row, said or not.
            not_null: spec.not_null || spec.primary_key,
        })
        .collect();

    let mut catalog = db.catalog.lock().expect("the catalog lock holds");
    catalog.add_table(name, columns, pk_index)?;
    let saved = catalog.save();
    if saved.is_err() {
        let _ = catalog.remove_table(name);
    }
    saved
}

/// Deletes a table and the rows that went with it. See [FR10], [FR11],
/// and [FR13].
fn drop_table(db: &Database, name: &str) -> Result<(), DbError> {
    let mut catalog = db.catalog.lock().expect("the catalog lock holds");
    let table = catalog.remove_table(name)?;
    let rows = catalog.dir().join(table.file_name());
    if let Err(e) = catalog.save() {
        catalog.restore(table);
        return Err(e);
    }
    match fs::remove_file(&rows) {
        Ok(()) => Ok(()),
        // No table holds a file until milestone 6 writes one.
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(named(
            ErrorCode::StorageFull,
            format!("the rows of {name} were not deleted: {e}"),
        )),
    }
}

/// Which column is the primary key. The B-tree is keyed by it, so a table
/// needs exactly one.
fn primary_key(name: &str, specs: &[ColumnSpec]) -> Result<usize, DbError> {
    let mut keys = specs
        .iter()
        .enumerate()
        .filter(|(_, spec)| spec.primary_key);
    let Some((index, _)) = keys.next() else {
        return Err(named(
            ErrorCode::SyntaxError,
            format!("table {name} needs a primary key"),
        ));
    };
    match keys.next() {
        None => Ok(index),
        Some(_) => Err(named(
            ErrorCode::SyntaxError,
            format!("table {name} names more than one primary key"),
        )),
    }
}

/// A column name appears once. Two columns of one name would make a row
/// impossible to read back.
fn check_columns(name: &str, specs: &[ColumnSpec]) -> Result<(), DbError> {
    for (index, spec) in specs.iter().enumerate() {
        if specs[..index].iter().any(|before| before.name == spec.name) {
            return Err(named(
                ErrorCode::SyntaxError,
                format!("table {name} names the column {} twice", spec.name),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use protocol::DataType;

    use super::*;
    use crate::sql::parser;

    use crate::catalog::testing::Dir;

    /// Runs the statement that `sql` holds.
    fn run_sql(sql: &str, db: &Database, registry: &Registry) -> Result<&'static str, DbError> {
        let statement = parser::parse(sql).expect("the statement parses");
        run(&statement, db, registry)
    }

    #[test]
    fn a_table_is_created_and_then_dropped() {
        let dir = Dir::new("lifecycle");
        let (registry, held) = dir.shop();
        assert_eq!(
            run_sql(
                "CREATE TABLE item (id INTEGER PRIMARY KEY, label TEXT NOT NULL)",
                &held.db,
                &registry
            )
            .unwrap(),
            "CREATE TABLE"
        );
        {
            let catalog = held.db.catalog.lock().unwrap();
            let table = catalog.table("item").unwrap();
            assert_eq!(table.pk_index, 0);
            assert_eq!(table.columns[0].ty, DataType::Integer);
            assert!(table.columns[0].not_null, "a primary key is not null");
            assert!(table.columns[1].not_null);
        }
        assert_eq!(
            run_sql("DROP TABLE item", &held.db, &registry).unwrap(),
            "DROP TABLE"
        );
        assert!(held.db.catalog.lock().unwrap().table("item").is_err());
    }

    #[test]
    fn a_table_that_exists_is_refused_and_one_that_does_not_cannot_be_dropped() {
        let dir = Dir::new("refusals");
        let (registry, held) = dir.shop();
        run_sql(
            "CREATE TABLE item (id INTEGER PRIMARY KEY)",
            &held.db,
            &registry,
        )
        .unwrap();
        assert_eq!(
            run_sql(
                "CREATE TABLE item (id INTEGER PRIMARY KEY)",
                &held.db,
                &registry
            )
            .unwrap_err()
            .code,
            ErrorCode::TableExists
        );
        assert_eq!(
            run_sql("DROP TABLE other", &held.db, &registry)
                .unwrap_err()
                .code,
            ErrorCode::UnknownTable
        );
    }

    #[test]
    fn a_table_needs_exactly_one_primary_key() {
        let dir = Dir::new("keys");
        let (registry, held) = dir.shop();
        for sql in [
            "CREATE TABLE item (id INTEGER, label TEXT)",
            "CREATE TABLE item (id INTEGER PRIMARY KEY, label TEXT PRIMARY KEY)",
        ] {
            let e = run_sql(sql, &held.db, &registry).unwrap_err();
            assert_eq!(e.code, ErrorCode::SyntaxError, "{sql}");
            assert!(e.message.contains("primary key"), "{e}");
        }
        assert!(held.db.catalog.lock().unwrap().table("item").is_err());
    }

    #[test]
    fn a_column_name_appears_once() {
        let dir = Dir::new("columns");
        let (registry, held) = dir.shop();
        let e = run_sql(
            "CREATE TABLE item (id INTEGER PRIMARY KEY, id TEXT)",
            &held.db,
            &registry,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::SyntaxError);
        assert!(e.message.contains("twice"), "{e}");
    }

    #[test]
    fn the_second_column_can_be_the_primary_key() {
        let dir = Dir::new("second-key");
        let (registry, held) = dir.shop();
        run_sql(
            "CREATE TABLE item (label TEXT, id INTEGER PRIMARY KEY)",
            &held.db,
            &registry,
        )
        .unwrap();
        assert_eq!(
            held.db
                .catalog
                .lock()
                .unwrap()
                .table("item")
                .unwrap()
                .pk_index,
            1
        );
    }

    #[test]
    fn a_database_is_made_and_dropped_through_a_statement() {
        let dir = Dir::new("database");
        let (registry, held) = dir.shop();
        assert_eq!(
            run_sql("CREATE DATABASE other", &held.db, &registry).unwrap(),
            "CREATE DATABASE"
        );
        assert_eq!(
            run_sql("CREATE DATABASE other", &held.db, &registry)
                .unwrap_err()
                .code,
            ErrorCode::DatabaseExists
        );
        assert_eq!(
            run_sql("DROP DATABASE other", &held.db, &registry).unwrap(),
            "DROP DATABASE"
        );
        // Its own database is held by this connection.
        assert_eq!(
            run_sql("DROP DATABASE shop", &held.db, &registry)
                .unwrap_err()
                .code,
            ErrorCode::DatabaseInUse
        );
    }

    #[test]
    fn the_rows_of_a_table_go_when_the_table_goes() {
        let dir = Dir::new("rows");
        let (registry, held) = dir.shop();
        run_sql(
            "CREATE TABLE item (id INTEGER PRIMARY KEY)",
            &held.db,
            &registry,
        )
        .unwrap();
        let rows = {
            let catalog = held.db.catalog.lock().unwrap();
            let table = catalog.table("item").unwrap();
            catalog.dir().join(table.file_name())
        };
        // Milestone 6 writes this file. Standing in for it proves the delete.
        fs::write(&rows, b"rows").unwrap();
        run_sql("DROP TABLE item", &held.db, &registry).unwrap();
        assert!(!rows.exists());
    }

    #[test]
    fn a_statement_on_a_row_waits_for_the_executor() {
        let dir = Dir::new("rows-later");
        let (registry, held) = dir.shop();
        run_sql(
            "CREATE TABLE item (id INTEGER PRIMARY KEY)",
            &held.db,
            &registry,
        )
        .unwrap();
        for sql in [
            "SELECT * FROM item",
            "INSERT INTO item (id) VALUES (1)",
            "UPDATE item SET id = 2",
            "DELETE FROM item",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
        ] {
            assert_eq!(
                run_sql(sql, &held.db, &registry).unwrap_err().code,
                ErrorCode::UnknownTable,
                "{sql}"
            );
        }
    }

    #[test]
    fn rows_that_will_not_delete_report_storage_full() {
        let dir = Dir::new("rows-stuck");
        let (registry, held) = dir.shop();
        run_sql(
            "CREATE TABLE item (id INTEGER PRIMARY KEY)",
            &held.db,
            &registry,
        )
        .unwrap();
        let rows = {
            let catalog = held.db.catalog.lock().unwrap();
            let table = catalog.table("item").unwrap();
            catalog.dir().join(table.file_name())
        };
        // Where the rows should be stands something that is not a file, so
        // the delete fails for a reason that is not absence.
        fs::create_dir(&rows).unwrap();
        fs::write(rows.join("inside"), b"x").unwrap();

        let e = run_sql("DROP TABLE item", &held.db, &registry).unwrap_err();
        assert_eq!(e.code, ErrorCode::StorageFull);
        assert!(e.message.contains("were not deleted"), "{e}");
    }

    #[test]
    fn a_catalog_that_will_not_save_changes_no_schema() {
        let dir = Dir::new("no-save");
        let (registry, held) = dir.shop();
        run_sql(
            "CREATE TABLE kept (id INTEGER PRIMARY KEY)",
            &held.db,
            &registry,
        )
        .unwrap();

        // The directory goes, so every save from here on fails.
        fs::remove_dir_all(dir.0.join("shop")).unwrap();

        let e = run_sql(
            "CREATE TABLE item (id INTEGER PRIMARY KEY)",
            &held.db,
            &registry,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::StorageFull);
        assert!(
            held.db.catalog.lock().unwrap().table("item").is_err(),
            "a failed create left a table behind"
        );

        let e = run_sql("DROP TABLE kept", &held.db, &registry).unwrap_err();
        assert_eq!(e.code, ErrorCode::StorageFull);
        assert!(
            held.db.catalog.lock().unwrap().table("kept").is_ok(),
            "a failed drop took the table away"
        );
    }
}
