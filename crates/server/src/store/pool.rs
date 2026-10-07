//! The buffer pool: a fixed count of frames, shared by every database.
//!
//! One pool for the server, because the memory limit is for the server. A
//! frame is claimed by the clock method, which sweeps the frames, clears a
//! reference bit that is set, and takes the first frame whose bit is clear
//! and which no one has pinned.
//!
//! Positioned reads and writes, so a file needs no cursor and no lock of its
//! own. That is a Unix call, which is what this server runs on.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use protocol::DbError;

use crate::store::page::{FileHeader, FileId, PAGE_SIZE, Page, PageHeader, PageId, PageKind};
use crate::store::storage_error;

/// The share of the memory limit that the budget gives the pool, as 640 MB
/// of every gigabyte.
const SHARE: (u64, u64) = (5, 8);

/// A pool holds page 0 of a file and one more page at the same time, which is
/// what allocating a page needs.
const LEAST_FRAMES: usize = 2;

/// What the pool knows about one frame.
#[derive(Clone, Copy, Default)]
struct Meta {
    /// The page the frame holds, or `None` for a frame that holds none.
    page: Option<PageId>,
    pins: u32,
    dirty: bool,
    /// The reference bit that the clock clears before it takes a frame.
    used: bool,
}

/// The files the pool has open.
#[derive(Default)]
struct Files {
    open: Vec<Arc<File>>,
    ids: HashMap<PathBuf, FileId>,
}

impl Files {
    fn file(&self, id: FileId) -> Result<Arc<File>, DbError> {
        self.open
            .get(id.0 as usize)
            .cloned()
            .ok_or_else(|| storage_error(format!("the pool holds no file {}", id.0)))
    }
}

/// Everything the pool decides under one lock, so two frames never disagree
/// about which page they hold.
struct Inner {
    map: HashMap<PageId, usize>,
    meta: Vec<Meta>,
    hand: usize,
    files: Files,
}

/// Pages in memory, written to their files when their frames are reused.
pub struct BufferPool {
    /// The page bytes, one lock each, so two callers can hold two pages.
    frames: Vec<Mutex<Box<Page>>>,
    inner: Mutex<Inner>,
}

impl BufferPool {
    /// A pool sized from the memory limit of the server.
    pub fn new(mem_limit: u64) -> BufferPool {
        let bytes = mem_limit / SHARE.1 * SHARE.0;
        BufferPool::with_frames((bytes / PAGE_SIZE as u64) as usize)
    }

