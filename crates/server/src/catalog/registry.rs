//! The databases of one server.
//!
//! A database is a directory, and there is no server-level catalog, so the
//! databases that exist are the directories under the data directory.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use protocol::{DbError, ErrorCode};

use crate::catalog::{Catalog, Database, TableDef, named};
use crate::store::page::FileId;
use crate::store::pool::BufferPool;

/// The database a new data directory gets. A client must name a database to
/// connect, so a directory that holds none could never be reached.
pub const FIRST_DATABASE: &str = "default";

/// What the registry holds under one lock, so the count and the map never
/// disagree about a database.
#[derive(Debug, Default)]
struct State {
    open: HashMap<String, Arc<Database>>,
    /// How many connections hold each database. A database with no
    /// connection holds no row here. See [FR4].
    connections: HashMap<String, usize>,
}

/// Every database of one server.
pub struct Registry {
    data_dir: PathBuf,
    state: Mutex<State>,
    /// One pool for the whole server, because the memory limit is for the
    /// whole server.
    pool: BufferPool,
}

impl Registry {
    /// Opens the data directory, making it when it is not there yet. The
    /// memory limit sizes the one pool that every database reads through.
    pub fn new(data_dir: &Path, mem_limit: u64) -> io::Result<Registry> {
        fs::create_dir_all(data_dir)?;
        Ok(Registry {
            data_dir: data_dir.to_path_buf(),
            state: Mutex::new(State::default()),
            pool: BufferPool::new(mem_limit),
        })
    }

    pub fn pool(&self) -> &BufferPool {
        &self.pool
    }

    /// The file that holds the rows of a table. The pool hands out the same
    /// id for a table it has already opened.
    pub fn table_file(&self, database: &str, table: &TableDef) -> Result<FileId, DbError> {
        self.pool
            .open(&self.data_dir.join(database).join(table.file_name()))
    }

    /// Takes a database for one connection. A client sees only this database,
    /// which gives [FR5]. The count falls when the guard drops, which lets a
    /// `DROP DATABASE` through.
    pub fn connect(registry: &Arc<Registry>, name: &str) -> Result<Connected, DbError> {
        check_name(name)?;
        let dir = registry.dir_of(name)?;
        let mut state = registry.state.lock().expect("the registry lock holds");
        let db = match state.open.get(name) {
            Some(db) => Arc::clone(db),
            None => {
                let catalog = Catalog::load(&dir).map_err(|e| {
                    named(
                        ErrorCode::UnknownDatabase,
                        format!("database {name} did not open: {e}"),
                    )
                })?;
                let db = Arc::new(Database {
                    name: name.to_string(),
                    catalog: Mutex::new(catalog),
                });
                state.open.insert(name.to_string(), Arc::clone(&db));
                db
            }
        };
        *state.connections.entry(name.to_string()).or_insert(0) += 1;
        Ok(Connected {
            db,
            registry: Arc::clone(registry),
        })
    }

