//! The schema of one database, and the file that holds it.
//!
//! The catalog is rewritten whole: a temp file, an `fsync`, a rename, then an
//! `fsync` of the directory. [FR55] keeps a schema change out of a
//! transaction, so an atomic rename is the only crash safety it needs and no
//! WAL record is written.

pub mod ddl;
pub mod registry;
#[cfg(test)]
pub mod testing;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use protocol::{DataType, DbError, ErrorCode};
use serde::{Deserialize, Serialize};

use crate::wal::writer::Wal;

/// The schema file of a database.
const FILE: &str = "catalog.json";

/// Where a save writes before it renames. A crash leaves this behind, and it
/// is not a catalog, so the next load ignores it.
const TEMP: &str = "catalog.json.tmp";

/// An error about a name. A position needs a statement, and a schema failure
/// is about a name the statement held.
pub fn named(code: ErrorCode, message: impl Into<String>) -> DbError {
    DbError {
        code,
        message: message.into(),
        position: None,
    }
}

/// A file that would not write. Milestone 9 makes a failed `fsync` fatal.
fn write_failed(e: io::Error) -> DbError {
    named(
        ErrorCode::StorageFull,
        format!("the catalog did not save: {e}"),
    )
}

fn unknown_table(name: &str) -> DbError {
    named(ErrorCode::UnknownTable, format!("no table named {name}"))
}

/// One column of a table. See [FR7] to [FR9].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub ty: DataType,
    pub not_null: bool,
}

/// One table. The file that holds its rows is named by `id` and not by name,
/// so a rename would touch only the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableDef {
    pub id: u32,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// Which column is the primary key. The B-tree is keyed by it.
    pub pk_index: usize,
}

impl TableDef {
    /// The file that holds the rows. Milestone 6 writes it.
    pub fn file_name(&self) -> String {
        format!("{}.tbl", self.id)
    }
}

/// The schema of one database, held in memory and rewritten whole.
#[derive(Debug, Serialize, Deserialize)]
pub struct Catalog {
    /// The directory of the database. It is not part of the file, because the
    /// file is found by it.
    #[serde(skip)]
    dir: PathBuf,
    tables: BTreeMap<String, TableDef>,
    /// The id the next table takes. An id is never reused, so a stale `.tbl`
    /// file can never pass for a live table.
    next_table_id: u32,
}

impl Catalog {
    /// Reads the catalog of a database. A directory with no file yet reads as
    /// an empty catalog, which is what a database starts as.
    pub fn load(dir: &Path) -> io::Result<Catalog> {
        match fs::read(dir.join(FILE)) {
            Ok(bytes) => {
                let mut catalog: Catalog =
                    serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                catalog.dir = dir.to_path_buf();
                Ok(catalog)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Catalog::empty_in(dir)),
            Err(e) => Err(e),
        }
    }

    /// The catalog of a database that has just been made. Nothing is read,
    /// so nothing can fail.
    pub fn empty_in(dir: &Path) -> Catalog {
        Catalog {
            dir: dir.to_path_buf(),
            tables: BTreeMap::new(),
            next_table_id: 1,
        }
    }

    /// Writes the catalog so that a crash leaves either the old file or the
    /// new one, and never half of either.
    pub fn save(&self) -> Result<(), DbError> {
        self.save_atomic().map_err(write_failed)
    }

