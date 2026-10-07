//! The page: its header, its slots, and the header page of a file.
//!
//! Slots grow from the front and records from the back, so the free space of
//! a page is the gap between them. A slot keeps its index for the life of the
//! record, because a removed slot leaves a hole rather than shifting the rest.

use protocol::DbError;

use crate::store::storage_error;

/// The size of a page, fixed at build time. A full scan reads half as many
/// pages as it would at 4 KB, and the B-tree is four levels deep at either.
pub const PAGE_SIZE: usize = 8192;

/// The bytes of one page.
pub type Page = [u8; PAGE_SIZE];

/// A file that the pool has open. The id is handed out at runtime and never
/// written to disk, so no database needs a number of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// One page of one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId {
    pub file: FileId,
    pub page_no: u32,
}

impl PageId {
    pub fn new(file: FileId, page_no: u32) -> PageId {
        PageId { file, page_no }
    }
}

/// What a page holds. The byte is never zero, so a page of zeros reads as a
/// page that was never written rather than as a header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    /// Page 0, which holds the root of the tree and the free list.
    Header,
    /// Records in slots.
    Slotted,
    /// One link of a chain that holds a value too large for a record.
    Overflow,
    /// On the free list, waiting to be handed out again.
    Free,
}

impl PageKind {
    fn to_byte(self) -> u8 {
        match self {
            PageKind::Header => 1,
            PageKind::Slotted => 2,
            PageKind::Overflow => 3,
            PageKind::Free => 4,
        }
    }

    fn from_byte(byte: u8) -> Result<PageKind, DbError> {
        match byte {
            1 => Ok(PageKind::Header),
            2 => Ok(PageKind::Slotted),
            3 => Ok(PageKind::Overflow),
            4 => Ok(PageKind::Free),
            other => Err(storage_error(format!("{other} is not a page kind"))),
        }
    }
}

/// The first bytes of every page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    /// The log record that last changed this page. Milestone 9 is the first
    /// milestone with one to write.
    pub lsn: u64,
    pub kind: PageKind,
    pub slot_count: u16,
    /// Where the records begin. They fill from the end of the page.
    pub free_offset: u16,
}

/// The width of a page header: an LSN, a kind, a slot count, a free offset.
pub const HEADER_SIZE: usize = 8 + 1 + 2 + 2;

/// The width of one slot: where its record starts and how long it is.
const SLOT_SIZE: usize = 4;

impl PageHeader {
    pub fn read(page: &Page) -> Result<PageHeader, DbError> {
        Ok(PageHeader {
            lsn: read_u64(page, 0),
            kind: PageKind::from_byte(page[8])?,
            slot_count: read_u16(page, 9),
            free_offset: read_u16(page, 11),
        })
    }

    pub fn write(&self, page: &mut Page) {
        page[0..8].copy_from_slice(&self.lsn.to_le_bytes());
        page[8] = self.kind.to_byte();
        page[9..11].copy_from_slice(&self.slot_count.to_le_bytes());
        page[11..13].copy_from_slice(&self.free_offset.to_le_bytes());
    }
}

/// Page 0 of a file. It holds no record, only the two pointers that say where
/// the tree starts and which pages are spare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHeader {
    /// The root of the B-tree, or zero when the tree is empty. Milestone 7
    /// is the first milestone to read it.
    pub root: u32,
    /// The first page of the free list, or zero when the list is empty.
    pub free_head: u32,
}

impl FileHeader {
    pub fn read(page: &Page) -> Result<FileHeader, DbError> {
        let header = PageHeader::read(page)?;
        if header.kind != PageKind::Header {
            return Err(storage_error("page 0 is not a file header"));
        }
        Ok(FileHeader {
            root: read_u32(page, HEADER_SIZE),
            free_head: read_u32(page, HEADER_SIZE + 4),
        })
    }

    pub fn write(&self, page: &mut Page) {
        PageHeader {
            lsn: 0,
            kind: PageKind::Header,
            slot_count: 0,
            free_offset: PAGE_SIZE as u16,
        }
        .write(page);
        page[HEADER_SIZE..HEADER_SIZE + 4].copy_from_slice(&self.root.to_le_bytes());
        page[HEADER_SIZE + 4..HEADER_SIZE + 8].copy_from_slice(&self.free_head.to_le_bytes());
    }
}

/// A page of records, read and changed in place.
pub struct SlottedPage<'a> {
    page: &'a mut Page,
}