    pub fn with_frames(frames: usize) -> BufferPool {
        let frames = frames.max(LEAST_FRAMES);
        BufferPool {
            frames: (0..frames)
                .map(|_| Mutex::new(Box::new([0; PAGE_SIZE])))
                .collect(),
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                meta: vec![Meta::default(); frames],
                hand: 0,
                files: Files::default(),
            }),
        }
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// How many frames hold a page. It never passes the frame count, which is
    /// what keeps the memory of the server off the size of the data.
    pub fn held(&self) -> usize {
        let inner = self.inner.lock().expect("the pool lock holds");
        inner.meta.iter().filter(|meta| meta.page.is_some()).count()
    }

    /// Opens a table file, or returns the id it already has. A file with no
    /// bytes gets its header page, so page 0 always reads as one.
    pub fn open(&self, path: &Path) -> Result<FileId, DbError> {
        let mut inner = self.inner.lock().expect("the pool lock holds");
        if let Some(&id) = inner.files.ids.get(path) {
            return Ok(id);
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| storage_error(format!("{} did not open: {e}", path.display())))?;
        let len = file
            .metadata()
            .map_err(|e| storage_error(format!("{} did not read: {e}", path.display())))?
            .len();
        if len == 0 {
            let mut page = [0; PAGE_SIZE];
            FileHeader {
                root: 0,
                free_head: 0,
            }
            .write(&mut page);
            write_page(&file, 0, &page)?;
        }
        let id = FileId(inner.files.open.len() as u32);
        inner.files.open.push(Arc::new(file));
        inner.files.ids.insert(path.to_path_buf(), id);
        Ok(id)
    }

    /// Pins a page and hands back its bytes. The pin falls when the guard
    /// drops.
    pub fn fetch(&self, id: PageId) -> Result<PageGuard<'_>, DbError> {
        let frame = self.pin(id)?;
        Ok(PageGuard {
            pool: self,
            id,
            bytes: self.frames[frame].lock().expect("the frame lock holds"),
            dirty: false,
        })
    }

    /// Takes a page for the caller to write. The bytes of a page that the
    /// file does not hold yet are zeros, so the caller writes a header.
    pub fn allocate(&self, file: FileId) -> Result<PageId, DbError> {
        let mut header_page = self.fetch(PageId::new(file, 0))?;
        let mut header = FileHeader::read(header_page.bytes())?;
        if header.free_head == 0 {
            // No spare page, so the file grows by one.
            return Ok(PageId::new(file, self.grow(file)?));
        }
        let taken = header.free_head;
        let next = {
            let spare = self.fetch(PageId::new(file, taken))?;
            PageHeader::read(spare.bytes())?.next
        };
        header.free_head = next;
        header.write(header_page.bytes_mut());
        Ok(PageId::new(file, taken))
    }

    /// Returns a page to the free list of its file.
    pub fn free(&self, id: PageId) -> Result<(), DbError> {
        if id.page_no == 0 {
            return Err(storage_error("page 0 is the header of its file"));
        }
        let mut header_page = self.fetch(PageId::new(id.file, 0))?;
        let mut header = FileHeader::read(header_page.bytes())?;
        {
            let mut page = self.fetch(id)?;
            let bytes = page.bytes_mut();
            // The free list links through the page header, like a chain.
            PageHeader {
                lsn: 0,
                kind: PageKind::Free,
                slot_count: 0,
                free_offset: PAGE_SIZE as u16,
                next: header.free_head,
                prev: 0,
            }
            .write(bytes);
        }
        header.free_head = id.page_no;
        header.write(header_page.bytes_mut());
        Ok(())
    }

    /// Writes every page that was changed. No page may be held, because a
    /// held page is one the caller may still be writing to.
    pub fn flush_all(&self) -> Result<(), DbError> {
        let mut inner = self.inner.lock().expect("the pool lock holds");
        for frame in 0..self.frames.len() {
            let meta = inner.meta[frame];
            let Some(page) = meta.page.filter(|_| meta.dirty) else {
                continue;
            };
            let file = inner.files.file(page.file)?;
            let Ok(bytes) = self.frames[frame].try_lock() else {
                return Err(storage_error("a page is held while the pool flushes"));
            };
            write_page(&file, page.page_no, &bytes)?;
            inner.meta[frame].dirty = false;
        }
        Ok(())
    }

    /// Finds the frame of a page, reading it in when the pool holds it not.
    ///
    /// The read happens under the lock, so no caller can reach a frame before
    /// its bytes arrive. That serialises the reads of the whole pool, which is
    /// the ceiling: milestone 10 wants readers to run beside one another, and
    /// will need a per-frame state that says "being read".
    fn pin(&self, id: PageId) -> Result<usize, DbError> {
        let mut inner = self.inner.lock().expect("the pool lock holds");
        if let Some(&frame) = inner.map.get(&id) {
            inner.meta[frame].pins += 1;
            inner.meta[frame].used = true;
            return Ok(frame);
        }
        let frame = self.claim(&mut inner)?;
        let file = inner.files.file(id.file)?;
        {
            let mut bytes = self.frames[frame].lock().expect("the frame lock holds");
            read_page(&file, id.page_no, &mut bytes)?;
        }
        inner.map.insert(id, frame);
        inner.meta[frame] = Meta {
            page: Some(id),
            pins: 1,
            dirty: false,
            used: true,
        };
        Ok(frame)
    }

    /// An empty frame, by the clock method. Two sweeps are enough: the first
    /// clears the reference bits, and the second finds one that is clear.
    fn claim(&self, inner: &mut Inner) -> Result<usize, DbError> {
        let frames = self.frames.len();
        for _ in 0..frames * 2 {
            let frame = inner.hand;
            inner.hand = (inner.hand + 1) % frames;
            let meta = inner.meta[frame];
            if meta.pins > 0 {
                continue;
            }
            if meta.used {
                inner.meta[frame].used = false;
                continue;
            }
            if let Some(page) = meta.page {
                if meta.dirty {
                    let file = inner.files.file(page.file)?;
                    let bytes = self.frames[frame].lock().expect("the frame lock holds");
                    write_page(&file, page.page_no, &bytes)?;
                }
                inner.map.remove(&page);
            }
            inner.meta[frame] = Meta::default();
            return Ok(frame);
        }
        Err(storage_error(format!(
            "every one of the {frames} pool frames is held"
        )))
    }

    /// Adds a page to the end of a file and returns its number.
    fn grow(&self, file: FileId) -> Result<u32, DbError> {
        let inner = self.inner.lock().expect("the pool lock holds");
        let handle = inner.files.file(file)?;
        let len = handle
            .metadata()
            .map_err(|e| storage_error(format!("the file did not read: {e}")))?
            .len();
        handle
            .set_len(len + PAGE_SIZE as u64)
            .map_err(|e| storage_error(format!("the file did not grow: {e}")))?;
        Ok((len / PAGE_SIZE as u64) as u32)
    }

    /// A pinned page is always mapped, because only a claim unmaps one and a
    /// claim passes over every frame that is pinned.
    fn unpin(&self, id: PageId) {
        let mut inner = self.inner.lock().expect("the pool lock holds");
        let frame = inner.map[&id];
        inner.meta[frame].pins -= 1;
    }

    /// Marks a page as changed, the moment the caller asks to write it, so a
    /// flush can see a page that is still being written.
    fn mark_dirty(&self, id: PageId) {
        let mut inner = self.inner.lock().expect("the pool lock holds");
        let frame = inner.map[&id];
        inner.meta[frame].dirty = true;
    }
}

