//! Every case file under `tests/cases/`.
//!
//! A milestone that adds a statement adds a file here, and no test.

mod h1;

use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn every_case_file_passes() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.expect("the directory reads").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "test"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no case file in {}", dir.display());
    for file in &files {
        h1::run_file(file);
    }
}
