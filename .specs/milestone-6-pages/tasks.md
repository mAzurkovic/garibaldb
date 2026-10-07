# Milestone 6, Pages and the buffer pool — Tasks

Source: `docs/projectplan.md` M6. Types: `docs/internals.md` section 5.
Requirements: [NFR6], [NFR7], [NFR10]-[NFR13]. Formats: the page and record section of
`docs/design.md`, and decisions D3 and D4.

Crate `crates/server`, module `src/store/`. The milestone ends with 8 KB pages that go to a file
and come back inside a fixed amount of memory. Nothing reads a row through them until the
B-tree arrives in milestone 7.

Settled before this list. Do not reopen.
- One buffer pool for the whole server. `PageId` is a file id and a page number, and the file id
  is handed out when the pool first opens a `.tbl`. It never reaches the disk, so no database
  needs a number of its own and `design.md` keeps its rule that there is no server-level
  catalog. Sections 4 and 5 of `internals.md` need the edit: `Database` holds no pool, and
  `PageId` holds no table id.
- A test in the per-build suite writes at most 16 MB, into a temp directory that it deletes. A
  16 MB file against a 1 MB pool is sixteen times the pool, which exercises eviction in under a
  second and leaves nothing on the machine that ran it.
- The figures of section 3.1, [NFR1] at 100 GB and [NFR2] at a billion rows, belong to
  milestone 12. That milestone is the capacity one and runs suites [S13] to [S16] weekly. This
  milestone proves that eviction is correct, not that the numbers hold.
- [H6], the resource monitor, is milestone 12's as well. This milestone asserts the frame count
  against the cap, which is the part of [NFR11] it can prove from inside the process.
- No WAL yet. A dirty page is written straight to its `.tbl` file when it is evicted or flushed.
  That is not crash-safe, and milestone 9 sends the write through the WAL instead.
- The pool takes its size from the memory limit, using the share that the budget in `design.md`
  gives it, so `--mem-limit` stays the only knob.
- A page is 8 KB, fixed at build time, which is decision D4.
- Page 0 of a file is its header: the B-tree root pointer and the free list head. Milestone 7
  reads the root. This milestone writes both.
- An overflow pointer is 12 bytes: a page number as a `u32` and a length as a `u64`. A value
  over 2 KB moves out of the record and leaves the pointer behind.
- A page LSN is written as zero. Milestone 9 is the first milestone with an LSN to write.
- Nothing outside a test calls the store until milestone 7, so `src/store/mod.rs` carries one
  `allow(dead_code)` with a note that names it, the way `Session::cancel` already does.

## 1. The page [serial]

- [x] 1.1 Add `src/store/mod.rs` declaring `page`, `codec`, `pool`, and `overflow`, with the
  `allow(dead_code)` note, and declare `mod store` in `main.rs`. `cargo check` passes.
- [x] 1.2 Add `PAGE_SIZE` of 8192 and `PageId`, holding a `FileId` and a page number, to
  `src/store/page.rs`. `PageId` is `Copy`, `Eq`, and `Hash`, because it keys the page table.
- [x] 1.3 Add `PageHeader` to `src/store/page.rs`: an LSN `u64`, a kind `u8`, a slot count `u16`,
  and a free offset `u16`. It reads from and writes to the first bytes of a page.
- [x] 1.4 Add `SlottedPage`. Slots grow from the front and records from the back, so
  `free_space` is the gap between them. `insert` returns the slot, and a record that does not
  fit is refused rather than truncated.
- [x] 1.5 Add `slot` and `remove`. A removed slot leaves a hole, and `free_space` counts it only
  after a compaction, which `insert` runs when the gap alone is too small.
- [x] 1.6 Add `FileHeader` for page 0, holding the B-tree root page and the free list head.
- [x] 1.7 Test the page: a header round-trips, records fill a page until one is refused, a slot
  reads back the bytes it was given, a remove makes room for a record of the same size, and a
  record of 1 MB is refused because no page holds one. Gives [NFR6] its boundary.

## 2. The record codec [parallel, needs 1.1]

