//! The log itself: frames in, frames out, and the sync that makes a commit
//! mean something.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

use protocol::DbError;

use crate::store::page::{PAGE_SIZE, Page};
use crate::store::storage_error;
use crate::wal::index::FrameIndex;
use crate::wal::recovery;

/// An LSN, a kind, a table, a page number, and a checksum.
pub const FRAME_HEADER: usize = 8 + 1 + 4 + 4 + 4;

/// A frame that carries a page.
pub const PAGE_FRAME: usize = FRAME_HEADER + PAGE_SIZE;

const KIND_PAGE: u8 = 1;
const KIND_COMMIT: u8 = 2;

/// What a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// A page as it stood when the frame was written.
    Page,
    /// Everything before this frame is committed.
    Commit,
}

/// One frame of the log, without its page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub lsn: u64,
    pub kind: FrameKind,
    pub table: u32,
    pub page_no: u32,
    /// Where the frame starts.
    pub at: u64,
    /// How many bytes it takes, so the next one is at `at + len`.
    pub len: u64,
}

/// What reading at an offset found.
#[derive(Debug)]
pub enum Read {
    Frame(Frame, Option<Box<Page>>),
    /// The bytes here are not a whole frame, or their checksum disagrees. A
    /// crash in the middle of a write leaves exactly this.
    Broken,
    /// The log ends here.
    End,
}

/// Writes one frame and returns how long it was.
pub fn write_frame(
    file: &File,
    at: u64,
    kind: FrameKind,
    lsn: u64,
    table: u32,
    page_no: u32,
    page: Option<&Page>,
) -> Result<u64, DbError> {
    let mut frame = Vec::with_capacity(match page {
        Some(_) => PAGE_FRAME,
        None => FRAME_HEADER,
    });
    frame.extend_from_slice(&lsn.to_le_bytes());
    frame.push(match kind {
        FrameKind::Page => KIND_PAGE,
        FrameKind::Commit => KIND_COMMIT,
    });
    frame.extend_from_slice(&table.to_le_bytes());
    frame.extend_from_slice(&page_no.to_le_bytes());
    // The checksum covers the header before it and the page after it, so a
    // frame half written is a frame that does not check out.
    let mut crc = crc32fast::Hasher::new();
    crc.update(&frame);
    if let Some(page) = page {
        crc.update(page.as_slice());
    }
    frame.extend_from_slice(&crc.finalize().to_le_bytes());
    if let Some(page) = page {
        frame.extend_from_slice(page.as_slice());
    }
    file.write_all_at(&frame, at)
        .map_err(|e| storage_error(format!("the log did not write: {e}")))?;
    Ok(frame.len() as u64)
}

/// Reads the frame at an offset, with the page of a page frame.
pub fn read_frame(file: &File, at: u64, end: u64) -> Result<Read, DbError> {
    if at >= end {
        return Ok(Read::End);
    }
    if end - at < FRAME_HEADER as u64 {
        return Ok(Read::Broken);
    }
    let mut header = [0; FRAME_HEADER];
    read_at(file, &mut header, at)?;
    let lsn = u64::from_le_bytes(header[0..8].try_into().expect("eight bytes"));
    let kind = match header[8] {
        KIND_PAGE => FrameKind::Page,
        KIND_COMMIT => FrameKind::Commit,
        _ => return Ok(Read::Broken),
    };
    let table = u32::from_le_bytes(header[9..13].try_into().expect("four bytes"));
    let page_no = u32::from_le_bytes(header[13..17].try_into().expect("four bytes"));
    let written = u32::from_le_bytes(header[17..21].try_into().expect("four bytes"));

    let mut crc = crc32fast::Hasher::new();
    crc.update(&header[..17]);
    let (page, len) = match kind {
        FrameKind::Commit => (None, FRAME_HEADER as u64),
        FrameKind::Page => {
            if end - at < PAGE_FRAME as u64 {
                return Ok(Read::Broken);
            }
            let mut page = Box::new([0; PAGE_SIZE]);
            read_at(file, page.as_mut_slice(), at + FRAME_HEADER as u64)?;
            crc.update(page.as_slice());
            (Some(page), PAGE_FRAME as u64)
        }
    };
    if crc.finalize() != written {
        return Ok(Read::Broken);
    }
    Ok(Read::Frame(
        Frame {
            lsn,
            kind,
            table,
            page_no,
            at,
            len,
        },
        page,
    ))
}