/// A page pinned in the pool. The pin falls when this drops, and the pool
/// writes the page later if the caller changed it.
pub struct PageGuard<'a> {
    pool: &'a BufferPool,
    id: PageId,
    bytes: MutexGuard<'a, Box<Page>>,
    dirty: bool,
}

impl PageGuard<'_> {
    pub fn bytes(&self) -> &Page {
        &self.bytes
    }

    /// The bytes, to change. The page is written to its file before its frame
    /// is reused. The pool learns of the change now and not when this guard
    /// drops, so a flush while the page is held does not pass it over.
    pub fn bytes_mut(&mut self) -> &mut Page {
        if !self.dirty {
            self.dirty = true;
            self.pool.mark_dirty(self.id);
        }
        &mut self.bytes
    }
}

impl Drop for PageGuard<'_> {
    fn drop(&mut self) {
        self.pool.unpin(self.id);
    }
}

fn read_page(file: &File, page_no: u32, into: &mut Page) -> Result<(), DbError> {
    file.read_exact_at(into, offset_of(page_no))
        .map_err(|e| storage_error(format!("page {page_no} did not read: {e}")))
}

fn write_page(file: &File, page_no: u32, from: &Page) -> Result<(), DbError> {
    file.write_all_at(from, offset_of(page_no))
        .map_err(|e| storage_error(format!("page {page_no} did not write: {e}")))
}