impl<'a> SlottedPage<'a> {
    /// Takes a page that already holds records.
    pub fn open(page: &'a mut Page) -> Result<SlottedPage<'a>, DbError> {
        let header = PageHeader::read(page)?;
        if header.kind != PageKind::Slotted {
            return Err(storage_error("the page holds no records"));
        }
        Ok(SlottedPage { page })
    }

    /// Makes an empty page of records out of whatever the page held.
    pub fn init(page: &'a mut Page) -> SlottedPage<'a> {
        PageHeader {
            lsn: 0,
            kind: PageKind::Slotted,
            slot_count: 0,
            free_offset: PAGE_SIZE as u16,
        }
        .write(page);
        SlottedPage { page }
    }

    pub fn slot_count(&self) -> u16 {
        read_u16(self.page, 9)
    }

    /// The room a record of its own can still take, the slot it needs
    /// included.
    pub fn free_space(&self) -> usize {
        usize::from(self.free_offset()) - self.directory_end()
    }

    /// One record, or `None` for a slot that holds none.
    pub fn slot(&self, index: u16) -> Option<&[u8]> {
        if index >= self.slot_count() {
            return None;
        }
        let (at, len) = self.entry(index);
        match len {
            0 => None,
            _ => Some(&self.page[at..at + len]),
        }
    }

    /// Adds a record and returns its slot. `None` means the page is full.
    pub fn insert(&mut self, record: &[u8]) -> Option<u16> {
        let needed = record.len() + SLOT_SIZE;
        if self.free_space() < needed {
            // A removed record leaves a hole, which only a compaction
            // reclaims.
            self.compact();
            if self.free_space() < needed {
                return None;
            }
        }
        let at = usize::from(self.free_offset()) - record.len();
        self.page[at..at + record.len()].copy_from_slice(record);
        let index = self.slot_count();
        self.set_entry(index, at, record.len());
        self.set_slot_count(index + 1);
        self.set_free_offset(at);
        Some(index)
    }

    /// Empties a slot. The slots that follow keep their own indexes, because
    /// a record is found by the index it was given.
    pub fn remove(&mut self, index: u16) {
        if index < self.slot_count() {
            self.set_entry(index, 0, 0);
        }
    }

    fn free_offset(&self) -> u16 {
        read_u16(self.page, 11)
    }

    fn directory_end(&self) -> usize {
        HEADER_SIZE + usize::from(self.slot_count()) * SLOT_SIZE
    }

    fn entry(&self, index: u16) -> (usize, usize) {
        let at = HEADER_SIZE + usize::from(index) * SLOT_SIZE;
        (
            usize::from(read_u16(self.page, at)),
            usize::from(read_u16(self.page, at + 2)),
        )
    }

    fn set_entry(&mut self, index: u16, at: usize, len: usize) {
        let slot = HEADER_SIZE + usize::from(index) * SLOT_SIZE;
        self.page[slot..slot + 2].copy_from_slice(&(at as u16).to_le_bytes());
        self.page[slot + 2..slot + 4].copy_from_slice(&(len as u16).to_le_bytes());
    }

    fn set_slot_count(&mut self, count: u16) {
        self.page[9..11].copy_from_slice(&count.to_le_bytes());
    }

    fn set_free_offset(&mut self, at: usize) {
        self.page[11..13].copy_from_slice(&(at as u16).to_le_bytes());
    }

    /// Moves every live record to the end of the page, so the holes that
    /// removals left become free space again. Slot indexes do not change.
    fn compact(&mut self) {
        let mut records: Vec<(u16, Vec<u8>)> = Vec::new();
        for index in 0..self.slot_count() {
            if let Some(record) = self.slot(index) {
                records.push((index, record.to_vec()));
            }
        }
        let mut at = PAGE_SIZE;
        for (index, record) in &records {
            at -= record.len();
            self.page[at..at + record.len()].copy_from_slice(record);
            self.set_entry(*index, at, record.len());
        }
        self.set_free_offset(at);
    }
}

fn read_u16(page: &Page, at: usize) -> u16 {
    u16::from_le_bytes([page[at], page[at + 1]])
}

fn read_u32(page: &Page, at: usize) -> u32 {
    u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]])
}

fn read_u64(page: &Page, at: usize) -> u64 {
    u64::from_le_bytes([
        page[at],
        page[at + 1],
        page[at + 2],
        page[at + 3],
        page[at + 4],
        page[at + 5],
        page[at + 6],
        page[at + 7],
    ])
}

