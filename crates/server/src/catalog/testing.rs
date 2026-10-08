//! Helpers that only a test uses.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::catalog::registry::{Connected, Registry};
use crate::store::page::FileId;
use crate::store::pool::BufferPool;
use crate::wal::writer::Wal;

/// A directory of its own for one test, gone when it drops. The name holds
/// the thread, so tests that run at the same time never share one.
pub struct Dir(pub PathBuf);

impl Dir {
    pub fn new(label: &str) -> Dir {
        let path = std::env::temp_dir().join(format!(
            "garibaldb-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the test directory is made");
        Dir(path)
    }

    pub fn registry(&self) -> Arc<Registry> {
        // A pool of a few hundred frames, which is plenty for a test and
        // keeps every one of them cheap to build.
        Arc::new(
            Registry::new(&self.0, 4 * 1024 * 1024, 64 * 1024 * 1024)
                .expect("the data directory opens"),
        )
    }

    /// A registry holding one database named `shop`, and that database open.
    pub fn shop(&self) -> (Arc<Registry>, Connected) {
        let registry = self.registry();
        registry.create("shop").expect("the database is new");
        let held = Registry::connect(&registry, "shop").expect("the database opens");
        (registry, held)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A pool, a log, and one open table file, in a directory of their own.
///
/// Every test of the store needs the three together, because the pool writes
/// through the log and reads back through it.
pub fn table(label: &str, frames: usize) -> (Dir, BufferPool, Arc<Wal>, FileId) {
    let dir = Dir::new(label);
    let pool = BufferPool::with_frames(frames);
    let wal = Arc::new(Wal::open(&dir.0, 64 * 1024 * 1024).expect("the log opens"));
    let file = pool
        .open(&dir.0.join("1.tbl"), 1, Arc::clone(&wal))
        .expect("the file opens");
    (dir, pool, wal, file)
}