    /// Makes a database. See [FR1] and [FR3].
    pub fn create(&self, name: &str) -> Result<(), DbError> {
        check_name(name)?;
        // A filesystem that folds case would hold `shop` and `Shop` in one
        // directory. Refusing the clash here makes the answer the same on
        // every platform instead of the filesystem's answer.
        if self
            .names()?
            .iter()
            .any(|held| held.eq_ignore_ascii_case(name))
        {
            return Err(exists(name));
        }
        let dir = self.data_dir.join(name);
        // One atomic step, so two clients cannot both believe they made it.
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(exists(name)),
            Err(e) => {
                return Err(named(
                    ErrorCode::StorageFull,
                    format!("database {name} was not made: {e}"),
                ));
            }
        }
        Catalog::empty_in(&dir).save()
    }

    /// Deletes a database and every table in it. See [FR2], [FR4], [FR11].
    pub fn drop_database(&self, name: &str) -> Result<(), DbError> {
        check_name(name)?;
        let dir = self.dir_of(name)?;
        let mut state = self.state.lock().expect("the registry lock holds");
        if state.connections.contains_key(name) {
            return Err(named(
                ErrorCode::DatabaseInUse,
                format!("a client is connected to {name}"),
            ));
        }
        fs::remove_dir_all(&dir).map_err(|e| {
            named(
                ErrorCode::StorageFull,
                format!("database {name} was not deleted: {e}"),
            )
        })?;
        state.open.remove(name);
        Ok(())
    }

    /// Makes the first database when the data directory holds none. A
    /// database that was dropped stays dropped, because only an empty
    /// directory gets one.
    pub fn bootstrap(&self) -> io::Result<()> {
        if !self.database_dirs()?.is_empty() {
            return Ok(());
        }
        self.create(FIRST_DATABASE).map_err(io::Error::other)?;
        log::info!("made the first database {FIRST_DATABASE}");
        Ok(())
    }

    /// The directory of a database, found by its exact name. Asking the
    /// filesystem would answer for a name the server never made, on a
    /// filesystem that folds case.
    fn dir_of(&self, name: &str) -> Result<PathBuf, DbError> {
        self.database_dirs()
            .map_err(unreadable)?
            .into_iter()
            .find(|dir| dir_name(dir) == Some(name))
            .ok_or_else(|| unknown(name))
    }

    /// The name of every database.
    fn names(&self) -> Result<Vec<String>, DbError> {
        Ok(self
            .database_dirs()
            .map_err(unreadable)?
            .iter()
            .filter_map(|dir| dir_name(dir).map(str::to_string))
            .collect())
    }

    /// Every database directory. A file beside them is not a database.
    fn database_dirs(&self) -> io::Result<Vec<PathBuf>> {
        let mut dirs = Vec::new();
        for entry in fs::read_dir(&self.data_dir)? {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path);
            }
        }
        Ok(dirs)
    }

    /// Deletes every `.tbl` file that no catalog names. A crash between a
    /// `CREATE TABLE` and the catalog save that follows it leaves one.
    pub fn sweep(&self) -> io::Result<()> {
        for dir in self.database_dirs()? {
            let catalog = Catalog::load(&dir)?;
            let named: Vec<String> = catalog.files().collect();
            for file in fs::read_dir(&dir)? {
                let path = file?.path();
                if path.extension().is_some_and(|ext| ext == "tbl")
                    && !named.iter().any(|known| path.ends_with(known))
                {
                    fs::remove_file(&path)?;
                    log::info!("deleted {}, which no catalog names", path.display());
                }
            }
        }
        Ok(())
    }
}

/// A database that one connection holds open. See [FR4].
pub struct Connected {
    pub db: Arc<Database>,
    registry: Arc<Registry>,
}

impl Drop for Connected {
    fn drop(&mut self) {
        let mut state = self.registry.state.lock().expect("the registry lock holds");
        // The name is always present, because `connect` put it there. The
        // default covers the impossible case without leaving a count behind.
        let count = state.connections.entry(self.db.name.clone()).or_insert(1);
        *count -= 1;
        if *count == 0 {
            state.connections.remove(&self.db.name);
        }
    }
}

/// The name a directory carries, when it is text at all.
fn dir_name(dir: &Path) -> Option<&str> {
    dir.file_name().and_then(|name| name.to_str())
}

fn exists(name: &str) -> DbError {
    named(
        ErrorCode::DatabaseExists,
        format!("a database named {name} exists"),
    )
}

/// The data directory itself would not read. That is the server's storage
/// and not the client's database, so it does not read as a missing database.
fn unreadable(e: io::Error) -> DbError {
    named(
        ErrorCode::StorageFull,
        format!("the data directory did not read: {e}"),
    )
}

fn unknown(name: &str) -> DbError {
    named(
        ErrorCode::UnknownDatabase,
        format!("no database named {name}"),
    )
}