- [x] 2.1 Add `encode_row` to `src/store/codec.rs`: a null bitmap of one bit for each column,
  then each value that is not null, in the order the columns hold.
- [x] 2.2 Encode each type at its documented width: `INTEGER` 8 bytes, `BOOLEAN` 1 byte,
  `DECIMAL` a sign byte and a scale byte and a 16-byte integer, `TEXT` a length and its UTF-8
  bytes.
- [x] 2.3 Add `decode_row`, which reads a row back against the columns of its table. A row whose
  bytes run out is an error and never a panic.
- [x] 2.4 Add the 12-byte overflow pointer and the 2 KB threshold: a value over the threshold
  encodes as its pointer, and `encode_row` reports which values need a chain.
- [x] 2.5 Test the codec: every type round-trips, a null round-trips, a row of 100 columns
  round-trips for [NFR5], and bytes that are too short are an error.

## 3. The buffer pool [serial, needs 1]

- [x] 3.1 Add `FileId` and the file table to `src/store/pool.rs`. `open` takes the path of a
  `.tbl`, returns a `FileId`, and returns the same one for a path it already holds.
- [x] 3.2 Add `PoolFrame`, holding its page bytes, its `PageId`, a pin count, a dirty flag, and
  a reference bit. Add `BufferPool::new`, which takes the frame count from the memory limit.
- [x] 3.3 Add `fetch` and `fetch_for_write`, which pin a frame, and `unpin`, which takes whether
  the caller wrote to it. A page that is already in the pool is not read again.
- [x] 3.4 Add `evict` by the clock method: sweep the frames, clear a reference bit that is set,
  and take the first frame whose bit is clear and whose pin count is zero. A dirty frame is
  written to its file before it is reused. Gives [NFR10] and [NFR11].
- [x] 3.5 Refuse a `fetch` with `STORAGE_FULL` when every frame is pinned, so a pool that is out
  of room is an error and never a wait or a panic.
- [x] 3.6 Add `allocate_page` and `free_page`, which take from and return to the free list in
  page 0, and `flush_all`, which writes every dirty frame.
- [x] 3.7 Test the pool: a page written then read gives the same bytes, a pinned page is never
  evicted, a dirty page reaches its file, a clean page does not need a write, a freed page is
  handed out again, and a full pool refuses.

## 4. Overflow chains [serial, needs 2 and 3]

- [x] 4.1 Add `OverflowChain::write` to `src/store/overflow.rs`. It takes the bytes of one
  value, fills a page at a time from the free list, and links each page to the next.
- [x] 4.2 Add `OverflowChain::read`, which follows the links and rebuilds the value.
- [x] 4.3 Add `OverflowChain::free`, which returns every page of a chain to the free list.
- [x] 4.4 Test overflow: a 1 MB value round-trips for [NFR7], a value one byte over the
  threshold takes one page, a free returns every page, and a chain survives eviction of its
  middle page.

## 5. Eviction under load [serial, needs 3 and 4]

- [x] 5.1 Test that 16 MB written through a 1 MB pool reads back byte for byte, in
  `src/store/pool.rs`. The data is sixteen times the pool, so most pages are evicted and read
  again.
- [x] 5.2 Test that the frame count never passes the cap, during that write and after it. Gives
  [NFR11] the part it can prove from inside the process.
- [x] 5.3 Test that a pool of one frame still reads and writes correctly, which is eviction on
  every single fetch.
- [x] 5.4 Update sections 4 and 5 of `docs/internals.md`: `Database` holds no pool, and `PageId`
  holds a file id rather than a table id.

## Dependencies

- 1.1 blocks everything.
- 2 runs beside 1, because a record holds no page.
- 3 needs 1.
- 4 needs 2 and 3.
- 5 needs 3 and 4.
- 5.4 is a document edit and can land at any point.

## Done when

- A page written through the pool and read after its frame was evicted gives the same bytes.
- 16 MB through a 1 MB pool reads back correctly, and the frame count never passes the cap.
- A 1 MB value round-trips through an overflow chain.
- A pinned page is never evicted, and a pool with no free frame refuses rather than waits.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
