//! The B-tree: an ordered map from primary key to row.
//!
//! The tree is the table, so a leaf holds whole rows and not pointers to them.
//! An interior page holds a separator key and a child page for each of its
//! children, the leftmost included, so no page needs a rightmost pointer of
//! its own.
//!
//! A descent carries page numbers and not pinned pages, so the pool needs the
//! same few frames whatever the depth of the tree.

use std::cmp::Ordering;
use std::ops::Bound;

use protocol::{DbError, ErrorCode, Value};

use crate::catalog::{ColumnDef, TableDef};
use crate::store::codec::{self, Cell};
use crate::store::page::{
    self, FileHeader, FileId, HEADER_SIZE, PAGE_SIZE, Page, PageHeader, PageId, PageKind,
    SLOT_SIZE, SlottedPage,
};
use crate::store::pool::{BufferPool, Mark, PageRef};
use crate::store::storage_error;

/// The largest record a page can hold: the page, less its header and one slot.
/// A row longer than this has to move values into chains before it fits.
pub const MAX_RECORD: usize = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;

/// An ordered map from primary key to row, in one table file.
///
/// Cheap to clone: a borrow of the pool, a file, and the columns of its table.
/// A cursor takes a clone, so an operator can hold one without borrowing the
/// tree it came from.
#[derive(Clone)]
pub struct BTree<'a> {
    pool: &'a BufferPool,
    file: FileId,
    columns: Vec<ColumnDef>,
    key_index: usize,
    /// Which form of a page every read of this tree takes. A tree that
    /// writes reads `Latest`, because it has to see its own work.
    mark: Mark,
}

/// The way down to one leaf.
struct Path {
    leaf: u32,
    /// Each interior page from the root down, and the slot taken in it.
    parents: Vec<(u32, u16)>,
}

impl<'a> BTree<'a> {
    pub fn open(pool: &'a BufferPool, file: FileId, table: &TableDef, mark: Mark) -> BTree<'a> {
        BTree {
            pool,
            file,
            columns: table.columns.clone(),
            key_index: table.pk_index,
            mark,
        }
    }

    /// A page of this tree, as of the mark it reads at.
    fn read(&self, page_no: u32) -> Result<PageRef<'a>, DbError> {
        self.pool.fetch_at(self.page(page_no), self.mark)
    }

    /// The row of a key, or none when the tree holds it not.
    pub fn get(&self, key: &Value) -> Result<Option<Vec<u8>>, DbError> {
        let root = self.root()?;
        if root == 0 {
            return Ok(None);
        }
        let leaf = self.descend(root, key)?.leaf;
        let page = self.read(leaf)?;
        let (slot, found) = self.search(page.bytes(), PageKind::Leaf, key)?;
        match found {
            false => Ok(None),
            true => Ok(Some(record(page.bytes(), slot)?.to_vec())),
        }
    }

    /// Adds a row. A key the tree already holds is refused, which gives
    /// [FR28].
    pub fn insert(&self, row: &[u8]) -> Result<(), DbError> {
        if row.len() > MAX_RECORD {
            return Err(storage_error(format!(
                "a row of {} bytes does not fit a page",
                row.len()
            )));
        }
        let key = self.key_of(row)?;
        let root = match self.root()? {
            0 => self.plant()?,
            root => root,
        };
        let path = self.descend(root, &key)?;
        let (slot, found) = {
            let page = self.read(path.leaf)?;
            self.search(page.bytes(), PageKind::Leaf, &key)?
        };
        if found {
            return Err(DbError {
                code: ErrorCode::DuplicateKey,
                message: "a row with this primary key is already there".to_string(),
                position: None,
            });
        }
        {
            let mut page = self.pool.fetch(self.page(path.leaf))?;
            let mut leaf = SlottedPage::open(page.bytes_mut(), PageKind::Leaf)?;
            if leaf.insert_at(slot, row) {
                return Ok(());
            }
        }
        // The leaf is full, so it splits around the new row and hands its
        // lowest key to the parent.
        let (right, right_key) = self.split_leaf(path.leaf, slot, row)?;
        self.raise(path.parents, path.leaf, right_key, right)
    }

    /// Takes a row out and hands it back, so the caller can free whatever of
    /// it lived in a chain. None means the tree never held the key.
    pub fn delete(&self, key: &Value) -> Result<Option<Vec<u8>>, DbError> {
        let root = self.root()?;
        if root == 0 {
            return Ok(None);
        }
        let path = self.descend(root, key)?;
        let (row, emptied) = {
            let mut page = self.pool.fetch(self.page(path.leaf))?;
            let (slot, found) = self.search(page.bytes(), PageKind::Leaf, key)?;
            if !found {
                return Ok(None);
            }
            let row = record(page.bytes(), slot)?.to_vec();
            let mut leaf = SlottedPage::open(page.bytes_mut(), PageKind::Leaf)?;
            leaf.remove(slot);
            (row, leaf.slot_count() == 0)
        };
        if emptied {
            self.unlink(path)?;
        }
        Ok(Some(row))
    }

