//! The buffer pool: a fixed count of frames, shared by every database.
//!
//! One pool for the server, because the memory limit is for the server. A
//! frame is claimed by the clock method, which sweeps the frames, clears a
//! reference bit that is set, and takes the first frame whose bit is clear
//! and which no one has pinned.
//!
//! Positioned reads and writes, so a file needs no cursor and no lock of its
//! own. That is a Unix call, which is what this server runs on.
//!
//! Three locks, always taken in this order: the pool, then the bytes of a
//! frame, then the log. The pin count and the dirty mark of a frame sit
//! outside the pool lock, so whoever holds the bytes of a page never waits
//! for the pool and the order holds.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use protocol::DbError;

use crate::store::page::{FileHeader, FileId, PAGE_SIZE, Page, PageHeader, PageId, PageKind};
use crate::store::storage_error;
use crate::wal::writer::Wal;

/// The share of the memory limit that the budget gives the pool, as 640 MB
/// of every gigabyte.
const SHARE: (u64, u64) = (5, 8);

/// A pool holds page 0 of a file and one more page at the same time, which is
/// what allocating a page needs.
const LEAST_FRAMES: usize = 2;

/// Which form of a page a read wants.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    /// The newest form, including a change the caller has not committed. What
    /// a writer reads, because it has to see its own work.
    Latest,
    /// The form the log held below an offset, which is what a snapshot reads.
    At(u64),
}

/// What the pool knows about one frame, under the pool lock.
#[derive(Clone, Copy, Default)]
struct Meta {
    /// The page the frame holds, or `None` for a frame that holds none.
    page: Option<PageId>,
    /// The reference bit that the clock clears before it takes a frame.
    used: bool,
}

/// One table file, with the table it holds and the log its writes go to.
#[derive(Clone)]
struct Open {
    file: Arc<File>,
    /// The table id, which is also the name of the file. A frame names a
    /// table this way, because a `FileId` means nothing to the next process.
    table: u32,
    wal: Arc<Wal>,
}

/// The files the pool has open.
#[derive(Default)]
struct Files {
    open: Vec<Open>,
    ids: HashMap<PathBuf, FileId>,
}

impl Files {
    fn at(&self, id: FileId) -> Result<Open, DbError> {
        self.open
            .get(id.0 as usize)
            .cloned()
            .ok_or_else(|| storage_error(format!("the pool holds no file {}", id.0)))
    }

    /// The file of one table of one database. A table id repeats across
    /// databases, so the log is what tells them apart.
    fn of_table(&self, wal: &Arc<Wal>, table: u32) -> Result<Open, DbError> {
        self.open
            .iter()
            .find(|open| open.table == table && Arc::ptr_eq(&open.wal, wal))
            .cloned()
            .ok_or_else(|| storage_error(format!("the pool holds no table {table}")))
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
    /// How many callers hold each frame. A pin is only ever taken under the
    /// pool lock, so a frame that reads as unpinned there stays unpinned.
    pins: Vec<AtomicU32>,
    /// Which frames hold a change the log has not got. Outside the pool lock,
    /// so a page can be marked and asked about by whoever holds its bytes.
    dirty: Vec<AtomicBool>,
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
            pins: (0..frames).map(|_| AtomicU32::new(0)).collect(),
            dirty: (0..frames).map(|_| AtomicBool::new(false)).collect(),
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

    /// Opens a table file, or returns the id it already has.
    ///
    /// A file with no bytes gets its header page, written straight to the
    /// file rather than the log. A crash after that leaves an empty header,
    /// which is what a new table starts with anyway.
    pub fn open(&self, path: &Path, table: u32, wal: Arc<Wal>) -> Result<FileId, DbError> {
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
        inner.files.open.push(Open {
            file: Arc::new(file),
            table,
            wal,
        });
        inner.files.ids.insert(path.to_path_buf(), id);
        Ok(id)
    }

    /// Pins a page and hands back its bytes. The pin falls when the guard
    /// drops.
    pub fn fetch(&self, id: PageId) -> Result<PageGuard<'_>, DbError> {
        let frame = self.pin(id)?;
        Ok(self.guard(id, frame))
    }

