//! A value too large for a record, held in a chain of pages of its own.
//!
//! Each page of a chain carries the number of the page that follows it, and
//! the last carries zero. A record keeps only the head and the length.

use protocol::DbError;

use crate::store::codec::ChainPtr;
use crate::store::page::{FileId, HEADER_SIZE, PAGE_SIZE, PageHeader, PageId, PageKind};
use crate::store::pool::BufferPool;
use crate::store::storage_error;

/// Where the bytes of a chain page begin. The link to the page that follows
/// lives in the page header, so nothing sits between it and the bytes.
const DATA_AT: usize = HEADER_SIZE;

/// How many bytes one page of a chain carries.
pub const PER_PAGE: usize = PAGE_SIZE - DATA_AT;

/// Writes a value into a chain and returns the pointer that a record keeps.
pub fn write(pool: &BufferPool, file: FileId, value: &[u8]) -> Result<ChainPtr, DbError> {
    // Built from the end, so every page knows the one that follows it before
    // it is written, and no page is ever written twice.
    let mut next = 0;
    for chunk in value.chunks(PER_PAGE).rev() {
        let id = pool.allocate(file)?;
        let mut page = pool.fetch(id)?;
        let bytes = page.bytes_mut();
        PageHeader {
            lsn: 0,
            kind: PageKind::Overflow,
            slot_count: 0,
            free_offset: PAGE_SIZE as u16,
            next,
            prev: 0,
        }
        .write(bytes);
        bytes[DATA_AT..DATA_AT + chunk.len()].copy_from_slice(chunk);
        next = id.page_no;
    }
    Ok(ChainPtr {
        head: next,
        len: value.len() as u64,
    })
}

/// Reads a value back out of its chain.
pub fn read(pool: &BufferPool, file: FileId, ptr: ChainPtr) -> Result<Vec<u8>, DbError> {
    let len = ptr.len as usize;
    // The length comes off a page, so it is not trusted to size a buffer.
    let mut value = Vec::new();
    let mut page_no = ptr.head;
    while value.len() < len {
        if page_no == 0 {
            return Err(storage_error("the chain ends before its value does"));
        }
        let page = pool.fetch(PageId::new(file, page_no))?;
        let header = PageHeader::read(page.bytes())?;
        if header.kind != PageKind::Overflow {
            return Err(storage_error("the chain reaches a page that is not a link"));
        }
        let wanted = (len - value.len()).min(PER_PAGE);
        value.extend_from_slice(&page.bytes()[DATA_AT..DATA_AT + wanted]);
        page_no = header.next;
    }
    Ok(value)
}

