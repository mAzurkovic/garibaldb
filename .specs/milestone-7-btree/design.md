# Milestone 7, the B-tree — Design

Sources: `docs/projectplan.md` M7, the storage layer of `docs/design.md`, `docs/internals.md`
section 5, [FR28] and [NFR2]. Built on the store from milestone 6.

## Overview

- An ordered map from primary key to row, on disk. The tree is the table, so a leaf holds the
  whole row and not a pointer to it.
- A B+tree over the buffer pool. Interior pages hold separator keys and child page numbers,
  leaves hold encoded rows in key order and link to the leaf on their right.

## Components

- `BTree`: `get`, `insert`, `delete`, `cursor`. Holds the pool, the file, the columns of its
  table, and which column is the key. `crates/server/src/store/btree.rs`.
- `Cursor`: walks leaves in key order, from one bound to another. Same file.
- Reads and writes pages through `BufferPool`, rows and keys through `codec`, and finds the root
  in `FileHeader::root` of page 0.

## Data Model

`PageHeader` gains `next` and `prev` page numbers, so it is 21 bytes rather than 13. A leaf uses
both, for the leaves on either side of it. A chain page uses `next` for the page that follows it
and leaves `prev` at zero. `PageKind` gains `Leaf` and `Interior`, and `SlottedPage::init` takes
the kind it is making.

- Leaf page: slotted. One encoded row for each slot, in key order. `next` and `prev` are the
  leaves on either side, and zero at the ends of the tree.
- Interior page: slotted. One slot for each child: the separator key, then the child page number
  as a `u32`. Slot `i` covers the keys from `key[i]` up to `key[i + 1]`, so the leftmost slot
  carries the lowest key of its subtree and the page needs no rightmost pointer of its own.
- A separator key is encoded by `codec::encode_row` against the key column alone, so one encoder
  serves both a row and a key.
- An empty tree is `root == 0`. The first insert allocates one leaf and writes its number there.

## Interfaces

- `BTree::open(pool: &BufferPool, file: FileId, table: &TableDef) -> BTree`.
- `get(&self, key: &Value) -> Result<Option<Vec<u8>>, DbError>`: the encoded row.
- `insert(&self, row: &[u8]) -> Result<(), DbError>`: `DUPLICATE_KEY` when the key is already
  there, which gives [FR28]. The key is read out of the row.
- `delete(&self, key: &Value) -> Result<Option<Vec<u8>>, DbError>`: hands the row back, so the
  caller can free the chains that its large values live in.
- `cursor(&self, from: Bound<Value>, to: Bound<Value>) -> Result<Cursor<'_>, DbError>`, with
  `std::ops::Bound`. A full scan is `Unbounded` to `Unbounded`.
- `Cursor::next(&mut self) -> Result<Option<Vec<u8>>, DbError>`.

## Flow

Insert:

- 1. Descend from the root, recording the page number and slot at each level. Only the page in
  hand is pinned.
- 2. At the leaf, binary search the key. An equal key is `DUPLICATE_KEY`.
- 3. The row fits: insert it in key order and stop.
- 4. The row does not fit: split the leaf so each half holds about half the bytes, point the left
  half's `next` at the new right half, and hand the lowest key of the right half to the parent.
- 5. The parent has no room for that key: split it the same way, and keep going up the path.
- 6. The root splits: allocate a new root with two children and write its number to
  `FileHeader::root`.

Delete:

- 1. Descend and remove the row from its leaf.
- 2. The leaf still holds a row: stop. A page half emptied stays half full.
- 3. The leaf holds none: unlink it from the leaves on both sides, remove its slot from the
  parent, and return its page to the free list. Repeat upward while a page is left with no
  child.

## Key Decisions

- D1. Decode the key and compare with `Value::compare`. Alternative: store an order-preserving
  byte key and compare with `memcmp`. Why: [FR39] needs `12.20` and `12.2` to compare equal,
  which byte order cannot give without a normalising encoding, and the comparison is already
  written and tested in `protocol`.
- D2. A node is freed only once it is empty. Alternative: borrow from a sibling, or merge, as
  soon as a node falls under half full. Why: the smallest code that keeps a tree correct, and
  the part of a B-tree most likely to hide a subtle bug. Space returns when a page empties
  rather than as rows leave.
- D3. A leaf links to the leaf on its right, and a scan follows the link. Alternative: descend
  from the root again for each leaf. Why: a scan then reads one page for each page of the table
  instead of one for each level, which is what [NFR17] asks for.
- D4. A descent holds page numbers, not pinned pages. Alternative: pin every page of the path.
  Why: the pool needs about three frames whatever the depth of the tree, so a small pool still
  works and a deep tree cannot exhaust it.
- D5. Every child of an interior page carries a key. Alternative: `n` children and `n - 1` keys,
  with the last child in the page header. Why: one slot layout for every entry, at the cost of
  one key per page that a search never needs.
- D6. `PageHeader` carries `next` and `prev` page numbers for every kind of page. Alternative: a
  reserved area after the header, sized by the kind. Why: `next` serves both the right sibling of
  a leaf and the next link of a chain, so `overflow.rs` drops a parallel concept it owns today.
- D10. Leaves link both ways. Alternative: find the predecessor of an empty leaf by climbing the
  path and descending again. Why: unlinking becomes two pointer writes instead of the fiddliest
  code in the delete path, and the backward scan that `ORDER BY DESC` wants in milestone 11 comes
  with it. Costs 4 bytes of every page.
- D7. The tree deals in encoded rows and frees no chain. Alternative: the tree owns the chains
  of the rows it holds. Why: `delete` hands the row back, and the caller already holds the
  schema that says which values live in a chain.
- D8. A split divides by bytes, not by slot count. Alternative: split at the middle slot. Why:
  rows vary in size, and halving the bytes is what leaves both pages able to take the next
  insert.
- D9. The per-build test inserts about 50,000 keys, a few MB. Alternative: the 10 million of the
  done-when. Why: milestone 6 settled that a per-build test writes at most 16 MB and that the
  figures of section 3.1 belong to milestone 12, which runs the weekly suites.

## Risks

- A split that fails halfway leaves the tree wrong, and nothing undoes it until the WAL arrives
  in milestone 9. A test that fills the free list and then splits is the way to see it.
- `[NFR2]` wants a billion rows, which is four levels at 8 KB. The descent is recursive in shape
  but written as a loop, so depth costs no stack.