    /// Every row from one bound to the other, in key order.
    pub fn cursor(&self, from: Bound<Value>, to: Bound<Value>) -> Result<Cursor<'a>, DbError> {
        let root = self.root()?;
        if root == 0 {
            return Ok(Cursor {
                tree: self.clone(),
                page_no: 0,
                slot: 0,
                upper: to,
                done: true,
            });
        }
        let (page_no, slot) = match &from {
            Bound::Unbounded => (self.leftmost(root)?, 0),
            Bound::Included(key) | Bound::Excluded(key) => {
                let leaf = self.descend(root, key)?.leaf;
                let page = self.read(leaf)?;
                let (slot, found) = self.search(page.bytes(), PageKind::Leaf, key)?;
                // An excluded bound that is there starts after it.
                let skip = found && matches!(from, Bound::Excluded(_));
                (leaf, slot + u16::from(skip))
            }
        };
        Ok(Cursor {
            tree: self.clone(),
            page_no,
            slot,
            upper: to,
            done: false,
        })
    }

    /// The column the tree is keyed by, as a list of one.
    fn key_column(&self) -> &[ColumnDef] {
        &self.columns[self.key_index..=self.key_index]
    }

    /// The key of a row. The catalog keeps the key among the columns, so the
    /// cell is always there.
    fn key_of(&self, row: &[u8]) -> Result<Value, DbError> {
        let mut cells = codec::decode_row(&self.columns, row)?;
        match cells.swap_remove(self.key_index) {
            Cell::Value(Value::Null) => Err(storage_error("a row holds no primary key")),
            Cell::Value(value) => Ok(value),
            Cell::Chain(_) => Err(storage_error("a primary key too large for a record")),
        }
    }

    fn encode_key(&self, key: &Value) -> Result<Vec<u8>, DbError> {
        codec::encode_row(self.key_column(), &[Cell::Value(key.clone())])
    }

    fn decode_key(&self, bytes: &[u8]) -> Result<Value, DbError> {
        let mut cells = codec::decode_row(self.key_column(), bytes)?;
        match cells.swap_remove(0) {
            Cell::Value(value) => Ok(value),
            Cell::Chain(_) => Err(storage_error("a primary key too large for a record")),
        }
    }

    /// One entry of an interior page: the separator key, then its child.
    fn entry(&self, key: &Value, child: u32) -> Result<Vec<u8>, DbError> {
        let mut entry = self.encode_key(key)?;
        entry.extend_from_slice(&child.to_le_bytes());
        Ok(entry)
    }

    /// The key and the child of an interior entry.
    fn split_entry(entry: &[u8]) -> Result<(&[u8], u32), DbError> {
        let at = entry
            .len()
            .checked_sub(4)
            .ok_or_else(|| storage_error("an interior entry names no child"))?;
        let child = u32::from_le_bytes([entry[at], entry[at + 1], entry[at + 2], entry[at + 3]]);
        Ok((&entry[..at], child))
    }

    /// The key of one slot, whichever kind of page holds it.
    fn slot_key(&self, bytes: &Page, kind: PageKind, index: u16) -> Result<Value, DbError> {
        let found = record(bytes, index)?;
        match kind {
            PageKind::Leaf => self.key_of(found),
            _ => self.decode_key(BTree::split_entry(found)?.0),
        }
    }

    /// The slot a key sits in, and whether it was there at all. A key that is
    /// not there gives the slot it would take.
    fn search(&self, bytes: &Page, kind: PageKind, key: &Value) -> Result<(u16, bool), DbError> {
        let mut low = 0;
        let mut high = page::slot_count(bytes);
        while low < high {
            let middle = low + (high - low) / 2;
            match compare(&self.slot_key(bytes, kind, middle)?, key)? {
                Ordering::Less => low = middle + 1,
                Ordering::Equal => return Ok((middle, true)),
                Ordering::Greater => high = middle,
            }
        }
        Ok((low, false))
    }

    /// The child of an interior page that a key belongs under: the last whose
    /// key is at or below it, and the first when the key is below them all.
    fn child_slot(&self, bytes: &Page, key: &Value) -> Result<u16, DbError> {
        let (slot, found) = self.search(bytes, PageKind::Interior, key)?;
        Ok(match found {
            true => slot,
            false => slot.saturating_sub(1),
        })
    }

    fn page(&self, page_no: u32) -> PageId {
        PageId::new(self.file, page_no)
    }

    fn root(&self) -> Result<u32, DbError> {
        let page = self.read(0)?;
        Ok(FileHeader::read(page.bytes())?.root)
    }

    fn set_root(&self, root: u32) -> Result<(), DbError> {
        let mut page = self.pool.fetch(self.page(0))?;
        let header = FileHeader {
            root,
            ..FileHeader::read(page.bytes())?
        };
        header.write(page.bytes_mut());
        Ok(())
    }

    /// The first leaf of an empty tree.
    fn plant(&self) -> Result<u32, DbError> {
        let id = self.pool.allocate(self.file)?;
        {
            let mut page = self.pool.fetch(id)?;
            SlottedPage::init(page.bytes_mut(), PageKind::Leaf);
        }
        self.set_root(id.page_no)?;
        Ok(id.page_no)
    }

    /// The way from the root down to the leaf a key belongs in.
    fn descend(&self, root: u32, key: &Value) -> Result<Path, DbError> {
        let mut page_no = root;
        let mut parents = Vec::new();
        loop {
            let step = {
                let page = self.read(page_no)?;
                match PageHeader::read(page.bytes())?.kind {
                    PageKind::Leaf => None,
                    PageKind::Interior => {
                        let slot = self.child_slot(page.bytes(), key)?;
                        let child = BTree::split_entry(record(page.bytes(), slot)?)?.1;
                        Some((slot, child))
                    }
                    other => {
                        return Err(storage_error(format!("{other:?} is no part of a tree")));
                    }
                }
            };
            match step {
                None => {
                    return Ok(Path {
                        leaf: page_no,
                        parents,
                    });
                }
                Some((slot, child)) => {
                    parents.push((page_no, slot));
                    page_no = child;
                }
            }
        }
    }

    /// The leftmost leaf under a page.
    fn leftmost(&self, page_no: u32) -> Result<u32, DbError> {
        let mut page_no = page_no;
        loop {
            let child = {
                let page = self.read(page_no)?;
                match PageHeader::read(page.bytes())?.kind {
                    PageKind::Leaf => return Ok(page_no),
                    PageKind::Interior => BTree::split_entry(record(page.bytes(), 0)?)?.1,
                    other => {
                        return Err(storage_error(format!("{other:?} is no part of a tree")));
                    }
                }
            };
            page_no = child;
        }
    }

    /// The lowest key under a page.
    fn lowest_key(&self, page_no: u32) -> Result<Value, DbError> {
        let leaf = self.leftmost(page_no)?;
        let page = self.read(leaf)?;
        self.slot_key(page.bytes(), PageKind::Leaf, 0)
    }

    /// Every record of a page, in slot order.
    fn records(&self, page_no: u32) -> Result<Vec<Vec<u8>>, DbError> {
        let page = self.read(page_no)?;
        (0..page::slot_count(page.bytes()))
            .map(|index| record(page.bytes(), index).map(<[u8]>::to_vec))
            .collect()
    }

    /// Splits a leaf around a row that would not fit, and returns the new page
    /// with the lowest key in it.
    ///
    /// The row joins the records first and the two halves are written from
    /// scratch, so a leaf of one row and a row of any size both work out.
    fn split_leaf(&self, leaf: u32, at: u16, row: &[u8]) -> Result<(u32, Value), DbError> {
        let mut rows = self.records(leaf)?;
        rows.insert(usize::from(at), row.to_vec());
        let middle = midpoint(&rows)?;
        let right = self.pool.allocate(self.file)?.page_no;

        let (old_next, old_prev) = {
            let page = self.read(leaf)?;
            let header = PageHeader::read(page.bytes())?;
            (header.next, header.prev)
        };
        self.fill(right, PageKind::Leaf, &rows[middle..])?;
        {
            let mut page = self.pool.fetch(self.page(right))?;
            let mut right_page = SlottedPage::open(page.bytes_mut(), PageKind::Leaf)?;
            right_page.set_next(old_next);
            right_page.set_prev(leaf);
        }
        self.fill(leaf, PageKind::Leaf, &rows[..middle])?;
        {
            let mut page = self.pool.fetch(self.page(leaf))?;
            let mut left = SlottedPage::open(page.bytes_mut(), PageKind::Leaf)?;
            left.set_next(right);
            left.set_prev(old_prev);
        }
        if old_next != 0 {
            let mut page = self.pool.fetch(self.page(old_next))?;
            SlottedPage::open(page.bytes_mut(), PageKind::Leaf)?.set_prev(right);
        }
        let key = self.key_of(&rows[middle])?;
        Ok((right, key))
    }

    /// Splits an interior page around an entry that would not fit.
    fn split_interior(&self, page_no: u32, at: u16, entry: &[u8]) -> Result<(u32, Value), DbError> {
        let mut entries = self.records(page_no)?;
        entries.insert(usize::from(at), entry.to_vec());
        let middle = midpoint(&entries)?;
        let right = self.pool.allocate(self.file)?.page_no;
        self.fill(right, PageKind::Interior, &entries[middle..])?;
        self.fill(page_no, PageKind::Interior, &entries[..middle])?;
        let key = self.decode_key(BTree::split_entry(&entries[middle])?.0)?;
        Ok((right, key))
    }

    /// Writes a page from scratch, holding exactly these records.
    fn fill(&self, page_no: u32, kind: PageKind, records: &[Vec<u8>]) -> Result<(), DbError> {
        let mut page = self.pool.fetch(self.page(page_no))?;
        let mut slotted = SlottedPage::init(page.bytes_mut(), kind);
        for record in records {
            if !slotted.insert(record) {
                return Err(storage_error("a page did not take the half it was given"));
            }
        }
        Ok(())
    }

    /// Hands a separator up the path, splitting each page that has no room,
    /// and growing a new root when the root itself splits.
    fn raise(
        &self,
        mut parents: Vec<(u32, u16)>,
        mut left: u32,
        mut key: Value,
        mut child: u32,
    ) -> Result<(), DbError> {
        loop {
            let Some((page_no, slot)) = parents.pop() else {
                return self.new_root(left, key, child);
            };
            let entry = self.entry(&key, child)?;
            // Just after the child the descent came through. Searching by key
            // would be wrong: a separator is the lowest key of its subtree
            // only until a smaller key is inserted under it, and a stale one
            // would put the new page on the wrong side of it.
            let slot = slot + 1;
            {
                let mut page = self.pool.fetch(self.page(page_no))?;
                let mut interior = SlottedPage::open(page.bytes_mut(), PageKind::Interior)?;
                if interior.insert_at(slot, &entry) {
                    return Ok(());
                }
            }
            let (right, right_key) = self.split_interior(page_no, slot, &entry)?;
            left = page_no;
            key = right_key;
            child = right;
        }
    }

    /// A root above two pages, which makes the tree one level deeper.
    fn new_root(&self, left: u32, key: Value, right: u32) -> Result<(), DbError> {
        let lowest = self.lowest_key(left)?;
        let root = self.pool.allocate(self.file)?.page_no;
        let entries = vec![self.entry(&lowest, left)?, self.entry(&key, right)?];
        self.fill(root, PageKind::Interior, &entries)?;
        self.set_root(root)
    }

    /// Takes an empty leaf out of the tree and gives its page back.
    fn unlink(&self, path: Path) -> Result<(), DbError> {
        let (next, prev) = {
            let page = self.read(path.leaf)?;
            let header = PageHeader::read(page.bytes())?;
            (header.next, header.prev)
        };
        if prev != 0 {
            let mut page = self.pool.fetch(self.page(prev))?;
            SlottedPage::open(page.bytes_mut(), PageKind::Leaf)?.set_next(next);
        }
        if next != 0 {
            let mut page = self.pool.fetch(self.page(next))?;
            SlottedPage::open(page.bytes_mut(), PageKind::Leaf)?.set_prev(prev);
        }
        self.prune(path.parents)?;
        self.pool.free(self.page(path.leaf))
    }

    /// Removes a child from its parent, and the parent from its own, while a
    /// page is left with no child at all.
    fn prune(&self, mut parents: Vec<(u32, u16)>) -> Result<(), DbError> {
        while let Some((page_no, slot)) = parents.pop() {
            let emptied = {
                let mut page = self.pool.fetch(self.page(page_no))?;
                let mut interior = SlottedPage::open(page.bytes_mut(), PageKind::Interior)?;
                interior.remove(slot);
                interior.slot_count() == 0
            };
            if !emptied {
                return Ok(());
            }
            self.pool.free(self.page(page_no))?;
        }
        // Every level has gone, so the tree is empty again.
        self.set_root(0)
    }
}