/// A database name becomes a directory name, and a name from `Startup` never
/// passed the parser, so it is checked here.
///
/// ASCII only. A directory name reaches the filesystem, which may fold its
/// case and may normalise its characters, and neither is something the server
/// can answer for. Anything but a letter, a digit, or an underscore could
/// also reach outside the data directory.
fn check_name(name: &str) -> Result<(), DbError> {
    let allowed = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    match allowed {
        true => Ok(()),
        false => Err(named(
            ErrorCode::SyntaxError,
            format!("{name:?} is not a database name"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::catalog::testing::Dir;

    #[test]
    fn one_table_has_one_file_and_two_have_two() {
        let dir = Dir::new("table-files");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        let first = TableDef {
            id: 1,
            name: "item".to_string(),
            columns: Vec::new(),
            pk_index: 0,
        };
        let second = TableDef {
            id: 2,
            ..first.clone()
        };

        let file = registry.table_file("shop", &first).unwrap();
        assert_eq!(registry.table_file("shop", &first).unwrap(), file);
        assert_ne!(registry.table_file("shop", &second).unwrap(), file);
        assert!(dir.0.join("shop").join("1.tbl").is_file());
        assert!(dir.0.join("shop").join("2.tbl").is_file());
    }

    #[test]
    fn a_table_of_another_database_is_another_file() {
        let dir = Dir::new("table-files-two");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        registry.create("other").unwrap();
        let table = TableDef {
            id: 1,
            name: "item".to_string(),
            columns: Vec::new(),
            pk_index: 0,
        };
        assert_ne!(
            registry.table_file("shop", &table).unwrap(),
            registry.table_file("other", &table).unwrap()
        );
        assert!(dir.0.join("other").join("1.tbl").is_file());
    }

    #[test]
    fn a_database_is_made_and_then_opens() {
        let dir = Dir::new("make");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        let held = Registry::connect(&registry, "shop").unwrap();
        assert_eq!(held.db.name, "shop");
        assert!(dir.0.join("shop").join("catalog.json").is_file());
    }

    #[test]
    fn two_connections_share_one_database() {
        let dir = Dir::new("share");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        let first = Registry::connect(&registry, "shop").unwrap();
        let second = Registry::connect(&registry, "shop").unwrap();
        assert!(Arc::ptr_eq(&first.db, &second.db));
    }

    #[test]
    fn a_database_that_exists_is_refused() {
        let dir = Dir::new("exists");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        assert_eq!(
            registry.create("shop").unwrap_err().code,
            ErrorCode::DatabaseExists
        );
    }

    #[test]
    fn a_database_that_is_not_there_does_not_open_and_does_not_drop() {
        let dir = Dir::new("absent");
        let registry = dir.registry();
        assert_eq!(
            Registry::connect(&registry, "shop").err().unwrap().code,
            ErrorCode::UnknownDatabase
        );
        assert_eq!(
            registry.drop_database("shop").unwrap_err().code,
            ErrorCode::UnknownDatabase
        );
    }

    #[test]
    fn a_drop_frees_the_name_and_takes_the_tables_with_it() {
        let dir = Dir::new("drop");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        fs::write(dir.0.join("shop").join("1.tbl"), b"rows").unwrap();
        registry.drop_database("shop").unwrap();
        assert!(!dir.0.join("shop").exists());
        registry.create("shop").unwrap();
    }

    #[test]
    fn a_drop_is_refused_while_a_client_is_connected() {
        let dir = Dir::new("in-use");
        let registry = dir.registry();
        registry.create("shop").unwrap();

        let first = Registry::connect(&registry, "shop").unwrap();
        let second = Registry::connect(&registry, "shop").unwrap();
        assert_eq!(
            registry.drop_database("shop").unwrap_err().code,
            ErrorCode::DatabaseInUse
        );

        // One of two is not enough. The last one is.
        drop(first);
        assert_eq!(
            registry.drop_database("shop").unwrap_err().code,
            ErrorCode::DatabaseInUse
        );
        drop(second);
        registry.drop_database("shop").unwrap();
    }

    #[test]
    fn a_name_that_is_not_an_identifier_reaches_no_directory() {
        let dir = Dir::new("escape");
        let registry = dir.registry();
        for name in [
            "",
            "../escape",
            "a/b",
            "a.b",
            "shop ",
            "caffè",
            &"x".repeat(65),
        ] {
            assert_eq!(
                registry.create(name).unwrap_err().code,
                ErrorCode::SyntaxError,
                "create {name:?}"
            );
            assert_eq!(
                Registry::connect(&registry, name).err().unwrap().code,
                ErrorCode::SyntaxError,
                "connect {name:?}"
            );
            assert_eq!(
                registry.drop_database(name).unwrap_err().code,
                ErrorCode::SyntaxError,
                "drop {name:?}"
            );
        }
        assert_eq!(
            fs::read_dir(&dir.0).unwrap().count(),
            0,
            "no directory was made"
        );
    }

    #[test]
    fn a_sweep_deletes_a_table_file_that_no_catalog_names() {
        let dir = Dir::new("sweep");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        let db = dir.0.join("shop");

        // One file the catalog names, one left by a crash, one that is not a
        // table file at all.
        let held = Registry::connect(&registry, "shop").unwrap();
        {
            let mut catalog = held.db.catalog.lock().unwrap();
            catalog
                .add_table("item", Vec::new(), 0)
                .expect("the table is new");
            catalog.save().unwrap();
        }
        let kept = db.join("1.tbl");
        fs::write(&kept, b"rows").unwrap();
        fs::write(db.join("99.tbl"), b"orphan").unwrap();
        fs::write(db.join("notes.txt"), b"keep me").unwrap();
        // A file beside the databases, which is not a database directory.
        fs::write(dir.0.join("stray.tbl"), b"keep me").unwrap();

        registry.sweep().unwrap();

        assert!(kept.is_file(), "the catalog names this one");
        assert!(!db.join("99.tbl").exists(), "no catalog names this one");
        assert!(db.join("notes.txt").is_file());
        assert!(dir.0.join("stray.tbl").is_file());
    }

    #[test]
    fn a_sweep_of_an_empty_data_directory_does_nothing() {
        let dir = Dir::new("sweep-empty");
        dir.registry().sweep().unwrap();
    }

    #[test]
    fn two_names_that_differ_only_by_case_cannot_both_exist() {
        let dir = Dir::new("case");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        for name in ["Shop", "SHOP", "sHoP"] {
            assert_eq!(
                registry.create(name).unwrap_err().code,
                ErrorCode::DatabaseExists,
                "create {name}"
            );
        }
        // And the one that exists is reached only by its own spelling.
        assert!(Registry::connect(&registry, "shop").is_ok());
        for name in ["Shop", "SHOP"] {
            assert_eq!(
                Registry::connect(&registry, name).err().unwrap().code,
                ErrorCode::UnknownDatabase,
                "connect {name}"
            );
            assert_eq!(
                registry.drop_database(name).unwrap_err().code,
                ErrorCode::UnknownDatabase,
                "drop {name}"
            );
        }
    }

    #[test]
    fn a_new_data_directory_gets_one_database_and_no_more() {
        let dir = Dir::new("bootstrap");
        let registry = dir.registry();
        registry.bootstrap().unwrap();
        assert!(Registry::connect(&registry, FIRST_DATABASE).is_ok());

        // A directory that already holds a database is left alone.
        registry.drop_database(FIRST_DATABASE).unwrap();
        registry.create("shop").unwrap();
        registry.bootstrap().unwrap();
        assert_eq!(
            Registry::connect(&registry, FIRST_DATABASE)
                .err()
                .unwrap()
                .code,
            ErrorCode::UnknownDatabase,
            "a dropped database stays dropped"
        );
    }

    #[test]
    fn a_data_directory_that_is_gone_reports_itself() {
        let dir = Dir::new("gone");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        fs::remove_dir_all(&dir.0).unwrap();

        assert_eq!(
            registry.create("other").unwrap_err().code,
            ErrorCode::StorageFull
        );
        for e in [
            Registry::connect(&registry, "shop").err().unwrap(),
            registry.drop_database("shop").unwrap_err(),
        ] {
            assert_eq!(e.code, ErrorCode::StorageFull, "{e}");
            assert!(e.message.contains("did not read"), "{e}");
        }
    }

    /// Permissions are a Unix idea, and the project runs on Unix.
    #[cfg(unix)]
    #[test]
    fn a_database_that_will_not_be_made_reports_storage_full() {
        use std::os::unix::fs::PermissionsExt;

        let dir = Dir::new("unwritable");
        let registry = dir.registry();
        // Readable, so the name check still runs, but not writable.
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o555)).unwrap();
        let e = registry.create("shop").unwrap_err();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(e.code, ErrorCode::StorageFull);
        assert!(e.message.contains("was not made"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn a_database_that_will_not_delete_reports_storage_full() {
        use std::os::unix::fs::PermissionsExt;

        let dir = Dir::new("undeletable");
        let registry = dir.registry();
        registry.create("shop").unwrap();

        // A directory nothing may write is a directory nothing may empty.
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o555)).unwrap();
        let e = registry.drop_database("shop").unwrap_err();
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(e.code, ErrorCode::StorageFull);
        assert!(e.message.contains("was not deleted"), "{e}");
    }

    #[test]
    fn a_database_whose_catalog_will_not_read_does_not_open() {
        let dir = Dir::new("corrupt");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        fs::write(dir.0.join("shop").join("catalog.json"), b"not json").unwrap();
        let e = Registry::connect(&registry, "shop").err().unwrap();
        assert_eq!(e.code, ErrorCode::UnknownDatabase);
        assert!(e.message.contains("did not open"), "{e}");
        assert!(registry.sweep().is_err());
    }
}