/// Reads exactly as many bytes as the buffer holds. The caller has already
/// checked that the log is long enough to hold them.
fn read_at(file: &File, into: &mut [u8], at: u64) -> Result<(), DbError> {
    file.read_exact_at(into, at)
        .map_err(|e| storage_error(format!("the log did not read: {e}")))
}

/// What the log knows, under one lock so two writers cannot interleave the
/// bytes of a frame.
struct State {
    end: u64,
    /// Where the log stood after the last commit frame. A reader takes this
    /// and not `end`, because an eviction appends a page frame that no one
    /// has committed and raises `end`.
    commit_end: u64,
    next_lsn: u64,
    index: FrameIndex,
    /// A sync that failed. Nothing may commit after one, because nothing
    /// after it can be known to have reached the disk.
    broken: bool,
}

/// The log of one database.
pub struct Wal {
    file: File,
    /// How large the log may grow. A write past it is refused, and reads
    /// carry on.
    limit: u64,
    state: Mutex<State>,
}

impl Wal {
    /// Opens the log of a database, and recovers it. What was committed is in
    /// the index when this returns, and the rest of the file is gone.
    pub fn open(dir: &Path, limit: u64) -> Result<Wal, DbError> {
        let wal_dir = dir.join("wal");
        fs::create_dir_all(&wal_dir)
            .map_err(|e| storage_error(format!("the log directory did not open: {e}")))?;
        let path = wal_dir.join("000.wal");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| storage_error(format!("{} did not open: {e}", path.display())))?;
        let recovered = recovery::recover(&file)?;
        if recovered.end < recovered.len {
            file.set_len(recovered.end)
                .map_err(|e| storage_error(format!("the log did not shorten: {e}")))?;
        }
        Ok(Wal {
            file,
            limit,
            state: Mutex::new(State {
                end: recovered.end,
                commit_end: recovered.end,
                next_lsn: recovered.next_lsn,
                index: recovered.index,
                broken: false,
            }),
        })
    }

    /// Adds a page to the log. It is not committed until a commit frame
    /// follows it, so a crash before that one drops it.
    pub fn append_page(&self, table: u32, page_no: u32, page: &Page) -> Result<(), DbError> {
        let mut state = self.state.lock().expect("the log lock holds");
        if state.broken {
            return Err(broken());
        }
        if state.end + PAGE_FRAME as u64 > self.limit {
            return Err(storage_error(format!(
                "the log would pass its limit of {} bytes",
                self.limit
            )));
        }
        let at = state.end;
        let len = write_frame(
            &self.file,
            at,
            FrameKind::Page,
            state.next_lsn,
            table,
            page_no,
            Some(page),
        )?;
        state.end = at + len;
        state.next_lsn += 1;
        state.index.insert(table, page_no, at);
        Ok(())
    }

    /// Settles everything appended so far. One sync, which is what makes a
    /// commit a promise.
    pub fn commit(&self) -> Result<(), DbError> {
        let mut state = self.state.lock().expect("the log lock holds");
        if state.broken {
            return Err(broken());
        }
        let at = state.end;
        let len = write_frame(
            &self.file,
            at,
            FrameKind::Commit,
            state.next_lsn,
            0,
            0,
            None,
        )?;
        if let Err(e) = self.file.sync_all() {
            state.broken = true;
            return Err(storage_error(format!("the log did not sync: {e}")));
        }
        state.end = at + len;
        state.commit_end = state.end;
        state.next_lsn += 1;
        Ok(())
    }

    /// The page as the log last held it at or before a mark, or none when the
    /// log holds it not and the table file answers instead.
    pub fn read_page(
        &self,
        table: u32,
        page_no: u32,
        upto: u64,
    ) -> Result<Option<Box<Page>>, DbError> {
        let (at, end) = {
            let state = self.state.lock().expect("the log lock holds");
            (state.index.newest(table, page_no, upto), state.end)
        };
        let Some(at) = at else {
            return Ok(None);
        };
        match read_frame(&self.file, at, end)? {
            Read::Frame(_, Some(page)) => Ok(Some(page)),
            _ => Err(storage_error(
                "the log holds no page where its index says one",
            )),
        }
    }

    /// Where the next frame goes, which is also how long the log is.
    pub fn end(&self) -> u64 {
        self.state.lock().expect("the log lock holds").end
    }

    /// The mark a reader takes: everything committed is below it, and
    /// everything above it is a change no one has committed.
    pub fn committed(&self) -> u64 {
        self.state.lock().expect("the log lock holds").commit_end
    }

    /// Has a page changed at or past a mark?
    pub fn changed_since(&self, table: u32, page_no: u32, mark: u64) -> bool {
        let state = self.state.lock().expect("the log lock holds");
        state.index.newer_than(table, page_no, mark)
    }

    /// Every page the log holds, with the frame that holds it.
    pub fn pages(&self) -> Vec<(u32, u32, u64)> {
        self.state.lock().expect("the log lock holds").index.pages()
    }

    /// The page of one frame, whatever the index says now.
    pub fn page_at(&self, at: u64) -> Result<Box<Page>, DbError> {
        let end = self.end();
        match read_frame(&self.file, at, end)? {
            Read::Frame(_, Some(page)) => Ok(page),
            _ => Err(storage_error("the log holds no page at that frame")),
        }
    }

    /// Empties the log, which a checkpoint does once every page it held is in
    /// its table file.
    pub fn truncate(&self) -> Result<(), DbError> {
        let mut state = self.state.lock().expect("the log lock holds");
        self.file
            .set_len(0)
            .map_err(|e| storage_error(format!("the log did not empty: {e}")))?;
        state.end = 0;
        state.commit_end = 0;
        state.index.clear();
        Ok(())
    }

    /// Cuts the log back to a mark, which is how a transaction that keeps
    /// nothing leaves no trace. Only frames above the last commit are ever
    /// cut, so `commit_end` stays where it is.
    pub fn truncate_to(&self, mark: u64) -> Result<(), DbError> {
        let mut state = self.state.lock().expect("the log lock holds");
        if mark >= state.end {
            return Ok(());
        }
        self.file
            .set_len(mark)
            .map_err(|e| storage_error(format!("the log did not shorten: {e}")))?;
        state.end = mark;
        state.index.drop_from(mark);
        Ok(())
    }
}