/// Walks the leaves in key order, from one bound to the other.
pub struct Cursor<'a> {
    tree: BTree<'a>,
    page_no: u32,
    slot: u16,
    upper: Bound<Value>,
    done: bool,
}

impl Cursor<'_> {
    /// The next row, or none at the end of the range.
    pub fn next(&mut self) -> Result<Option<Vec<u8>>, DbError> {
        while !self.done {
            let (row, next) = {
                let page = self.tree.read(self.page_no)?;
                let row = page::slot(page.bytes(), self.slot).map(<[u8]>::to_vec);
                (row, PageHeader::read(page.bytes())?.next)
            };
            match row {
                Some(row) => {
                    self.slot += 1;
                    if self.beyond(&row)? {
                        self.done = true;
                        return Ok(None);
                    }
                    return Ok(Some(row));
                }
                // The leaf ran out, so the walk goes to the one on its right.
                None => match next {
                    0 => self.done = true,
                    page_no => {
                        self.page_no = page_no;
                        self.slot = 0;
                    }
                },
            }
        }
        Ok(None)
    }

    /// Whether a row is past the upper bound.
    fn beyond(&self, row: &[u8]) -> Result<bool, DbError> {
        let key = self.tree.key_of(row)?;
        Ok(match &self.upper {
            Bound::Unbounded => false,
            Bound::Included(limit) => compare(&key, limit)? == Ordering::Greater,
            Bound::Excluded(limit) => compare(&key, limit)? != Ordering::Less,
        })
    }
}