    fn save_atomic(&self) -> io::Result<()> {
        let temp = self.dir.join(TEMP);
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        let mut file = File::create(&temp)?;
        file.write_all(&json)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, self.dir.join(FILE))?;
        // The rename is durable only once the directory itself is synced.
        File::open(&self.dir)?.sync_all()
    }

    /// Adds a table and hands it the next id. See [FR6] and [FR12].
    pub fn add_table(
        &mut self,
        name: &str,
        columns: Vec<ColumnDef>,
        pk_index: usize,
    ) -> Result<(), DbError> {
        if self.tables.contains_key(name) {
            return Err(named(
                ErrorCode::TableExists,
                format!("a table named {name} exists"),
            ));
        }
        let id = self.next_table_id;
        self.next_table_id += 1;
        self.tables.insert(
            name.to_string(),
            TableDef {
                id,
                name: name.to_string(),
                columns,
                pk_index,
            },
        );
        Ok(())
    }

    /// Removes a table and returns it, so the caller can delete its file.
    /// See [FR10], [FR11], and [FR13].
    pub fn remove_table(&mut self, name: &str) -> Result<TableDef, DbError> {
        self.tables.remove(name).ok_or_else(|| unknown_table(name))
    }

    /// Puts a table back after a save that failed, so a statement that
    /// failed changes nothing.
    fn restore(&mut self, table: TableDef) {
        self.tables.insert(table.name.clone(), table);
    }

    /// One table by name. See [FR13]. Milestone 8 reads a table to run a
    /// statement against it, so nothing outside a test reads one before then.
    #[allow(dead_code)]
    pub fn table(&self, name: &str) -> Result<&TableDef, DbError> {
        self.tables.get(name).ok_or_else(|| unknown_table(name))
    }

    /// The file of every table the catalog names.
    pub fn files(&self) -> impl Iterator<Item = String> + '_ {
        self.tables.values().map(TableDef::file_name)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// One database: its name, its schema, and the log its writes go through.
/// The write lock joins it in milestone 10.
pub struct Database {
    pub name: String,
    pub catalog: Mutex<Catalog>,
    pub wal: Arc<Wal>,
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::testing::Dir;