    /// Reads a page as of a mark.
    ///
    /// The pool holds the newest form of every page, so a frame answers a
    /// snapshot only when nothing uncommitted sits in it and the log says the
    /// page has not changed since the mark. Otherwise the reader gets a copy
    /// of its own, and the pool keeps the newest form for the writer.
    pub fn fetch_at(&self, id: PageId, mark: Mark) -> Result<PageRef<'_>, DbError> {
        let Mark::At(upto) = mark else {
            return self.fetch(id).map(PageRef::Pinned);
        };
        let open = {
            let inner = self.inner.lock().expect("the pool lock holds");
            inner.files.at(id.file)?
        };
        if self.may_answer(&open, id, upto) {
            // Asked again with the bytes in hand, because the answer above
            // came before the pin and a writer changes a page in place. A
            // writer marks a page before it writes it, and it cannot reach
            // the bytes while they are held here.
            let page = self.fetch(id)?;
            if self.holds_form(&open, page.frame, id.page_no, upto) {
                return Ok(PageRef::Pinned(page));
            }
        }
        Ok(PageRef::Own(read_as_of(&open, id.page_no, upto)?))
    }

    /// Is the pool's frame worth pinning for a read at this mark? Asked
    /// before the pin, so the answer can go stale.
    fn may_answer(&self, open: &Open, id: PageId, upto: u64) -> bool {
        let frame = {
            let inner = self.inner.lock().expect("the pool lock holds");
            inner.map.get(&id).copied()
        };
        match frame {
            Some(frame) => self.holds_form(open, frame, id.page_no, upto),
            None => !open.wal.changed_since(open.table, id.page_no, upto),
        }
    }

    /// Does a frame hold the form of a page that a mark asks for?
    fn holds_form(&self, open: &Open, frame: usize, page_no: u32, upto: u64) -> bool {
        !self.dirty[frame].load(Ordering::Acquire)
            && !open.wal.changed_since(open.table, page_no, upto)
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

    /// Appends every changed page of one log's files, then commits the log.
    ///
    /// No page may be held, because a held page is one the caller may still
    /// be writing to.
    pub fn commit(&self, wal: &Arc<Wal>) -> Result<(), DbError> {
        {
            let inner = self.inner.lock().expect("the pool lock holds");
            for frame in 0..self.frames.len() {
                if !self.dirty[frame].load(Ordering::Acquire) {
                    continue;
                }
                let Some(page) = inner.meta[frame].page else {
                    continue;
                };
                let open = inner.files.at(page.file)?;
                if !Arc::ptr_eq(&open.wal, wal) {
                    continue;
                }
                // Waits for a reader that is part way through the page. The
                // wait is short, and the log is never held while it happens.
                let bytes = self.frames[frame].lock().expect("the frame lock holds");
                open.wal.append_page(open.table, page.page_no, &bytes)?;
                self.dirty[frame].store(false, Ordering::Release);
            }
        }
        wal.commit()
    }

    /// Drops every page of one log's files.
    ///
    /// What a rollback leaves behind: the pool holds forms of pages that no
    /// one committed, and the log no longer holds the frames to correct them
    /// with.
    /// A frame someone is reading is given up as well: it leaves the page
    /// table at once, so no later read finds it, and the clock takes the
    /// frame itself once the reader lets go. Only a reader can hold a page
    /// here, because a writer holds none between its statements.
    pub fn discard(&self, wal: &Arc<Wal>) -> Result<(), DbError> {
        let mut inner = self.inner.lock().expect("the pool lock holds");
        for frame in 0..self.frames.len() {
            let Some(page) = inner.meta[frame].page else {
                continue;
            };
            if !Arc::ptr_eq(&inner.files.at(page.file)?.wal, wal) {
                continue;
            }
            inner.map.remove(&page);
            inner.meta[frame] = Meta::default();
            self.dirty[frame].store(false, Ordering::Release);
        }
        Ok(())
    }

    /// Puts one page in its table file, where the log is not. Only a
    /// checkpoint does this, and it syncs the file afterwards.
    pub fn write_to_table(
        &self,
        wal: &Arc<Wal>,
        table: u32,
        page_no: u32,
        page: &Page,
    ) -> Result<(), DbError> {
        let open = {
            let inner = self.inner.lock().expect("the pool lock holds");
            inner.files.of_table(wal, table)?
        };
        write_page(&open.file, page_no, page)
    }

    /// Settles a table file, once a checkpoint has put every page in it.
    pub fn sync_table(&self, wal: &Arc<Wal>, table: u32) -> Result<(), DbError> {
        let open = {
            let inner = self.inner.lock().expect("the pool lock holds");
            inner.files.of_table(wal, table)?
        };
        open.file
            .sync_all()
            .map_err(|e| storage_error(format!("a table file did not sync: {e}")))
    }

    /// Finds the frame of a page, reading it in when the pool holds it not.
    ///
    /// The read happens under the lock, so no caller can reach a frame before
    /// its bytes arrive. That serialises the reads of the whole pool, which is
    /// the ceiling: milestone 10 wants readers to run beside one another, and
    /// will need a per-frame state that says "being read".
    fn pin(&self, id: PageId) -> Result<usize, DbError> {
        let mut inner = self.inner.lock().expect("the pool lock holds");
        self.pin_in(&mut inner, id)
    }

    fn pin_in(&self, inner: &mut Inner, id: PageId) -> Result<usize, DbError> {
        if let Some(&frame) = inner.map.get(&id) {
            self.pins[frame].fetch_add(1, Ordering::AcqRel);
            inner.meta[frame].used = true;
            return Ok(frame);
        }
        let frame = self.claim(inner)?;
        let open = inner.files.at(id.file)?;
        {
            let mut bytes = self.frames[frame].lock().expect("the frame lock holds");
            // The log holds the newest form of a page until a checkpoint
            // moves it, so it answers before the table file does.
            match open.wal.read_page(open.table, id.page_no, open.wal.end())? {
                Some(page) => bytes[..].copy_from_slice(page.as_slice()),
                None => read_page(&open.file, id.page_no, &mut bytes)?,
            }
        }
        inner.map.insert(id, frame);
        inner.meta[frame] = Meta {
            page: Some(id),
            used: true,
        };
        self.pins[frame].store(1, Ordering::Release);
        self.dirty[frame].store(false, Ordering::Release);
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
            // A pin is only taken under this lock, so a frame that reads as
            // unpinned here cannot be pinned before it is taken.
            if self.pins[frame].load(Ordering::Acquire) > 0 {
                continue;
            }
            if meta.used {
                inner.meta[frame].used = false;
                continue;
            }
            if let Some(page) = meta.page {
                if self.dirty[frame].load(Ordering::Acquire) {
                    // To the log and not the table file. The frame is not
                    // committed, and recovery drops it unless one follows.
                    let open = inner.files.at(page.file)?;
                    let bytes = self.frames[frame].lock().expect("the frame lock holds");
                    open.wal.append_page(open.table, page.page_no, &bytes)?;
                }
                inner.map.remove(&page);
            }
            inner.meta[frame] = Meta::default();
            self.dirty[frame].store(false, Ordering::Release);
            return Ok(frame);
        }
        Err(storage_error(format!(
            "every one of the {frames} pool frames is held"
        )))
    }

    /// Adds a page to the end of a file and returns its number.
    ///
    /// The length of the file is not logged. A crash after this leaves a page
    /// of zeros at the end, which the allocator hands out again and the
    /// startup sweep clears.
    fn grow(&self, file: FileId) -> Result<u32, DbError> {
        let inner = self.inner.lock().expect("the pool lock holds");
        let handle = inner.files.at(file)?.file;
        let len = handle
            .metadata()
            .map_err(|e| storage_error(format!("the file did not read: {e}")))?
            .len();
        handle
            .set_len(len + PAGE_SIZE as u64)
            .map_err(|e| storage_error(format!("the file did not grow: {e}")))?;
        Ok((len / PAGE_SIZE as u64) as u32)
    }

    fn guard(&self, id: PageId, frame: usize) -> PageGuard<'_> {
        PageGuard {
            pool: self,
            id,
            frame,
            bytes: self.frames[frame].lock().expect("the frame lock holds"),
            dirty: false,
        }
    }
}