/// Writes a `u32` at an offset. The chain pages and the free list both store
/// a page number this way.
pub fn write_u32(page: &mut Page, at: usize, value: u32) {
    page[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

pub fn page_u32(page: &Page, at: usize) -> u32 {
    read_u32(page, at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> Box<Page> {
        Box::new([0; PAGE_SIZE])
    }

    #[test]
    fn a_header_reads_back_every_field() {
        let mut page = empty();
        let header = PageHeader {
            lsn: u64::MAX,
            kind: PageKind::Overflow,
            slot_count: 7,
            free_offset: 4096,
        };
        header.write(&mut page);
        assert_eq!(PageHeader::read(&page).unwrap(), header);
    }

    #[test]
    fn every_kind_reads_back_from_its_byte() {
        for kind in [
            PageKind::Header,
            PageKind::Slotted,
            PageKind::Overflow,
            PageKind::Free,
        ] {
            assert_eq!(PageKind::from_byte(kind.to_byte()).unwrap(), kind);
        }
    }

    #[test]
    fn a_page_that_was_never_written_is_not_a_page() {
        let page = empty();
        let e = PageHeader::read(&page).unwrap_err();
        assert!(e.message.contains("not a page kind"), "{e}");
        assert!(PageKind::from_byte(9).is_err());
    }

    #[test]
    fn a_file_header_holds_the_root_and_the_free_list() {
        let mut page = empty();
        let header = FileHeader {
            root: 12,
            free_head: 34,
        };
        header.write(&mut page);
        assert_eq!(FileHeader::read(&page).unwrap(), header);
        assert_eq!(PageHeader::read(&page).unwrap().kind, PageKind::Header);
    }

    #[test]
    fn a_page_of_records_is_not_a_file_header() {
        let mut page = empty();
        SlottedPage::init(&mut page);
        let e = FileHeader::read(&page).unwrap_err();
        assert!(e.message.contains("not a file header"), "{e}");
    }

    #[test]
    fn a_file_header_is_not_a_page_of_records() {
        let mut page = empty();
        FileHeader {
            root: 0,
            free_head: 0,
        }
        .write(&mut page);
        let e = SlottedPage::open(&mut page).err().unwrap();
        assert!(e.message.contains("holds no records"), "{e}");
    }

    #[test]
    fn a_record_reads_back_from_its_slot() {
        let mut page = empty();
        let mut slotted = SlottedPage::init(&mut page);
        let first = slotted.insert(b"alpha").unwrap();
        let second = slotted.insert(b"beta").unwrap();
        assert_eq!(slotted.slot(first), Some(&b"alpha"[..]));
        assert_eq!(slotted.slot(second), Some(&b"beta"[..]));
        assert_eq!(slotted.slot_count(), 2);
        assert_eq!(slotted.slot(2), None, "no slot was ever handed out");
    }

    #[test]
    fn a_page_reopens_with_its_records() {
        let mut page = empty();
        SlottedPage::init(&mut page).insert(b"alpha").unwrap();
        let slotted = SlottedPage::open(&mut page).unwrap();
        assert_eq!(slotted.slot(0), Some(&b"alpha"[..]));
    }

    #[test]
    fn records_fill_a_page_until_one_is_refused() {
        let mut page = empty();
        let mut slotted = SlottedPage::init(&mut page);
        let record = [7u8; 100];
        let mut count = 0;
        while slotted.insert(&record).is_some() {
            count += 1;
        }
        assert!(
            count > 70,
            "a page of 8 KB holds about 78 of these, got {count}"
        );
        assert!(slotted.free_space() < record.len() + SLOT_SIZE);
        // Every record is still readable after the page filled.
        for index in 0..count {
            assert_eq!(slotted.slot(index), Some(&record[..]));
        }
    }

    #[test]
    fn a_record_no_page_could_hold_is_refused() {
        let mut page = empty();
        let mut slotted = SlottedPage::init(&mut page);
        assert_eq!(slotted.insert(&[0; PAGE_SIZE]), None);
        assert_eq!(slotted.insert(&[0; 1024 * 1024]), None);
        assert_eq!(slotted.slot_count(), 0);
    }

    #[test]
    fn a_removed_slot_holds_nothing_and_keeps_the_later_indexes() {
        let mut page = empty();
        let mut slotted = SlottedPage::init(&mut page);
        slotted.insert(b"alpha").unwrap();
        slotted.insert(b"beta").unwrap();
        slotted.remove(0);
        assert_eq!(slotted.slot(0), None);
        assert_eq!(slotted.slot(1), Some(&b"beta"[..]), "beta kept its slot");
        assert_eq!(slotted.slot_count(), 2);
        // A slot that was never handed out cannot be removed.
        slotted.remove(9);
        assert_eq!(slotted.slot_count(), 2);
    }

    #[test]
    fn the_room_a_removal_frees_is_handed_out_again() {
        let mut page = empty();
        let mut slotted = SlottedPage::init(&mut page);
        let record = [7u8; 200];
        let mut slots = Vec::new();
        while let Some(slot) = slotted.insert(&record) {
            slots.push(slot);
        }
        // Nothing fits, until a removal makes room for the same size again.
        assert_eq!(slotted.insert(&record), None);
        slotted.remove(slots[0]);
        slotted.remove(slots[1]);
        let added = slotted.insert(&record).expect("the hole was reclaimed");
        assert_eq!(slotted.slot(added), Some(&record[..]));
        // The records the compaction moved are still where their slots say.
        for slot in &slots[2..] {
            assert_eq!(slotted.slot(*slot), Some(&record[..]));
        }
    }

    #[test]
    fn a_page_number_reads_back_from_its_bytes() {
        let mut page = empty();
        write_u32(&mut page, HEADER_SIZE, u32::MAX);
        assert_eq!(page_u32(&page, HEADER_SIZE), u32::MAX);
    }
}
