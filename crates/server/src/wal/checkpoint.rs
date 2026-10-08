//! Moving the log into the table files.

use std::collections::HashMap;
use std::sync::Arc;

use protocol::DbError;

use crate::store::pool::BufferPool;
use crate::wal::writer::Wal;

/// How large a log grows before a checkpoint empties it.
pub const THRESHOLD: u64 = 64 * 1024 * 1024;

/// Writes every page the log holds into its table file, then empties the log.
///
/// One page at a time, so a log of any size costs one page of memory. A
/// failure leaves the log alone, so the pages are still in it and the next
/// checkpoint tries again.
pub fn run(pool: &BufferPool, wal: &Arc<Wal>) -> Result<(), DbError> {
    let mut by_table: HashMap<u32, Vec<(u32, u64)>> = HashMap::new();
    for (table, page_no, at) in wal.pages() {
        by_table.entry(table).or_default().push((page_no, at));
    }
    for (table, frames) in by_table {
        for (page_no, at) in frames {
            let page = wal.page_at(at)?;
            pool.write_to_table(wal, table, page_no, &page)?;
        }
        pool.sync_table(wal, table)?;
    }
    wal.truncate()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::catalog::testing::{self, Dir};
    use crate::store::page::{
        HEADER_SIZE, PAGE_SIZE, PageId, PageKind, SlottedPage, page_u32, write_u32,
    };
    use crate::wal::writer::Wal;

    /// Writes the page number into a page, so a read can prove which one it
    /// got back.
    fn stamp(pool: &BufferPool, id: PageId) {
        let mut page = pool.fetch(id).expect("the page fetches");
        let bytes = page.bytes_mut();
        SlottedPage::init(bytes, PageKind::Leaf);
        write_u32(bytes, HEADER_SIZE, id.page_no);
    }

    fn stamped(pool: &BufferPool, id: PageId) -> u32 {
        let page = pool.fetch(id).expect("the page fetches");
        page_u32(page.bytes(), HEADER_SIZE)
    }

    #[test]
    fn a_checkpoint_puts_the_pages_in_their_file_and_empties_the_log() {
        let (dir, pool, wal, file) = testing::table("checkpoint", 8);
        let pages: Vec<PageId> = (0..4).map(|_| pool.allocate(file).unwrap()).collect();
        for id in &pages {
            stamp(&pool, *id);
        }
        pool.commit(&wal).unwrap();
        assert!(wal.end() > 0, "the log holds the pages");

        run(&pool, &wal).unwrap();
        assert_eq!(wal.end(), 0, "the log is empty");
        assert!(wal.pages().is_empty());

        // A pool and a log of their own, so every page comes off the file.
        let fresh = BufferPool::with_frames(8);
        let empty = Arc::new(Wal::open(&dir.0, 64 * 1024 * 1024).unwrap());
        let file = fresh
            .open(&dir.0.join("1.tbl"), 1, Arc::clone(&empty))
            .unwrap();
        assert_eq!(empty.end(), 0, "nothing is left in the log to read from");
        for id in &pages {
            assert_eq!(stamped(&fresh, PageId::new(file, id.page_no)), id.page_no);
        }
    }

    #[test]
    fn a_checkpoint_of_an_empty_log_does_nothing() {
        let (_dir, pool, wal, _file) = testing::table("checkpoint-empty", 8);
        run(&pool, &wal).unwrap();
        assert_eq!(wal.end(), 0);
    }

    #[test]
    fn the_newest_form_of_a_page_is_the_one_that_reaches_the_file() {
        let (dir, pool, wal, file) = testing::table("checkpoint-newest", 8);
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        pool.commit(&wal).unwrap();
        {
            let mut page = pool.fetch(id).unwrap();
            write_u32(page.bytes_mut(), HEADER_SIZE, 777);
        }
        pool.commit(&wal).unwrap();

        run(&pool, &wal).unwrap();

        let fresh = BufferPool::with_frames(8);
        let empty = Arc::new(Wal::open(&dir.0, 64 * 1024 * 1024).unwrap());
        let file = fresh.open(&dir.0.join("1.tbl"), 1, empty).unwrap();
        assert_eq!(stamped(&fresh, PageId::new(file, id.page_no)), 777);
    }

    #[test]
    fn the_pages_of_two_tables_reach_two_files() {
        let dir = Dir::new("checkpoint-two");
        let pool = BufferPool::with_frames(8);
        let wal = Arc::new(Wal::open(&dir.0, 64 * 1024 * 1024).unwrap());
        let first = pool
            .open(&dir.0.join("1.tbl"), 1, Arc::clone(&wal))
            .unwrap();
        let second = pool
            .open(&dir.0.join("2.tbl"), 2, Arc::clone(&wal))
            .unwrap();
        let one = pool.allocate(first).unwrap();
        let two = pool.allocate(second).unwrap();
        stamp(&pool, one);
        stamp(&pool, two);
        pool.commit(&wal).unwrap();

        run(&pool, &wal).unwrap();
        assert_eq!(wal.end(), 0);
        for path in ["1.tbl", "2.tbl"] {
            let len = std::fs::metadata(dir.0.join(path)).unwrap().len();
            assert!(len >= 2 * PAGE_SIZE as u64, "{path} holds its pages");
        }
    }

    #[test]
    fn a_table_file_that_cannot_sync_leaves_the_log_alone() {
        let dir = Dir::new("checkpoint-no-sync");
        let pool = BufferPool::with_frames(8);
        let wal = Arc::new(Wal::open(&dir.0, 64 * 1024 * 1024).unwrap());
        // A table file that takes every write and settles none of them.
        let path = dir.0.join("1.tbl");
        std::os::unix::fs::symlink("/dev/null", &path).unwrap();
        pool.open(&path, 1, Arc::clone(&wal)).unwrap();
        wal.append_page(1, 1, &Box::new([0; PAGE_SIZE])).unwrap();
        wal.commit().unwrap();

        let e = run(&pool, &wal).err().unwrap();
        assert!(e.message.contains("did not sync"), "{e}");
        assert!(wal.end() > 0, "the pages are still in the log");
    }

    #[test]
    fn a_log_of_a_table_the_pool_never_opened_cannot_be_checkpointed() {
        let (_dir, pool, wal, _file) = testing::table("checkpoint-stranger", 8);
        // A frame for a table that no file of this pool holds.
        wal.append_page(99, 1, &Box::new([0; PAGE_SIZE])).unwrap();
        wal.commit().unwrap();
        let e = run(&pool, &wal).err().unwrap();
        assert!(e.message.contains("holds no table 99"), "{e}");
        assert!(
            wal.end() > 0,
            "a checkpoint that failed leaves the log alone"
        );
    }
}