/// A page pinned in the pool. The pin falls when this drops, and the pool
/// writes the page later if the caller changed it.
pub struct PageGuard<'a> {
    pool: &'a BufferPool,
    id: PageId,
    /// The frame itself, so letting go and marking a change need no lookup
    /// and no pool lock. A discard can unmap the page while it is held.
    frame: usize,
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
            // Before the bytes change, so a commit or a snapshot that sees
            // the mark never reads a half-written page.
            self.pool.dirty[self.frame].store(true, Ordering::Release);
        }
        &mut self.bytes
    }
}

impl Drop for PageGuard<'_> {
    fn drop(&mut self) {
        self.pool.pins[self.frame].fetch_sub(1, Ordering::AcqRel);
    }
}

/// A page to read: one of the pool's own frames, or a copy for a reader the
/// pool cannot answer from a frame.
pub enum PageRef<'a> {
    Pinned(PageGuard<'a>),
    Own(Box<Page>),
}

impl PageRef<'_> {
    pub fn bytes(&self) -> &Page {
        match self {
            PageRef::Pinned(page) => page.bytes(),
            PageRef::Own(page) => page,
        }
    }
}

/// A page as of a mark, read past the pool. The log answers first, because it
/// holds the newest form until a checkpoint moves it to the file.
fn read_as_of(open: &Open, page_no: u32, upto: u64) -> Result<Box<Page>, DbError> {
    match open.wal.read_page(open.table, page_no, upto)? {
        Some(page) => Ok(page),
        None => {
            let mut page = Box::new([0; PAGE_SIZE]);
            read_page(&open.file, page_no, &mut page)?;
            Ok(page)
        }
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
    use std::sync::Arc;

    use crate::catalog::testing::{self, Dir};
    use crate::store::page::{HEADER_SIZE, SlottedPage, page_u32, write_u32};
    use crate::wal::writer::{FRAME_HEADER, Wal};

    /// A pool of `frames` frames, a log, and one open table file.
    fn pool(label: &str, frames: usize) -> (Dir, BufferPool, Arc<Wal>, FileId) {
        testing::table(label, frames)
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

    /// Writes a number of its own into a page, so a read can tell which
    /// form of it came back.
    fn write(pool: &BufferPool, id: PageId, value: u32) {
        let mut page = pool.fetch(id).expect("the page fetches");
        write_u32(page.bytes_mut(), HEADER_SIZE, value);
    }

    fn read_at(pool: &BufferPool, id: PageId, mark: Mark) -> u32 {
        let page = pool.fetch_at(id, mark).expect("the page reads");
        page_u32(page.bytes(), HEADER_SIZE)
    }

    #[test]
    fn a_read_at_a_mark_misses_a_change_committed_after_it() {
        let (_dir, pool, wal, file) = pool("mark-after", 4);
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        write(&pool, id, 1);
        pool.commit(&wal).unwrap();
        let mark = wal.committed();

        write(&pool, id, 2);
        pool.commit(&wal).unwrap();

        assert_eq!(read_at(&pool, id, Mark::At(mark)), 1, "the older form");
        assert_eq!(read_at(&pool, id, Mark::Latest), 2);
        assert_eq!(
            read_at(&pool, id, Mark::At(wal.committed())),
            2,
            "a mark taken now"
        );
    }

    #[test]
    fn a_read_at_a_mark_misses_a_change_no_one_committed() {
        let (_dir, pool, wal, file) = pool("mark-dirty", 4);
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        write(&pool, id, 1);
        pool.commit(&wal).unwrap();
        let mark = wal.committed();

        // Changed in the pool and left there, which is a writer partway
        // through its transaction.
        write(&pool, id, 2);

        assert_eq!(read_at(&pool, id, Mark::At(mark)), 1);
        assert_eq!(read_at(&pool, id, Mark::Latest), 2, "the writer's own form");
    }

    #[test]
    fn a_read_at_a_mark_takes_the_pooled_page_when_nothing_changed() {
        let (_dir, pool, wal, file) = pool("mark-pooled", 4);
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        write(&pool, id, 7);
        pool.commit(&wal).unwrap();

        let held = pool.held();
        assert_eq!(read_at(&pool, id, Mark::At(wal.committed())), 7);
        assert_eq!(pool.held(), held, "no frame was spent on a copy");
    }

    #[test]
    fn a_read_at_a_mark_of_a_page_the_pool_lost_comes_off_the_log() {
        // Two frames: the header page and one more, so a read of a third
        // page pushes the one before it out.
        let (_dir, pool, wal, file) = pool("mark-evicted", 2);
        let first = pool.allocate(file).unwrap();
        stamp(&pool, first);
        write(&pool, first, 5);
        pool.commit(&wal).unwrap();
        let mark = wal.committed();
        write(&pool, first, 6);
        pool.commit(&wal).unwrap();

        let second = pool.allocate(file).unwrap();
        stamp(&pool, second);
        pool.commit(&wal).unwrap();

        assert_eq!(read_at(&pool, first, Mark::At(mark)), 5);
    }

    #[test]
    fn a_read_at_a_mark_before_a_page_existed_comes_off_the_file() {
        let (_dir, pool, wal, file) = pool("mark-before", 4);
        // Nothing has been committed, so the mark is the start of the log.
        let mark = wal.committed();
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        write(&pool, id, 8);
        pool.commit(&wal).unwrap();

        // The log holds no frame of that page below the mark, so the file
        // answers, and the file holds the page as the grow left it.
        assert_eq!(read_at(&pool, id, Mark::At(mark)), 0);
        assert_eq!(read_at(&pool, id, Mark::At(wal.committed())), 8);
    }

    #[test]
    fn a_discard_drops_the_pages_of_one_log_only() {
        let dir = Dir::new("discard");
        let pool = BufferPool::with_frames(8);
        let one = Arc::new(Wal::open(&dir.0.join("one"), 64 * 1024 * 1024).unwrap());
        let two = Arc::new(Wal::open(&dir.0.join("two"), 64 * 1024 * 1024).unwrap());
        let first = pool
            .open(&dir.0.join("one").join("1.tbl"), 1, Arc::clone(&one))
            .unwrap();
        let second = pool
            .open(&dir.0.join("two").join("1.tbl"), 1, Arc::clone(&two))
            .unwrap();
        let kept = pool.allocate(second).unwrap();
        stamp(&pool, kept);
        write(&pool, kept, 9);
        let gone = pool.allocate(first).unwrap();
        stamp(&pool, gone);
        write(&pool, gone, 9);

        pool.discard(&one).unwrap();

        assert_eq!(stamped(&pool, kept), 9, "the other log kept its pages");
        assert_eq!(
            stamped(&pool, gone),
            0,
            "the change never committed, so the page reads as the file holds it"
        );
    }

    #[test]
    fn a_discard_takes_a_held_page_out_of_reach_and_frees_its_frame() {
        let (_dir, pool, wal, file) = pool("discard-held", 4);
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        write(&pool, id, 7);
        pool.commit(&wal).unwrap();
        let held = pool.fetch(id).unwrap();
        assert_eq!(page_u32(held.bytes(), HEADER_SIZE), 7);

        pool.discard(&wal).unwrap();

        // The reader carries on with the page it holds, and the pool has
        // forgotten it, so the frame comes back once the reader lets go.
        assert_eq!(page_u32(held.bytes(), HEADER_SIZE), 7);
        assert_eq!(pool.held(), 0);
        drop(held);
        assert_eq!(stamped(&pool, id), 7, "and it reads again off the log");
    }

    #[test]
    fn a_new_file_opens_with_a_header_page() {
        let (_dir, pool, _wal, file) = pool("new-file", 4);
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
        let (dir, pool, wal, file) = pool("same-path", 4);
        let again = pool
            .open(&dir.0.join("1.tbl"), 1, Arc::clone(&wal))
            .unwrap();
        assert_eq!(file, again);
        let other = pool
            .open(&dir.0.join("2.tbl"), 2, Arc::clone(&wal))
            .unwrap();
        assert_ne!(file, other);
    }

    #[test]
    fn a_path_that_will_not_open_is_an_error() {
        let (dir, pool, wal, _) = pool("bad-path", 4);
        let e = pool
            .open(&dir.0.join("no").join("where.tbl"), 2, wal)
            .unwrap_err();
        assert!(e.message.contains("did not open"), "{e}");
    }

    #[test]
    fn a_file_the_pool_never_opened_is_an_error() {
        let (_dir, pool, _wal, _) = pool("no-file", 4);
        let e = pool.fetch(PageId::new(FileId(99), 0)).err().unwrap();
        assert!(e.message.contains("holds no file 99"), "{e}");
    }

    #[test]
    fn a_page_read_after_its_frame_was_reused_holds_what_was_written() {
        let (_dir, pool, _wal, file) = pool("round-trip", LEAST_FRAMES);
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
        let (_dir, pool, _wal, file) = pool("pinned", LEAST_FRAMES);
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
        let (_dir, pool, _wal, file) = pool("full", LEAST_FRAMES);
        let one = pool.allocate(file).unwrap();
        let two = pool.allocate(file).unwrap();
        let _first = pool.fetch(one).unwrap();
        let _second = pool.fetch(two).unwrap();
        let e = pool.fetch(PageId::new(file, 0)).err().unwrap();
        assert!(e.message.contains("frames is held"), "{e}");
    }

    #[test]
    fn a_committed_page_comes_back_through_the_log() {
        let dir = Dir::new("durable");
        let path = dir.0.join("1.tbl");
        let id = {
            let pool = BufferPool::with_frames(4);
            let wal = Arc::new(Wal::open(&dir.0, 64 * 1024 * 1024).unwrap());
            let file = pool.open(&path, 1, Arc::clone(&wal)).unwrap();
            let id = pool.allocate(file).unwrap();
            stamp(&pool, id);
            pool.commit(&wal).unwrap();
            id
        };
        // A pool and a log of their own, so nothing comes from memory. The
        // page is in the log and not yet in the table file.
        let pool = BufferPool::with_frames(4);
        let wal = Arc::new(Wal::open(&dir.0, 64 * 1024 * 1024).unwrap());
        let file = pool.open(&path, 1, wal).unwrap();
        assert_eq!(stamped(&pool, PageId::new(file, id.page_no)), id.page_no);
    }

    #[test]
    fn a_commit_leaves_nothing_changed_behind_it() {
        let (_dir, pool, wal, file) = pool("clean", 4);
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        pool.commit(&wal).unwrap();
        let after = wal.end();

        // Reading changes nothing, so a second commit appends only its own
        // frame and no page.
        assert_eq!(stamped(&pool, id), id.page_no);
        pool.commit(&wal).unwrap();
        assert_eq!(wal.end(), after + FRAME_HEADER as u64);
    }

    #[test]
    fn a_commit_leaves_the_pages_of_another_log_alone() {
        let dir = Dir::new("two-logs");
        let pool = BufferPool::with_frames(8);
        let one = Arc::new(Wal::open(&dir.0.join("one"), 64 * 1024 * 1024).unwrap());
        let two = Arc::new(Wal::open(&dir.0.join("two"), 64 * 1024 * 1024).unwrap());
        let first = pool
            .open(&dir.0.join("one").join("1.tbl"), 1, Arc::clone(&one))
            .unwrap();
        let second = pool
            .open(&dir.0.join("two").join("1.tbl"), 1, Arc::clone(&two))
            .unwrap();
        stamp(&pool, pool.allocate(first).unwrap());
        stamp(&pool, pool.allocate(second).unwrap());

        pool.commit(&one).unwrap();
        assert!(one.end() > 0, "its own pages went to its own log");
        assert_eq!(two.end(), 0, "the other log was left alone");

        pool.commit(&two).unwrap();
        assert!(two.end() > 0);
    }

    #[test]
    fn a_commit_waits_for_a_page_that_is_held_and_then_takes_it() {
        let (_dir, pool, wal, file) = pool("held-commit", 4);
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        write(&pool, id, 5);

        let held = pool.fetch(id).unwrap();
        let (sent, committed) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                pool.commit(&wal).expect("the commit goes through");
                sent.send(()).expect("the test is listening");
            });
            assert!(
                committed
                    .recv_timeout(std::time::Duration::from_millis(50))
                    .is_err(),
                "a reader held the page, so the commit could not have it yet"
            );
            drop(held);
        });

        assert!(wal.committed() > 0, "the page reached the log in the end");
        assert_eq!(read_at(&pool, id, Mark::At(wal.committed())), 5);
    }

    #[test]
    fn a_page_evicted_before_a_commit_is_still_in_the_log() {
        // Two frames against many pages, so every page is evicted into the
        // log before anything commits.
        let (_dir, pool, wal, file) = pool("evicted", LEAST_FRAMES);
        let pages: Vec<PageId> = (0..8).map(|_| pool.allocate(file).unwrap()).collect();
        for id in &pages {
            stamp(&pool, *id);
        }
        pool.commit(&wal).unwrap();
        for id in &pages {
            assert_eq!(stamped(&pool, *id), id.page_no);
        }
    }

    #[test]
    fn a_read_at_a_mark_before_a_write_does_not_see_it() {
        let (_dir, pool, wal, file) = pool("mark", 4);
        let id = pool.allocate(file).unwrap();
        stamp(&pool, id);
        pool.commit(&wal).unwrap();
        let before = wal.end();

        // The same page again, with a different stamp.
        {
            let mut page = pool.fetch(id).unwrap();
            write_u32(page.bytes_mut(), HEADER_SIZE, 999);
        }
        pool.commit(&wal).unwrap();

        assert_eq!(
            page_u32(
                &wal.read_page(1, id.page_no, before).unwrap().unwrap(),
                HEADER_SIZE
            ),
            id.page_no,
            "the mark hides the later frame"
        );
        assert_eq!(
            page_u32(
                &wal.read_page(1, id.page_no, wal.end()).unwrap().unwrap(),
                HEADER_SIZE
            ),
            999
        );
    }

    #[test]
    fn a_freed_page_is_handed_out_again() {
        let (_dir, pool, _wal, file) = pool("free-list", 4);
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
        let (_dir, pool, _wal, file) = pool("free-many", 4);
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
        let (_dir, pool, _wal, file) = pool("free-header", 4);
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
        let (_dir, pool, _wal, file) = pool("load", frames);
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
        let (_dir, pool, _wal, file) = pool("smallest", 1);
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
        let (_dir, pool, _wal, file) = pool("past-end", 4);
        let e = pool.fetch(PageId::new(file, 999)).err().unwrap();
        assert!(e.message.contains("did not read"), "{e}");
    }
}
