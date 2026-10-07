# Milestone 7, the B-tree — Tasks

Source: `.specs/milestone-7-btree/design.md`, and `docs/projectplan.md` M7.
Requirements: [FR28], [NFR2]. Types: `docs/internals.md` section 5.

Crate `crates/server`, module `src/store/btree.rs`. The milestone ends with an ordered map from
primary key to row, on disk. Nothing runs a statement through it until milestone 8.

Settled before this list. Do not reopen. Every one of these is a decision of the design, which
holds the alternative it was taken over.
- A key is decoded and compared with `Value::compare`, not stored as order-preserving bytes.
  `12.20` and `12.2` have to compare equal, which byte order cannot give.
- A node is freed only once it is empty. Nothing borrows from a sibling and nothing merges two
  part-full pages.
- Leaves link both ways. Unlinking an empty leaf is then two pointer writes, and the backward
  scan that milestone 11 wants comes with it.
- A descent holds page numbers, not pinned pages, so the pool needs about three frames whatever
  the depth of the tree.
- Every child of an interior page carries a key, so no page holds a rightmost pointer of its own.
- The tree deals in encoded rows and frees no chain. `delete` hands the row back and the caller
  frees what lived in a chain.
- The per-build test inserts about 50,000 keys. Milestone 6 settled that a per-build test writes
  at most 16 MB, and the 10 million of the done-when belongs to milestone 12.
- Nothing outside a test calls the tree yet, so it sits under the `allow(dead_code)` that
  `src/store/mod.rs` already carries, with milestone 8 named as the first caller.

## 1. The page changes the tree needs [serial]

- [x] 1.1 Add `next` and `prev` to `PageHeader` in `src/store/page.rs`, read and written with the
  rest of it. `HEADER_SIZE` becomes 21. Every page test still passes.
- [x] 1.2 Add `Leaf` and `Interior` to `PageKind`, and give `SlottedPage::init` and
  `SlottedPage::open` the kind they are to work on.
- [x] 1.3 Move the chain link in `src/store/overflow.rs` onto `PageHeader::next`, so a chain page
  holds no link of its own. `PER_PAGE` changes, and every overflow test still passes.
- [x] 1.4 Test the header: `next` and `prev` round-trip, a page at either end of a chain of
  leaves reads zero, and a page of one kind is refused when another is asked for.

## 2. Keys and the node layout [serial, needs 1]

- [x] 2.1 Add `src/store/btree.rs` holding `BTree`, with the pool, the file, the columns of its
  table, and which column is the key. Declare it in `src/store/mod.rs`.
- [x] 2.2 Add `key_of`, which reads the key out of an encoded row through `codec::decode_row`. A
  row whose key is null, or whose key lives in a chain, is an error and not a panic.
- [x] 2.3 Add the interior entry: a separator key encoded against the key column alone, then the
  child page number. One function writes an entry and one reads it back.
- [x] 2.4 Add `search`, a binary search over the slots of a page that returns either the slot
  holding a key or the slot it would take, comparing through `Value::compare`.
- [x] 2.5 Test the layout: a key of each type round-trips through an entry, a search finds every
  key of a full page, a search returns the right place for a key that is absent, and a search of
  an empty page returns the first slot.

## 3. Insert and split [serial, needs 2]

- [x] 3.1 Add `descend`, which walks from the root to the leaf that holds a key and returns the
  path as page numbers and slots. Only the page in hand is pinned.
- [x] 3.2 Add `insert` for a tree with room: an empty tree allocates its first leaf and writes the
  page number into `FileHeader::root`, and a leaf with space takes the row in key order.
- [x] 3.3 Refuse a key the tree already holds, with `DUPLICATE_KEY`. Gives [FR28].
- [x] 3.4 Add `split_leaf`, which divides the rows so each page holds about half the bytes, links
  the new page between its neighbours both ways, and returns the lowest key of the right half.
- [x] 3.5 Add the climb: put the separator into the parent, split the parent the same way when it
  has no room, and allocate a new root when the root itself splits.
- [x] 3.6 Test insert: keys in order, keys in reverse, keys at random, a row near the size one
  page holds, a duplicate of each type, and that enough rows make a tree of more than one level.

## 4. Get and the cursor [serial, needs 3]

- [x] 4.1 Add `get`, which descends to a key and returns its row, or none when the tree holds it
  not.
- [x] 4.2 Add `Cursor` and `BTree::cursor`, taking a pair of `std::ops::Bound<Value>` and
  starting at the first key the range holds.
- [x] 4.3 Add `Cursor::next`, which walks the slots of a leaf, follows `next` to the leaf on the
  right, and stops at the upper bound.
- [x] 4.4 Test reads: a full scan returns every key in order, each form of bound takes and leaves
  the right rows, a scan of an empty tree returns nothing, and a get of an absent key returns
  none.
- [x] 4.5 Test that a scan crosses many leaves, and that a scan of a tree of more than one level
  returns the same order as the keys went in.

## 5. Delete [serial, needs 4]

- [x] 5.1 Add `delete`, which descends, takes the row out of its leaf, and hands the row back so
  the caller can free what lived in a chain.
- [x] 5.2 Unlink a leaf that lost its last row: the leaves on either side take each other, the
  parent loses the slot, and the page goes back to the free list.
- [x] 5.3 Climb while a page is left with no child, and write the root back to zero once the last
  leaf goes, so an emptied tree is an empty tree again.
- [x] 5.4 Test delete: an absent key returns none, a deleted key is gone from a get and from a
  scan, deleting every key empties the tree, and the pages come back to the free list.
- [x] 5.5 Test the tree under load: about 50,000 keys inserted at random read back in order,
  deleting half leaves the other half readable in order, and the pool never passes its cap.

## Dependencies

- 1 blocks everything, because the tree needs the links and the kinds that it adds.
- 2 needs 1. 3 needs 2. 4 needs 3. 5 needs 4.
- 1.3 only moves the chain link and can land with 1.1.

## Done when

- Keys inserted at random read back in key order, through a tree of more than one level.
- Deleting half the keys leaves the rest readable in order, and the pages of the emptied leaves
  return to the free list.
- A duplicate primary key is refused with `DUPLICATE_KEY`.
- A scan crosses leaves by their links, and the pool never holds more frames than its cap.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
