//! Helpers that only a test uses.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use crate::catalog::registry::{Connected, Registry};

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
        Arc::new(Registry::new(&self.0).expect("the data directory opens"))
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