fn offset_of(page_no: u32) -> u64 {
    u64::from(page_no) * PAGE_SIZE as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::testing::Dir;
    use crate::store::page::{HEADER_SIZE, SlottedPage, page_u32, write_u32};

    /// A pool of `frames` frames, and one open table file.
    fn pool(label: &str, frames: usize) -> (Dir, BufferPool, FileId) {
        let dir = Dir::new(label);
        let pool = BufferPool::with_frames(frames);
        let file = pool.open(&dir.0.join("1.tbl")).expect("the file opens");
        (dir, pool, file)
    }

    /// Writes the page number into a page, so a read can prove which page it
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
    fn a_new_file_opens_with_a_header_page() {
        let (_dir, pool, file) = pool("new-file", 4);
        let page = pool.fetch(PageId::new(file, 0)).unwrap();
        assert_eq!(
            FileHeader::read(page.bytes()).unwrap(),
            FileHeader {
                root: 0,
                free_head: 0
            }
        );
    }

    #[test]
    fn the_same_path_opens_once() {
        let (dir, pool, file) = pool("same-path", 4);
        let again = pool.open(&dir.0.join("1.tbl")).unwrap();
        assert_eq!(file, again);
        let other = pool.open(&dir.0.join("2.tbl")).unwrap();
        assert_ne!(file, other);
    }

    #[test]
    fn a_path_that_will_not_open_is_an_error() {
        let (dir, pool, _) = pool("bad-path", 4);
        let e = pool.open(&dir.0.join("no").join("where.tbl")).unwrap_err();
        assert!(e.message.contains("did not open"), "{e}");
    }

    #[test]
    fn a_file_the_pool_never_opened_is_an_error() {
        let (_dir, pool, _) = pool("no-file", 4);
        let e = pool.fetch(PageId::new(FileId(99), 0)).err().unwrap();
        assert!(e.message.contains("holds no file 99"), "{e}");
    }

    #[test]
    fn a_page_read_after_its_frame_was_reused_holds_what_was_written() {
        let (_dir, pool, file) = pool("round-trip", LEAST_FRAMES);
        let pages: Vec<PageId> = (0..8).map(|_| pool.allocate(file).unwrap()).collect();
        for id in &pages {
            stamp(&pool, *id);
        }
        // Eight pages through two frames, so every frame was reused.
        for id in &pages {
            assert_eq!(stamped(&pool, *id), id.page_no);
        }
    }

    #[test]
    fn a_pinned_page_is_never_evicted() {
        let (_dir, pool, file) = pool("pinned", LEAST_FRAMES);
        let first = pool.allocate(file).unwrap();
        stamp(&pool, first);

        let held = pool.fetch(first).unwrap();
        // Every other frame is now taken by pages that come and go.
        for _ in 0..8 {
            let id = pool.allocate(file).unwrap();
            stamp(&pool, id);
        }
        assert_eq!(page_u32(held.bytes(), HEADER_SIZE), first.page_no);
    }

    #[test]
    fn a_pool_whose_every_frame_is_held_refuses() {
        let (_dir, pool, file) = pool("full", LEAST_FRAMES);
        let one = pool.allocate(file).unwrap();
        let two = pool.allocate(file).unwrap();
        let _first = pool.fetch(one).unwrap();
        let _second = pool.fetch(two).unwrap();
        let e = pool.fetch(PageId::new(file, 0)).err().unwrap();
        assert!(e.message.contains("frames is held"), "{e}");
    }

    #[test]
    fn a_changed_page_reaches_its_file() {
        let dir = Dir::new("durable");
        let path = dir.0.join("1.tbl");
        let id = {
            let pool = BufferPool::with_frames(4);
            let file = pool.open(&path).unwrap();
            let id = pool.allocate(file).unwrap();
            stamp(&pool, id);
            pool.flush_all().unwrap();
            id
        };
        // A pool of its own, so nothing is read from memory.
        let pool = BufferPool::with_frames(4);
        let file = pool.open(&path).unwrap();
        assert_eq!(stamped(&pool, PageId::new(file, id.page_no)), id.page_no);
    }

    #[test]
    fn a_page_nobody_changed_is_not_written() {
        let (_dir, pool, file) = pool("clean", 4);
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        pool.flush_all().unwrap();
        // Reading leaves the page clean, so a second flush writes nothing and
        // cannot fail.
        assert_eq!(stamped(&pool, id), id.page_no);
        pool.flush_all().unwrap();
    }

    #[test]
    fn a_flush_while_a_page_is_held_is_an_error_and_not_a_wait() {
        let (_dir, pool, file) = pool("held-flush", 4);
        let id = pool.allocate(file).unwrap();
        let mut page = pool.fetch(id).unwrap();
        SlottedPage::init(page.bytes_mut(), PageKind::Leaf);
        let e = pool.flush_all().unwrap_err();
        assert!(e.message.contains("held while the pool flushes"), "{e}");
    }

    #[test]
    fn a_freed_page_is_handed_out_again() {
        let (_dir, pool, file) = pool("free-list", 4);
        let first = pool.allocate(file).unwrap();
        let second = pool.allocate(file).unwrap();
        assert_ne!(first.page_no, second.page_no);

        pool.free(second).unwrap();
        let again = pool.allocate(file).unwrap();
        assert_eq!(again.page_no, second.page_no, "the free list was used");

        // And the list empties, so the next page grows the file.
        let fresh = pool.allocate(file).unwrap();
        assert!(fresh.page_no > second.page_no);
    }

    #[test]
    fn a_free_list_of_more_than_one_page_empties_in_order() {
        let (_dir, pool, file) = pool("free-many", 4);
        let pages: Vec<PageId> = (0..3).map(|_| pool.allocate(file).unwrap()).collect();
        for id in &pages {
            pool.free(*id).unwrap();
        }
        // The list is a stack, so the last freed page comes back first.
        for id in pages.iter().rev() {
            assert_eq!(pool.allocate(file).unwrap().page_no, id.page_no);
        }
    }

    #[test]
    fn the_header_page_is_never_freed() {
        let (_dir, pool, file) = pool("free-header", 4);
        let e = pool.free(PageId::new(file, 0)).unwrap_err();
        assert!(e.message.contains("header of its file"), "{e}");
    }

    #[test]
    fn the_frame_count_comes_from_the_memory_limit() {
        // The budget gives the pool 640 MB of every gigabyte.
        assert_eq!(BufferPool::new(1024 * 1024 * 1024).frame_count(), 81920);
        // And a limit too small to divide still leaves a working pool.
        assert_eq!(BufferPool::new(0).frame_count(), LEAST_FRAMES);
        assert_eq!(BufferPool::with_frames(0).frame_count(), LEAST_FRAMES);
    }

    #[test]
    fn sixteen_megabytes_through_a_small_pool_reads_back() {
        // A pool of 1 MB against 16 MB of pages, so most pages are evicted
        // and read again.
        let frames = 1024 * 1024 / PAGE_SIZE;
        let (_dir, pool, file) = pool("load", frames);
        let pages = 16 * 1024 * 1024 / PAGE_SIZE;

        let mut written = Vec::with_capacity(pages);
        for _ in 0..pages {
            let id = pool.allocate(file).unwrap();
            stamp(&pool, id);
            written.push(id);
            assert!(
                pool.held() <= pool.frame_count(),
                "the pool grew past its cap"
            );
        }
        for id in &written {
            assert_eq!(stamped(&pool, *id), id.page_no);
        }
        assert_eq!(pool.held(), pool.frame_count(), "every frame is in use");
        assert_eq!(pool.frame_count(), frames);
    }

    #[test]
    fn the_smallest_pool_still_reads_and_writes() {
        let (_dir, pool, file) = pool("smallest", 1);
        assert_eq!(pool.frame_count(), LEAST_FRAMES);
        let pages: Vec<PageId> = (0..4).map(|_| pool.allocate(file).unwrap()).collect();
        for id in &pages {
            stamp(&pool, *id);
        }
        for id in &pages {
            assert_eq!(stamped(&pool, *id), id.page_no);
        }
    }

    #[test]
    fn a_page_past_the_end_of_its_file_is_an_error() {
        let (_dir, pool, file) = pool("past-end", 4);
        let e = pool.fetch(PageId::new(file, 999)).err().unwrap();
        assert!(e.message.contains("did not read"), "{e}");
    }
}