/// Returns every page of a chain to the free list of its file.
pub fn free(pool: &BufferPool, file: FileId, ptr: ChainPtr) -> Result<(), DbError> {
    let mut page_no = ptr.head;
    while page_no != 0 {
        let id = PageId::new(file, page_no);
        // The link is read before the page joins the free list, because
        // joining it writes over the link.
        let next = {
            let page = pool.fetch(id)?;
            PageHeader::read(page.bytes())?.next
        };
        pool.free(id)?;
        page_no = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::catalog::testing::{self, Dir};
    use crate::store::codec::INLINE_LIMIT;
    use crate::store::page::SlottedPage;
    use crate::wal::writer::Wal;

    fn pool(label: &str, frames: usize) -> (Dir, BufferPool, Arc<Wal>, FileId) {
        testing::table(label, frames)
    }

    /// The page that follows one page of a chain.
    fn link(pool: &BufferPool, file: FileId, page_no: u32) -> u32 {
        let page = pool
            .fetch(PageId::new(file, page_no))
            .expect("the page fetches");
        PageHeader::read(page.bytes())
            .expect("the page has a header")
            .next
    }

    #[test]
    fn a_value_of_a_megabyte_reads_back_byte_for_byte() {
        let (_dir, pool, _wal, file) = pool("megabyte", 8);
        let value: Vec<u8> = (0..1024 * 1024).map(|n| (n % 251) as u8).collect();
        let ptr = write(&pool, file, &value).unwrap();
        assert_eq!(ptr.len, value.len() as u64);
        assert!(ptr.head > 0);
        assert_eq!(read(&pool, file, ptr).unwrap(), value);
    }

    #[test]
    fn a_value_just_over_the_record_limit_takes_one_page() {
        let (_dir, pool, _wal, file) = pool("one-page", 4);
        let value = vec![7; INLINE_LIMIT + 1];
        let ptr = write(&pool, file, &value).unwrap();
        assert_eq!(
            link(&pool, file, ptr.head),
            0,
            "one page, so no page follows"
        );
        assert_eq!(read(&pool, file, ptr).unwrap(), value);
    }

    #[test]
    fn a_value_that_exactly_fills_its_pages_reads_back() {
        let (_dir, pool, _wal, file) = pool("exact", 4);
        for pages in 1..4 {
            let value = vec![3; PER_PAGE * pages];
            let ptr = write(&pool, file, &value).unwrap();
            assert_eq!(read(&pool, file, ptr).unwrap(), value, "{pages} pages");
        }
    }

    #[test]
    fn a_chain_survives_the_eviction_of_its_middle() {
        // Two frames against a chain of many pages, so each page is read
        // again after its frame was reused.
        let (_dir, pool, _wal, file) = pool("evicted", 1);
        let value: Vec<u8> = (0..PER_PAGE as u32 * 5).map(|n| (n % 253) as u8).collect();
        let ptr = write(&pool, file, &value).unwrap();
        assert_eq!(read(&pool, file, ptr).unwrap(), value);
    }

    #[test]
    fn a_freed_chain_gives_every_page_back() {
        let (_dir, pool, _wal, file) = pool("freed", 4);
        let value = vec![1; PER_PAGE * 3];
        let ptr = write(&pool, file, &value).unwrap();
        let after_write = pool.allocate(file).unwrap().page_no;

        free(&pool, file, ptr).unwrap();
        // Three pages came back, so the next three allocations take them
        // rather than growing the file.
        let reused: Vec<u32> = (0..3)
            .map(|_| pool.allocate(file).unwrap().page_no)
            .collect();
        assert!(
            reused.iter().all(|page| *page < after_write),
            "the free list was used, got {reused:?}"
        );
    }

    #[test]
    fn an_empty_value_needs_no_page() {
        let (_dir, pool, _wal, file) = pool("empty", 4);
        let ptr = write(&pool, file, &[]).unwrap();
        assert_eq!(ptr, ChainPtr { head: 0, len: 0 });
        assert_eq!(read(&pool, file, ptr).unwrap(), Vec::<u8>::new());
        free(&pool, file, ptr).unwrap();
    }

    #[test]
    fn a_chain_that_ends_too_soon_is_an_error() {
        let (_dir, pool, _wal, file) = pool("short", 4);
        let ptr = write(&pool, file, &vec![1; PER_PAGE]).unwrap();
        // The record claims more than the chain holds.
        let lying = ChainPtr {
            head: ptr.head,
            len: ptr.len + 1,
        };
        let e = read(&pool, file, lying).err().unwrap();
        assert!(e.message.contains("ends before its value"), "{e}");
    }

    #[test]
    fn a_chain_that_reaches_another_kind_of_page_is_an_error() {
        let (_dir, pool, _wal, file) = pool("wrong-kind", 4);
        let id = pool.allocate(file).unwrap();
        SlottedPage::init(pool.fetch(id).unwrap().bytes_mut(), PageKind::Leaf);
        let e = read(
            &pool,
            file,
            ChainPtr {
                head: id.page_no,
                len: 10,
            },
        )
        .err()
        .unwrap();
        assert!(e.message.contains("not a link"), "{e}");
    }

    #[test]
    fn a_chain_that_starts_nowhere_is_an_error() {
        let (_dir, pool, _wal, file) = pool("no-head", 4);
        let e = read(&pool, file, ChainPtr { head: 0, len: 1 })
            .err()
            .unwrap();
        assert!(e.message.contains("ends before its value"), "{e}");
    }
}