    fn column(name: &str) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            ty: DataType::Integer,
            not_null: true,
        }
    }

    #[test]
    fn a_directory_with_no_file_reads_as_an_empty_catalog() {
        let dir = Dir::new("empty");
        let catalog = Catalog::load(&dir.0).unwrap();
        assert_eq!(catalog.files().count(), 0);
        assert_eq!(
            catalog.table("item").unwrap_err().code,
            ErrorCode::UnknownTable
        );
    }

    #[test]
    fn a_table_survives_a_save_and_a_load_with_every_field() {
        let dir = Dir::new("round-trip");
        let mut catalog = Catalog::load(&dir.0).unwrap();
        let columns = vec![
            column("id"),
            ColumnDef {
                name: "price".to_string(),
                ty: DataType::Decimal { p: 10, s: 2 },
                not_null: false,
            },
        ];
        catalog.add_table("item", columns.clone(), 0).unwrap();
        catalog.save().unwrap();

        let read = Catalog::load(&dir.0).unwrap();
        let table = read.table("item").unwrap();
        assert_eq!(table.name, "item");
        assert_eq!(table.columns, columns);
        assert_eq!(table.pk_index, 0);
        assert_eq!(table.file_name(), format!("{}.tbl", table.id));
    }

    #[test]
    fn a_second_table_takes_the_next_id_and_a_dropped_id_never_returns() {
        let dir = Dir::new("ids");
        let mut catalog = Catalog::load(&dir.0).unwrap();
        catalog.add_table("a", vec![column("id")], 0).unwrap();
        catalog.add_table("b", vec![column("id")], 0).unwrap();
        let first = catalog.table("a").unwrap().id;
        let second = catalog.table("b").unwrap().id;
        assert_ne!(first, second);

        catalog.remove_table("a").unwrap();
        catalog.add_table("c", vec![column("id")], 0).unwrap();
        let third = catalog.table("c").unwrap().id;
        assert_ne!(third, first, "a dropped id is never handed out again");
        assert_ne!(third, second);

        // The count survives a restart, or a reload would reuse an id.
        catalog.save().unwrap();
        let mut read = Catalog::load(&dir.0).unwrap();
        read.add_table("d", vec![column("id")], 0).unwrap();
        assert!(read.table("d").unwrap().id > third);
    }

    #[test]
    fn a_table_that_exists_is_refused_and_keeps_its_columns() {
        let dir = Dir::new("exists");
        let mut catalog = Catalog::load(&dir.0).unwrap();
        catalog.add_table("item", vec![column("id")], 0).unwrap();
        let e = catalog
            .add_table("item", vec![column("other")], 0)
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::TableExists);
        assert_eq!(catalog.table("item").unwrap().columns, vec![column("id")]);
    }

    #[test]
    fn a_table_that_is_not_there_cannot_be_read_or_removed() {
        let dir = Dir::new("missing");
        let mut catalog = Catalog::load(&dir.0).unwrap();
        assert_eq!(
            catalog.remove_table("item").unwrap_err().code,
            ErrorCode::UnknownTable
        );
        assert_eq!(
            catalog.table("item").unwrap_err().code,
            ErrorCode::UnknownTable
        );
    }

    #[test]
    fn a_name_keeps_its_case() {
        let dir = Dir::new("case");
        let mut catalog = Catalog::load(&dir.0).unwrap();
        catalog.add_table("item", vec![column("id")], 0).unwrap();
        catalog.add_table("Item", vec![column("id")], 0).unwrap();
        assert_ne!(
            catalog.table("item").unwrap().id,
            catalog.table("Item").unwrap().id
        );
    }

    #[test]
    fn a_save_leaves_no_temp_file_behind() {
        let dir = Dir::new("temp");
        let mut catalog = Catalog::load(&dir.0).unwrap();
        catalog.add_table("item", vec![column("id")], 0).unwrap();
        catalog.save().unwrap();
        assert!(dir.0.join(FILE).exists());
        assert!(!dir.0.join(TEMP).exists());
    }

    #[test]
    fn a_temp_file_a_crash_left_behind_is_not_a_catalog() {
        let dir = Dir::new("leftover");
        let mut catalog = Catalog::load(&dir.0).unwrap();
        catalog.add_table("item", vec![column("id")], 0).unwrap();
        catalog.save().unwrap();

        // What a crash between the write and the rename leaves.
        fs::write(dir.0.join(TEMP), b"{\"tables\":{},\"next_table_id\":99}").unwrap();
        let read = Catalog::load(&dir.0).unwrap();
        assert!(read.table("item").is_ok(), "the real catalog still reads");

        // And the next save replaces it rather than tripping over it.
        read.save().unwrap();
        assert!(!dir.0.join(TEMP).exists());
    }

    #[test]
    fn a_catalog_that_is_not_json_is_an_error() {
        let dir = Dir::new("corrupt");
        fs::write(dir.0.join(FILE), b"not json").unwrap();
        let e = Catalog::load(&dir.0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn a_catalog_that_cannot_be_read_reports_the_reason() {
        // A directory where the file should be is neither missing nor JSON.
        let dir = Dir::new("unreadable");
        fs::create_dir(dir.0.join(FILE)).unwrap();
        assert!(Catalog::load(&dir.0).is_err());
    }

    #[test]
    fn a_save_into_a_directory_that_is_gone_reports_storage_full() {
        let dir = Dir::new("vanished");
        let mut catalog = Catalog::load(&dir.0).unwrap();
        catalog.add_table("item", vec![column("id")], 0).unwrap();
        fs::remove_dir_all(&dir.0).unwrap();
        let e = catalog.save().unwrap_err();
        assert_eq!(e.code, ErrorCode::StorageFull);
        assert!(e.message.contains("did not save"), "{e}");
    }

    #[test]
    fn the_files_of_a_catalog_are_named_by_id() {
        let dir = Dir::new("files");
        let mut catalog = Catalog::load(&dir.0).unwrap();
        catalog.add_table("a", vec![column("id")], 0).unwrap();
        catalog.add_table("b", vec![column("id")], 0).unwrap();
        let files: Vec<String> = catalog.files().collect();
        assert_eq!(files, vec!["1.tbl".to_string(), "2.tbl".to_string()]);
        assert_eq!(catalog.dir(), dir.0);
    }
}