fn broken() -> DbError {
    storage_error("the log did not sync, so nothing more can be committed")
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::catalog::testing::Dir;

    const ROOM: u64 = 64 * 1024 * 1024;

    fn log(label: &str) -> (Dir, Wal) {
        let dir = Dir::new(label);
        let wal = Wal::open(&dir.0, ROOM).expect("the log opens");
        (dir, wal)
    }

    /// A page whose first bytes say which page it is.
    fn page(mark: u32) -> Box<Page> {
        let mut page = Box::new([0; PAGE_SIZE]);
        page[0..4].copy_from_slice(&mark.to_le_bytes());
        page
    }

    fn mark_of(page: &Page) -> u32 {
        u32::from_le_bytes(page[0..4].try_into().expect("four bytes"))
    }

    /// The log file of a directory, which only a test reaches into.
    fn file_of(dir: &Dir) -> PathBuf {
        dir.0.join("wal").join("000.wal")
    }

    /// What a power cut leaves: every byte past `to` is gone.
    fn cut(dir: &Dir, to: u64) {
        OpenOptions::new()
            .write(true)
            .open(file_of(dir))
            .expect("the log opens")
            .set_len(to)
            .expect("the log shortens");
    }

    /// A frame that was only part written before the machine went away.
    fn stub(dir: &Dir, at: u64, bytes: &[u8]) {
        OpenOptions::new()
            .write(true)
            .open(file_of(dir))
            .expect("the log opens")
            .write_all_at(bytes, at)
            .expect("the bytes write");
    }

    /// A log whose file is not a file. `/dev/null` takes every write and
    /// syncs none of them, which is a disk that has gone away under us, and a
    /// FIFO takes no positioned write at all.
    fn log_on(dir: &Dir, target: &Path) -> Wal {
        std::fs::create_dir_all(dir.0.join("wal")).expect("the log directory is made");
        std::os::unix::fs::symlink(target, file_of(dir)).expect("the target links");
        Wal::open(&dir.0, ROOM).expect("the log opens")
    }

    /// A pipe, which is the one thing a test can make that refuses a write at
    /// an offset.
    fn fifo(dir: &Dir) -> PathBuf {
        let path = dir.0.join("pipe");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .expect("mkfifo runs");
        assert!(made.success(), "the pipe is made");
        path
    }

    /// A byte changed in place, which is what a checksum is for.
    fn corrupt(dir: &Dir, at: u64) {
        let file = OpenOptions::new()
            .write(true)
            .open(file_of(dir))
            .expect("the log opens");
        file.write_all_at(&[0xff], at).expect("the byte writes");
    }

    #[test]
    fn a_log_that_cannot_sync_refuses_every_later_commit() {
        let dir = Dir::new("no-sync");
        let wal = log_on(&dir, Path::new("/dev/null"));
        wal.append_page(1, 1, &page(1)).unwrap();

        let e = wal.commit().err().unwrap();
        assert_eq!(e.code, protocol::ErrorCode::StorageFull);
        assert!(e.message.contains("did not sync"), "{e}");

        for e in [
            wal.append_page(1, 2, &page(2)).err().unwrap(),
            wal.commit().err().unwrap(),
        ] {
            assert_eq!(e.code, protocol::ErrorCode::StorageFull);
            assert!(e.message.contains("nothing more can be committed"), "{e}");
        }

        // Shut to writes, open to reads.
        assert!(wal.read_page(9, 9, wal.end()).unwrap().is_none());

        // The device swallowed the page it took, so the log cannot answer for
        // it. An error, never a wrong page.
        let e = wal.read_page(1, 1, wal.end()).err().unwrap();
        assert!(e.message.contains("did not read"), "{e}");
    }

    #[test]
    fn a_log_directory_that_is_a_file_is_an_error() {
        let dir = Dir::new("wal-in-the-way");
        std::fs::write(dir.0.join("wal"), b"not a directory").expect("the file writes");
        let e = Wal::open(&dir.0, ROOM).err().unwrap();
        assert_eq!(e.code, protocol::ErrorCode::StorageFull);
        assert!(e.message.contains("log directory did not open"), "{e}");
    }

    #[test]
    fn a_log_file_that_is_a_directory_is_an_error() {
        let dir = Dir::new("log-in-the-way");
        std::fs::create_dir_all(file_of(&dir)).expect("the directory is made");
        let e = Wal::open(&dir.0, ROOM).err().unwrap();
        assert_eq!(e.code, protocol::ErrorCode::StorageFull);
        assert!(e.message.contains("did not open"), "{e}");
    }

    #[test]
    fn an_append_that_cannot_write_leaves_the_log_where_it_was() {
        let dir = Dir::new("no-writes");
        let wal = log_on(&dir, &fifo(&dir));

        let e = wal.append_page(1, 1, &page(1)).err().unwrap();
        assert_eq!(e.code, protocol::ErrorCode::StorageFull);
        assert!(e.message.contains("did not write"), "{e}");
        assert_eq!(wal.end(), 0);
        assert!(wal.pages().is_empty());

        assert!(
            wal.commit()
                .err()
                .unwrap()
                .message
                .contains("did not write")
        );
        assert_eq!(wal.end(), 0);
    }

    #[test]
    fn a_frame_that_rots_after_it_was_indexed_is_an_error_and_not_a_wrong_page() {
        let (dir, wal) = log("rotted");
        wal.append_page(1, 7, &page(42)).unwrap();
        wal.commit().unwrap();
        corrupt(&dir, FRAME_HEADER as u64);

        let e = wal.read_page(1, 7, wal.end()).err().unwrap();
        assert_eq!(e.code, protocol::ErrorCode::StorageFull);
        assert!(
            e.message.contains("no page where its index says one"),
            "{e}"
        );
    }

    #[test]
    fn the_mark_a_reader_takes_sits_past_the_last_commit() {
        let (_dir, wal) = log("mark");
        assert_eq!(wal.committed(), 0);

        wal.append_page(1, 1, &page(1)).unwrap();
        assert!(wal.end() > 0);
        assert_eq!(wal.committed(), 0, "no one committed that page");

        wal.commit().unwrap();
        assert_eq!(wal.committed(), wal.end());

        wal.append_page(1, 1, &page(2)).unwrap();
        assert!(
            wal.end() > wal.committed(),
            "the log grew past what committed"
        );
    }

    #[test]
    fn a_log_cut_back_to_a_mark_forgets_what_came_after() {
        let (_dir, wal) = log("cut-back");
        wal.append_page(1, 1, &page(1)).unwrap();
        wal.commit().unwrap();
        let mark = wal.end();
        wal.append_page(1, 1, &page(2)).unwrap();
        wal.append_page(1, 2, &page(2)).unwrap();

        wal.truncate_to(mark).unwrap();

        assert_eq!(wal.end(), mark);
        assert_eq!(wal.committed(), mark, "the commit is still a commit");
        assert_eq!(mark_of(&wal.read_page(1, 1, mark).unwrap().unwrap()), 1);
        assert_eq!(
            wal.read_page(1, 2, mark).unwrap(),
            None,
            "its frame is gone"
        );
        assert!(!wal.changed_since(1, 1, mark));
    }

    #[test]
    fn a_cut_to_a_mark_at_the_end_or_past_it_leaves_the_log_alone() {
        let (_dir, wal) = log("cut-nothing");
        wal.append_page(1, 1, &page(1)).unwrap();
        wal.commit().unwrap();
        let end = wal.end();

        wal.truncate_to(end).unwrap();
        wal.truncate_to(end + 1).unwrap();

        assert_eq!(wal.end(), end);
        assert!(wal.changed_since(1, 1, 0));
    }

    #[test]
    fn a_page_comes_back_from_the_log() {
        let (_dir, wal) = log("round-trip");
        wal.append_page(1, 7, &page(42)).unwrap();
        wal.commit().unwrap();
        let read = wal.read_page(1, 7, wal.end()).unwrap().unwrap();
        assert_eq!(mark_of(&read), 42);
    }

    #[test]
    fn the_newest_frame_of_a_page_is_the_one_that_answers() {
        let (_dir, wal) = log("newest");
        wal.append_page(1, 7, &page(1)).unwrap();
        let between = wal.end();
        wal.append_page(1, 7, &page(2)).unwrap();
        wal.commit().unwrap();

        assert_eq!(
            mark_of(&wal.read_page(1, 7, wal.end()).unwrap().unwrap()),
            2
        );
        assert_eq!(
            mark_of(&wal.read_page(1, 7, between).unwrap().unwrap()),
            1,
            "a mark before the second frame hides it"
        );
    }

    #[test]
    fn a_page_the_log_holds_not_is_left_to_the_table_file() {
        let (_dir, wal) = log("absent");
        wal.append_page(1, 7, &page(1)).unwrap();
        assert_eq!(wal.read_page(1, 8, wal.end()).unwrap(), None);
        assert_eq!(wal.read_page(2, 7, wal.end()).unwrap(), None);
    }

    #[test]
    fn a_commit_frame_carries_no_page() {
        let (_dir, wal) = log("commit-size");
        let before = wal.end();
        wal.commit().unwrap();
        assert_eq!(wal.end() - before, FRAME_HEADER as u64);

        wal.append_page(1, 1, &page(1)).unwrap();
        assert_eq!(wal.end() - before, (FRAME_HEADER + PAGE_FRAME) as u64);
    }

    #[test]
    fn every_page_of_the_log_comes_back_with_its_frame() {
        let (_dir, wal) = log("pages");
        wal.append_page(1, 1, &page(1)).unwrap();
        wal.append_page(2, 5, &page(2)).unwrap();
        wal.commit().unwrap();
        let mut pages = wal.pages();
        pages.sort_unstable();
        assert_eq!(pages.len(), 2);
        assert_eq!(mark_of(&wal.page_at(pages[0].2).unwrap()), 1);
    }

    #[test]
    fn a_frame_that_is_not_a_page_holds_none() {
        let (_dir, wal) = log("not-a-page");
        let at = wal.end();
        wal.commit().unwrap();
        let e = wal.page_at(at).err().unwrap();
        assert!(e.message.contains("no page at that frame"), "{e}");
    }

    #[test]
    fn an_emptied_log_holds_nothing() {
        let (_dir, wal) = log("truncate");
        wal.append_page(1, 1, &page(1)).unwrap();
        wal.commit().unwrap();
        wal.truncate().unwrap();
        assert_eq!(wal.end(), 0);
        assert_eq!(wal.read_page(1, 1, u64::MAX).unwrap(), None);
        assert!(wal.pages().is_empty());
    }

    #[test]
    fn a_log_at_its_limit_refuses_a_write_and_answers_a_read() {
        let dir = Dir::new("limit");
        // Room for two page frames and no more.
        let wal = Wal::open(&dir.0, 2 * PAGE_FRAME as u64).unwrap();
        wal.append_page(1, 1, &page(1)).unwrap();
        wal.append_page(1, 2, &page(2)).unwrap();
        wal.commit().unwrap();

        let e = wal.append_page(1, 3, &page(3)).err().unwrap();
        assert_eq!(e.code, protocol::ErrorCode::StorageFull);
        assert!(e.message.contains("pass its limit"), "{e}");
        assert_eq!(
            mark_of(&wal.read_page(1, 1, wal.end()).unwrap().unwrap()),
            1
        );
    }

    #[test]
    fn a_committed_page_survives_the_log_being_opened_again() {
        let dir = Dir::new("reopen");
        {
            let wal = Wal::open(&dir.0, ROOM).unwrap();
            wal.append_page(1, 7, &page(42)).unwrap();
            wal.commit().unwrap();
        }
        let wal = Wal::open(&dir.0, ROOM).unwrap();
        assert_eq!(
            mark_of(&wal.read_page(1, 7, wal.end()).unwrap().unwrap()),
            42
        );
    }

    #[test]
    fn a_page_with_no_commit_after_it_never_happened() {
        let dir = Dir::new("uncommitted");
        let committed = {
            let wal = Wal::open(&dir.0, ROOM).unwrap();
            wal.append_page(1, 1, &page(1)).unwrap();
            wal.commit().unwrap();
            let committed = wal.end();
            // Appended, and then the process goes away.
            wal.append_page(1, 2, &page(2)).unwrap();
            committed
        };
        let wal = Wal::open(&dir.0, ROOM).unwrap();
        assert_eq!(wal.end(), committed, "the uncommitted tail was cut away");
        assert_eq!(
            mark_of(&wal.read_page(1, 1, wal.end()).unwrap().unwrap()),
            1
        );
        assert_eq!(wal.read_page(1, 2, wal.end()).unwrap(), None);
    }

    #[test]
    fn a_frame_cut_in_half_leaves_the_frames_before_it() {
        let dir = Dir::new("torn");
        let committed = {
            let wal = Wal::open(&dir.0, ROOM).unwrap();
            wal.append_page(1, 1, &page(1)).unwrap();
            wal.commit().unwrap();
            let committed = wal.end();
            wal.append_page(1, 2, &page(2)).unwrap();
            wal.commit().unwrap();
            committed
        };
        // A cut in the middle of the second page frame.
        cut(&dir, committed + FRAME_HEADER as u64 + 100);

        let wal = Wal::open(&dir.0, ROOM).unwrap();
        assert_eq!(wal.end(), committed);
        assert_eq!(
            mark_of(&wal.read_page(1, 1, wal.end()).unwrap().unwrap()),
            1
        );
        assert_eq!(wal.read_page(1, 2, wal.end()).unwrap(), None);
    }

    #[test]
    fn a_frame_header_cut_in_half_leaves_the_frames_before_it() {
        let dir = Dir::new("torn-header");
        let committed = {
            let wal = Wal::open(&dir.0, ROOM).unwrap();
            wal.append_page(1, 1, &page(1)).unwrap();
            wal.commit().unwrap();
            wal.end()
        };
        // Fewer bytes than a header, which is where a write was cut off.
        stub(&dir, committed, &[1, 2, 3]);

        let wal = Wal::open(&dir.0, ROOM).unwrap();
        assert_eq!(wal.end(), committed);
        assert_eq!(
            mark_of(&wal.read_page(1, 1, wal.end()).unwrap().unwrap()),
            1
        );
    }

    #[test]
    fn a_checksum_that_disagrees_stops_the_log_there() {
        let dir = Dir::new("corrupt");
        let committed = {
            let wal = Wal::open(&dir.0, ROOM).unwrap();
            wal.append_page(1, 1, &page(1)).unwrap();
            wal.commit().unwrap();
            let committed = wal.end();
            wal.append_page(1, 2, &page(2)).unwrap();
            wal.commit().unwrap();
            committed
        };
        // One byte of the second page, which its checksum covers.
        corrupt(&dir, committed + FRAME_HEADER as u64 + 10);

        let wal = Wal::open(&dir.0, ROOM).unwrap();
        assert_eq!(wal.end(), committed, "nothing after the bad frame is kept");
        assert_eq!(
            mark_of(&wal.read_page(1, 1, wal.end()).unwrap().unwrap()),
            1
        );
    }

    #[test]
    fn a_frame_of_no_known_kind_stops_the_log_there() {
        let dir = Dir::new("kind");
        {
            let wal = Wal::open(&dir.0, ROOM).unwrap();
            wal.append_page(1, 1, &page(1)).unwrap();
            wal.commit().unwrap();
        }
        // The kind byte of the first frame.
        corrupt(&dir, 8);
        let wal = Wal::open(&dir.0, ROOM).unwrap();
        assert_eq!(wal.end(), 0);
    }

    #[test]
    fn a_log_that_was_never_written_recovers_to_nothing() {
        let (_dir, wal) = log("empty");
        assert_eq!(wal.end(), 0);
        assert!(wal.pages().is_empty());
        assert_eq!(wal.read_page(1, 1, u64::MAX).unwrap(), None);
    }

    #[test]
    fn recovering_twice_changes_nothing() {
        let dir = Dir::new("twice");
        {
            let wal = Wal::open(&dir.0, ROOM).unwrap();
            wal.append_page(1, 1, &page(1)).unwrap();
            wal.commit().unwrap();
        }
        let once = Wal::open(&dir.0, ROOM).unwrap().end();
        let twice = Wal::open(&dir.0, ROOM).unwrap();
        assert_eq!(twice.end(), once);
        assert_eq!(
            mark_of(&twice.read_page(1, 1, twice.end()).unwrap().unwrap()),
            1
        );
    }

    #[test]
    fn a_cut_during_recovery_still_recovers() {
        let dir = Dir::new("cut-twice");
        let first = {
            let wal = Wal::open(&dir.0, ROOM).unwrap();
            wal.append_page(1, 1, &page(1)).unwrap();
            wal.commit().unwrap();
            let first = wal.end();
            wal.append_page(1, 2, &page(2)).unwrap();
            wal.commit().unwrap();
            first
        };
        // Recovery ran, and the machine went away again before it finished.
        let _ = Wal::open(&dir.0, ROOM).unwrap();
        cut(&dir, first);

        let wal = Wal::open(&dir.0, ROOM).unwrap();
        assert_eq!(wal.end(), first);
        assert_eq!(
            mark_of(&wal.read_page(1, 1, wal.end()).unwrap().unwrap()),
            1
        );
    }

    #[test]
    fn the_log_of_one_database_says_nothing_about_another() {
        let one = Dir::new("db-one");
        let two = Dir::new("db-two");
        let first = Wal::open(&one.0, ROOM).unwrap();
        let second = Wal::open(&two.0, ROOM).unwrap();
        first.append_page(1, 1, &page(1)).unwrap();
        first.commit().unwrap();
        assert_eq!(second.read_page(1, 1, u64::MAX).unwrap(), None);
        assert_eq!(second.end(), 0);
    }
}