/// One record of a page.
fn record(bytes: &Page, index: u16) -> Result<&[u8], DbError> {
    page::slot(bytes, index).ok_or_else(|| storage_error("the page holds no such slot"))
}

/// Where to cut a list of records so each half holds about half the bytes.
/// At least one record stays on each side, so neither page comes out empty.
fn midpoint(records: &[Vec<u8>]) -> Result<usize, DbError> {
    if records.len() < 2 {
        return Err(storage_error("a page of one record cannot be split"));
    }
    let total: usize = records.iter().map(Vec::len).sum();
    let mut at = records.len() - 1;
    let mut taken = 0;
    for (index, record) in records.iter().enumerate() {
        taken += record.len();
        if taken * 2 >= total {
            at = index;
            break;
        }
    }
    Ok((at + 1).clamp(1, records.len() - 1))
}

/// Two keys in order. A pair that cannot be compared is a key of the wrong
/// type for its column, which no row should have reached the tree with.
fn compare(left: &Value, right: &Value) -> Result<Ordering, DbError> {
    left.compare(right).ok_or_else(|| DbError {
        code: ErrorCode::TypeMismatch,
        message: "two keys of different types cannot be put in order".to_string(),
        position: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::DataType;

    use std::sync::Arc;

    use crate::catalog::testing::{self, Dir};
    use crate::wal::writer::Wal;
    use crate::wal::{checkpoint, writer::PAGE_FRAME};

    /// Settles the pages written so far and empties the log once it has
    /// grown, which is what a server does between statements. A run of
    /// writes that never commits grows the log without bound, and the log
    /// has a limit.
    fn settle(pool: &BufferPool, wal: &Arc<Wal>) {
        pool.commit(wal).expect("the pages commit");
        if wal.end() > 4 * 1024 * 1024 {
            checkpoint::run(pool, wal).expect("the checkpoint runs");
        }
    }

    /// A table of a key column and a label, keyed by the first.
    fn table(ty: DataType) -> TableDef {
        TableDef {
            id: 1,
            name: "item".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    ty,
                    not_null: true,
                },
                ColumnDef {
                    name: "label".to_string(),
                    ty: DataType::Text,
                    not_null: false,
                },
            ],
            pk_index: 0,
        }
    }

    /// A table keyed by an integer, with `texts` text columns after it. A
    /// single value cannot pass the inline limit, so a row only outgrows a
    /// page by holding many columns.
    fn wide_table(texts: usize) -> TableDef {
        let mut columns = vec![ColumnDef {
            name: "id".to_string(),
            ty: DataType::Integer,
            not_null: true,
        }];
        for n in 0..texts {
            columns.push(ColumnDef {
                name: format!("t{n}"),
                ty: DataType::Text,
                not_null: false,
            });
        }
        TableDef {
            id: 1,
            name: "wide".to_string(),
            columns,
            pk_index: 0,
        }
    }

    /// A row of a wide table, every text filled to the inline limit.
    fn wide_row(table: &TableDef, key: i64) -> Vec<u8> {
        let mut cells = vec![Cell::Value(Value::Integer(key))];
        for _ in 1..table.columns.len() {
            cells.push(Cell::Value(Value::Text("x".repeat(codec::INLINE_LIMIT))));
        }
        codec::encode_row(&table.columns, &cells).expect("the row encodes")
    }

    fn fixture(label: &str, frames: usize) -> (Dir, BufferPool, Arc<Wal>, FileId) {
        testing::table(label, frames)
    }

    fn row(table: &TableDef, key: Value, label: &str) -> Vec<u8> {
        codec::encode_row(
            &table.columns,
            &[
                Cell::Value(key),
                Cell::Value(Value::Text(label.to_string())),
            ],
        )
        .expect("the row encodes")
    }

    fn int_row(table: &TableDef, key: i64) -> Vec<u8> {
        row(table, Value::Integer(key), &format!("row {key}"))
    }

    /// Every key of the tree, in the order a scan gives them.
    fn keys(tree: &BTree) -> Vec<i64> {
        scan(tree, Bound::Unbounded, Bound::Unbounded)
    }

    fn scan(tree: &BTree, from: Bound<Value>, to: Bound<Value>) -> Vec<i64> {
        let mut cursor = tree.cursor(from, to).expect("the cursor opens");
        let mut found = Vec::new();
        while let Some(row) = cursor.next().expect("the scan reads") {
            match tree.key_of(&row).expect("the row holds a key") {
                Value::Integer(key) => found.push(key),
                other => panic!("expected an integer key, got {other:?}"),
            }
        }
        found
    }

    fn root_kind(tree: &BTree) -> PageKind {
        let root = tree.root().expect("the root reads");
        let page = tree.pool.fetch(tree.page(root)).expect("the root fetches");
        PageHeader::read(page.bytes())
            .expect("the root has a header")
            .kind
    }

    /// How many pages the file holds, which says whether a delete gave any
    /// back.
    fn pages(tree: &BTree) -> u32 {
        let mut count = 1;
        while tree.pool.fetch(tree.page(count)).is_ok() {
            count += 1;
        }
        count
    }

    #[test]
    fn a_scan_at_a_mark_returns_the_rows_of_that_moment() {
        let (_dir, pool, wal, file) = fixture("scan-at-mark", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        for key in 1..=3 {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        pool.commit(&wal).unwrap();
        let mark = wal.committed();

        for key in 4..=6 {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        tree.delete(&Value::Integer(2)).unwrap();
        pool.commit(&wal).unwrap();

        let then = BTree::open(&pool, file, &table, Mark::At(mark));
        assert_eq!(keys(&then), vec![1, 2, 3]);
        assert_eq!(keys(&tree), vec![1, 3, 4, 5, 6]);
        assert!(then.get(&Value::Integer(5)).unwrap().is_none());
        assert!(then.get(&Value::Integer(2)).unwrap().is_some());
    }

    #[test]
    fn an_empty_tree_holds_nothing() {
        let (_dir, pool, _wal, file) = fixture("empty", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        assert_eq!(tree.get(&Value::Integer(1)).unwrap(), None);
        assert_eq!(keys(&tree), Vec::<i64>::new());
        assert_eq!(tree.delete(&Value::Integer(1)).unwrap(), None);
    }

    #[test]
    fn a_key_reads_back_through_an_interior_entry() {
        let (_dir, pool, _wal, file) = fixture("entry", 8);
        for (ty, key) in [
            (DataType::Integer, Value::Integer(-7)),
            (DataType::Text, Value::Text("hé".to_string())),
            (DataType::Boolean, Value::Boolean(true)),
            (
                DataType::Decimal { p: 10, s: 2 },
                Value::Decimal("12.20".parse().unwrap()),
            ),
        ] {
            let table = table(ty);
            let tree = BTree::open(&pool, file, &table, Mark::Latest);
            let entry = tree.entry(&key, 42).unwrap();
            let (bytes, child) = BTree::split_entry(&entry).unwrap();
            assert_eq!(child, 42);
            assert_eq!(tree.decode_key(bytes).unwrap(), key, "{ty}");
        }
    }

    #[test]
    fn an_entry_that_names_no_child_is_an_error() {
        let e = BTree::split_entry(&[1, 2]).err().unwrap();
        assert!(e.message.contains("names no child"), "{e}");
    }

    #[test]
    fn a_search_finds_every_key_of_a_page_and_places_the_rest() {
        let (_dir, pool, _wal, file) = fixture("search", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        // Even keys only, so every odd key is one that is not there.
        let mut page = Box::new([0; PAGE_SIZE]);
        {
            let mut leaf = SlottedPage::init(&mut page, PageKind::Leaf);
            for key in (0..40).step_by(2) {
                assert!(leaf.insert(&int_row(&table, key)));
            }
        }
        for key in 0..40 {
            let (slot, found) = tree
                .search(&page, PageKind::Leaf, &Value::Integer(key))
                .unwrap();
            assert_eq!(found, key % 2 == 0, "key {key}");
            assert_eq!(usize::from(slot), (key as usize).div_ceil(2), "key {key}");
        }
        let (slot, found) = tree
            .search(&page, PageKind::Leaf, &Value::Integer(-1))
            .unwrap();
        assert_eq!((slot, found), (0, false), "a key below every other");
    }

    #[test]
    fn a_search_of_an_empty_page_gives_the_first_slot() {
        let (_dir, pool, _wal, file) = fixture("search-empty", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        let mut page = Box::new([0; PAGE_SIZE]);
        SlottedPage::init(&mut page, PageKind::Leaf);
        assert_eq!(
            tree.search(&page, PageKind::Leaf, &Value::Integer(1))
                .unwrap(),
            (0, false)
        );
    }

    #[test]
    fn keys_read_back_in_order_however_they_went_in() {
        for (label, order) in [
            ("ordered", (0..300).collect::<Vec<i64>>()),
            ("reversed", (0..300).rev().collect()),
            ("shuffled", shuffled(300)),
        ] {
            let (_dir, pool, _wal, file) = fixture(label, 8);
            let table = table(DataType::Integer);
            let tree = BTree::open(&pool, file, &table, Mark::Latest);
            for key in &order {
                tree.insert(&int_row(&table, *key)).unwrap();
            }
            assert_eq!(keys(&tree), (0..300).collect::<Vec<i64>>(), "{label}");
            for key in &order {
                assert_eq!(
                    tree.get(&Value::Integer(*key)).unwrap(),
                    Some(int_row(&table, *key)),
                    "{label} key {key}"
                );
            }
        }
    }

    #[test]
    fn enough_rows_make_a_tree_of_more_than_one_level() {
        let (_dir, pool, _wal, file) = fixture("levels", 16);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        tree.insert(&int_row(&table, 1)).unwrap();
        assert_eq!(root_kind(&tree), PageKind::Leaf, "one row needs one leaf");
        for key in 2..400 {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        assert_eq!(root_kind(&tree), PageKind::Interior);
        assert_eq!(keys(&tree), (1..400).collect::<Vec<i64>>());
    }

    #[test]
    fn a_tree_deep_enough_to_split_an_interior_page_stays_in_order() {
        // Wide rows, so few fit a leaf and the leaves alone fill the root.
        let (_dir, pool, wal, file) = fixture("deep", 32);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        let wide = "x".repeat(400);
        let count = 15_000;
        for (written, key) in shuffled(count).into_iter().enumerate() {
            tree.insert(&row(&table, Value::Integer(key), &wide))
                .unwrap();
            if written % 500 == 0 {
                settle(&pool, &wal);
            }
        }
        settle(&pool, &wal);
        assert_eq!(keys(&tree), (0..count).collect::<Vec<i64>>());
        // The root split at least once, so the tree is three levels deep.
        let root = tree.root().unwrap();
        let child = {
            let page = tree.pool.fetch(tree.page(root)).unwrap();
            BTree::split_entry(record(page.bytes(), 0).unwrap())
                .unwrap()
                .1
        };
        let page = tree.pool.fetch(tree.page(child)).unwrap();
        assert_eq!(
            PageHeader::read(page.bytes()).unwrap().kind,
            PageKind::Interior,
            "the root's children are interior pages"
        );
    }

    #[test]
    fn a_duplicate_key_is_refused_for_every_kind_of_key() {
        let cases = [
            (DataType::Integer, Value::Integer(7)),
            (DataType::Text, Value::Text("seven".to_string())),
            (DataType::Boolean, Value::Boolean(false)),
            (
                DataType::Decimal { p: 10, s: 2 },
                Value::Decimal("7.00".parse().unwrap()),
            ),
        ];
        for (ty, key) in cases {
            let (_dir, pool, _wal, file) = fixture(&format!("duplicate-{ty}"), 8);
            let table = table(ty);
            let tree = BTree::open(&pool, file, &table, Mark::Latest);
            tree.insert(&row(&table, key.clone(), "first")).unwrap();
            let e = tree
                .insert(&row(&table, key.clone(), "second"))
                .err()
                .unwrap();
            assert_eq!(e.code, ErrorCode::DuplicateKey, "{ty}");
            // The row that was there is the one that stays.
            assert_eq!(tree.get(&key).unwrap(), Some(row(&table, key, "first")));
        }
    }

    #[test]
    fn a_decimal_key_of_the_same_value_written_differently_is_a_duplicate() {
        let (_dir, pool, _wal, file) = fixture("decimal-key", 8);
        let table = table(DataType::Decimal { p: 10, s: 2 });
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        tree.insert(&row(&table, Value::Decimal("12.20".parse().unwrap()), "a"))
            .unwrap();
        let e = tree
            .insert(&row(&table, Value::Decimal("12.2".parse().unwrap()), "b"))
            .err()
            .unwrap();
        assert_eq!(
            e.code,
            ErrorCode::DuplicateKey,
            "12.20 and 12.2 are one key"
        );
    }

    #[test]
    fn a_row_that_no_page_could_hold_is_refused() {
        let (_dir, pool, _wal, file) = fixture("too-wide", 8);
        let table = wide_table(5);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        let row = wide_row(&table, 1);
        assert!(row.len() > MAX_RECORD, "the row is {} bytes", row.len());
        let e = tree.insert(&row).err().unwrap();
        assert!(e.message.contains("does not fit a page"), "{e}");
    }

    #[test]
    fn a_row_that_only_just_fits_a_page_goes_in() {
        let (_dir, pool, _wal, file) = fixture("just-fits", 8);
        let table = wide_table(3);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        assert!(
            wide_row(&table, 1).len() > MAX_RECORD / 2,
            "one row to a leaf"
        );
        // Each row takes a leaf of its own, so every insert splits.
        for key in [1, 2, 3] {
            tree.insert(&wide_row(&table, key)).unwrap();
        }
        let mut cursor = tree.cursor(Bound::Unbounded, Bound::Unbounded).unwrap();
        let mut found = Vec::new();
        while let Some(row) = cursor.next().unwrap() {
            found.push(tree.key_of(&row).unwrap());
        }
        assert_eq!(
            found,
            vec![Value::Integer(1), Value::Integer(2), Value::Integer(3)]
        );
    }

    #[test]
    fn a_page_of_one_record_cannot_be_split() {
        let (_dir, pool, _wal, file) = fixture("no-split", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        let leaf = tree.plant().unwrap();
        let e = tree.split_leaf(leaf, 0, &int_row(&table, 1)).err().unwrap();
        assert!(e.message.contains("cannot be split"), "{e}");
    }

    #[test]
    fn every_bound_takes_and_leaves_the_right_rows() {
        let (_dir, pool, _wal, file) = fixture("bounds", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        for key in 0..200 {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        let at = |n: i64| Value::Integer(n);

        assert_eq!(
            scan(&tree, Bound::Included(at(10)), Bound::Included(at(13))),
            vec![10, 11, 12, 13]
        );
        assert_eq!(
            scan(&tree, Bound::Excluded(at(10)), Bound::Excluded(at(13))),
            vec![11, 12]
        );
        assert_eq!(
            scan(&tree, Bound::Included(at(197)), Bound::Unbounded),
            vec![197, 198, 199]
        );
        assert_eq!(
            scan(&tree, Bound::Unbounded, Bound::Excluded(at(3))),
            vec![0, 1, 2]
        );
        // Bounds that name keys the tree holds not.
        assert_eq!(
            scan(&tree, Bound::Included(at(-5)), Bound::Included(at(2))),
            vec![0, 1, 2]
        );
        assert_eq!(
            scan(&tree, Bound::Included(at(500)), Bound::Unbounded),
            Vec::<i64>::new()
        );
        assert_eq!(
            scan(&tree, Bound::Included(at(5)), Bound::Included(at(4))),
            Vec::<i64>::new()
        );
    }

    #[test]
    fn a_scan_crosses_every_leaf_of_a_tree() {
        let (_dir, pool, _wal, file) = fixture("cross", 4);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        for key in 0..600 {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        // More leaves than the pool has frames, so the scan reads from disk.
        assert!(pages(&tree) > 4);
        assert_eq!(keys(&tree), (0..600).collect::<Vec<i64>>());
    }

    #[test]
    fn a_deleted_key_is_gone_from_a_read_and_from_a_scan() {
        let (_dir, pool, _wal, file) = fixture("delete-one", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        for key in 0..100 {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        assert_eq!(
            tree.delete(&Value::Integer(50)).unwrap(),
            Some(int_row(&table, 50)),
            "the row comes back so the caller can free its chains"
        );
        assert_eq!(tree.get(&Value::Integer(50)).unwrap(), None);
        assert_eq!(tree.delete(&Value::Integer(50)).unwrap(), None);
        let left: Vec<i64> = (0..100).filter(|key| *key != 50).collect();
        assert_eq!(keys(&tree), left);
    }

    #[test]
    fn deleting_every_key_empties_the_tree_and_gives_the_pages_back() {
        let (_dir, pool, _wal, file) = fixture("delete-all", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        for key in 0..400 {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        let grown = pages(&tree);
        for key in 0..400 {
            assert!(
                tree.delete(&Value::Integer(key)).unwrap().is_some(),
                "key {key}"
            );
        }
        assert_eq!(keys(&tree), Vec::<i64>::new());
        assert_eq!(tree.root().unwrap(), 0, "an emptied tree is an empty tree");

        // The pages came back, so filling it again does not grow the file.
        for key in 0..400 {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        assert!(
            pages(&tree) <= grown,
            "the file grew from {grown} to {} instead of reusing pages",
            pages(&tree)
        );
    }

    #[test]
    fn deleting_half_the_keys_leaves_the_rest_in_order() {
        let (_dir, pool, _wal, file) = fixture("delete-half", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        for key in shuffled(1000) {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        for key in (0..1000).step_by(2) {
            tree.delete(&Value::Integer(key))
                .unwrap()
                .expect("the key was there");
        }
        let odd: Vec<i64> = (1..1000).step_by(2).collect();
        assert_eq!(keys(&tree), odd);
        for key in &odd {
            assert!(
                tree.get(&Value::Integer(*key)).unwrap().is_some(),
                "key {key}"
            );
        }
    }

    #[test]
    fn fifty_thousand_keys_go_in_at_random_and_read_back_in_order() {
        // A pool far smaller than the tree, so pages are evicted throughout.
        let (_dir, pool, wal, file) = fixture("load", 64);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        let count = 50_000;
        for (written, key) in shuffled(count).into_iter().enumerate() {
            tree.insert(&int_row(&table, key)).unwrap();
            if written % 1000 == 0 {
                settle(&pool, &wal);
            }
        }
        settle(&pool, &wal);
        assert_eq!(keys(&tree), (0..count).collect::<Vec<i64>>());
        assert!(
            pool.held() <= pool.frame_count(),
            "the pool grew past its cap"
        );

        for (gone, key) in (0..count).step_by(2).enumerate() {
            tree.delete(&Value::Integer(key))
                .unwrap()
                .expect("the key was there");
            if gone % 1000 == 0 {
                settle(&pool, &wal);
            }
        }
        settle(&pool, &wal);
        assert_eq!(keys(&tree), (1..count).step_by(2).collect::<Vec<i64>>());
        assert_eq!(pool.held(), pool.frame_count());
    }

    #[test]
    fn a_row_whose_key_is_null_is_refused() {
        let (_dir, pool, _wal, file) = fixture("null-key", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        let row = codec::encode_row(
            &table.columns,
            &[Cell::Value(Value::Null), Cell::Value(Value::Null)],
        )
        .unwrap();
        let e = tree.insert(&row).err().unwrap();
        assert!(e.message.contains("holds no primary key"), "{e}");
    }

    #[test]
    fn a_key_that_lives_in_a_chain_is_refused() {
        let (_dir, pool, _wal, file) = fixture("chain-key", 8);
        // A key column of text could in principle hold a value too large for
        // a record, and a key the tree cannot read is no key at all.
        let table = table(DataType::Text);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        let chain = Cell::Chain(codec::ChainPtr { head: 9, len: 4096 });
        let row =
            codec::encode_row(&table.columns, &[chain.clone(), Cell::Value(Value::Null)]).unwrap();
        let e = tree.insert(&row).err().unwrap();
        assert!(e.message.contains("too large for a record"), "{e}");

        // And the same for a separator key read off an interior page.
        let separator = codec::encode_row(tree.key_column(), &[chain]).unwrap();
        let e = tree.decode_key(&separator).err().unwrap();
        assert!(e.message.contains("too large for a record"), "{e}");
    }

    #[test]
    fn a_page_that_is_no_part_of_a_tree_is_an_error() {
        let (_dir, pool, _wal, file) = fixture("wrong-kind", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        tree.insert(&int_row(&table, 1)).unwrap();

        // The root becomes a page of a kind no tree holds.
        let root = tree.root().unwrap();
        {
            let mut page = pool.fetch(tree.page(root)).unwrap();
            SlottedPage::init(page.bytes_mut(), PageKind::Overflow);
        }
        let e = tree.get(&Value::Integer(1)).err().unwrap();
        assert!(e.message.contains("no part of a tree"), "{e}");
        let e = tree
            .cursor(Bound::Unbounded, Bound::Unbounded)
            .err()
            .unwrap();
        assert!(e.message.contains("no part of a tree"), "{e}");
    }

    #[test]
    fn an_emptied_leaf_is_unlinked_from_the_leaf_before_it() {
        let (_dir, pool, _wal, file) = fixture("unlink-middle", 8);
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);
        for key in 0..400 {
            tree.insert(&int_row(&table, key)).unwrap();
        }
        // Backwards, so the leaf that empties first has one before it and
        // none after it.
        for key in (0..400).rev() {
            tree.delete(&Value::Integer(key))
                .unwrap()
                .expect("the key was there");
        }
        assert_eq!(keys(&tree), Vec::<i64>::new());
        assert_eq!(tree.root().unwrap(), 0);
    }

    #[test]
    fn two_keys_of_different_types_cannot_be_put_in_order() {
        let e = compare(&Value::Integer(1), &Value::Text("one".to_string()))
            .err()
            .unwrap();
        assert_eq!(e.code, ErrorCode::TypeMismatch);
        assert_eq!(
            compare(&Value::Integer(1), &Value::Integer(2)).unwrap(),
            Ordering::Less
        );
    }

    #[test]
    fn a_run_of_writes_that_never_commits_fills_the_log() {
        // A log of room for a few hundred frames, and a pool small enough
        // that every write evicts a page into it.
        let dir = Dir::new("log-full");
        let pool = BufferPool::with_frames(2);
        let wal = Arc::new(Wal::open(&dir.0, 200 * PAGE_FRAME as u64).unwrap());
        let file = pool
            .open(&dir.0.join("1.tbl"), 1, Arc::clone(&wal))
            .unwrap();
        let table = table(DataType::Integer);
        let tree = BTree::open(&pool, file, &table, Mark::Latest);

        // One transaction holds its whole change set in the log, so a long
        // enough one runs out of log.
        let mut written = 0;
        let full = loop {
            match tree.insert(&int_row(&table, written)) {
                Ok(()) => written += 1,
                Err(e) => break e,
            }
            assert!(written < 10_000, "the log never filled");
        };
        assert_eq!(full.code, ErrorCode::StorageFull);
        assert!(written > 0, "something went in before the log filled");

        // Reads do not carry on here, and a pool this small is why. Taking a
        // frame can mean evicting a page that was changed, and a changed page
        // has nowhere to go while the log is full. Nothing can drop those
        // pages until there is a transaction to abort, which is milestone 10.
        // A pool of a realistic size holds clean frames to take instead.
        assert_eq!(pool.frame_count(), 2);
    }

    /// The keys 0 to `count`, in an order that is not sorted and repeats each
    /// run, so a failure is the same failure next time.
    fn shuffled(count: i64) -> Vec<i64> {
        let mut keys: Vec<i64> = (0..count).collect();
        // A multiplier coprime with the count would skip, so swap by a
        // sequence that visits every index.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        for index in (1..keys.len()).rev() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let other = (state >> 33) as usize % (index + 1);
            keys.swap(index, other);
        }
        keys
    }
}
