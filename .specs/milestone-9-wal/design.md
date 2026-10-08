# Milestone 9, the WAL and recovery — Design

Sources: `docs/projectplan.md` M9, the durability section and decision D1 of `docs/design.md`,
`docs/internals.md` section 4, [FR56]-[FR60], [NFR18]-[NFR19], and harness [H3] with section 4.4
of `docs/testplan.md`. Built on the store and executor of milestones 6 to 8.

## Overview

- Committed data outlives the process, and a change that never committed leaves no trace.
- A redo-only log: a changed page is appended to the WAL, never written to the table file until
  a checkpoint moves it, and a read looks in the log before the file.

## Components

- `Wal`: appends frames, syncs, reads a page back, truncates. One per database.
  `crates/server/src/wal/writer.rs`.
- `FrameIndex`: the newest frame holding each page, bounded by a mark. `src/wal/index.rs`.
- `recover`: reads a log forward at startup and settles what survived. `src/wal/recovery.rs`.
- `checkpoint`: copies the pages of a log into their table files, then empties the log.
  `src/wal/checkpoint.rs`.
- `BufferPool` learns a WAL for each file it opens, so an eviction appends a frame and a read
  consults the index. `src/store/pool.rs`.
- `Database` gains its `Wal`. `Registry` opens it when it first opens the database.
- `Crash`: the fault injection a test uses to discard writes that were never synced.
  `src/wal/writer.rs`, under `cfg(test)`.

## Data Model

A frame, in `<database>/wal/000.wal`:

- `lsn` `u64`, `kind` `u8`, `table` `u32`, `page_no` `u32`, `crc` `u32`, then the page for a page
  frame and nothing for a commit frame. So 21 bytes of header, and 8213 bytes in all for a page.
- The `crc` is CRC32 over the header before it and the payload after it, which is what makes a
  half-written frame recognisable.
- `table` is the table id, not a `FileId`. A `FileId` is handed out at runtime and would mean
  nothing after a restart; a table id is the name of its file.
- A commit frame carries no page. Fixing every frame at 8213 bytes would waste 8 KB on each
  commit, and [NFR16] asks for a thousand a second.

`FrameIndex` holds a map from `(table, page_no)` to the offset of the newest frame for it.

## Interfaces

- `Wal::open(dir) -> io::Result<Wal>`: opens or makes the log of a database.
- `Wal::append_page(table, page_no, bytes) -> Result<(), DbError>`
- `Wal::commit() -> Result<(), DbError>`: appends a commit frame and syncs. A sync that fails
  marks the log broken.
- `Wal::read_page(table, page_no, upto) -> Result<Option<Box<Page>>, DbError>`: the newest frame
  at or before the mark, or none when the log holds that page not.
- `Wal::end() -> u64`: the mark a reader takes. Milestone 10 hands it a snapshot instead.
- `Wal::size() -> u64` and `Wal::truncate()`.
- `BufferPool::open(path, table, wal)`: the file, its table id, and the log that owns its writes.
- `BufferPool::commit(wal)`: appends every dirty page of that log's files, then commits.
- `recover(dir) -> io::Result<Recovered>`: the index of what committed, after the uncommitted
  tail is cut away.
- `checkpoint(pool, wal) -> Result<(), DbError>`

## Flow

A statement that changes rows:

- 1. Pages change in the pool as they do today.
- 2. An eviction of a dirty page appends a page frame. It is uncommitted, and recovery will
  discard it if nothing commits after it. This is what lets the pool stay a fixed size without
  an undo log.
- 3. The statement ends: every dirty page of this database appends a page frame, then a commit
  frame, then one `fsync`.

A read of a page:

- 1. The pool holds it: use it.
- 2. Otherwise ask the log for the newest frame at or before the reader's mark.
- 3. Otherwise read the table file.

Startup, before any connection is accepted:

- 1. For each database, read its log forward, checking each frame's CRC.
- 2. Stop at the first bad CRC, and remember the last commit frame before it.
- 3. Cut the log back to just after that commit frame, so nothing uncommitted remains.
- 4. Build the index from the frames that survived.
- 5. Log that recovery started and finished, then accept connections.

A checkpoint, when the log passes 64 MB at the end of a statement:

- 1. Write every page the index names into its table file.
- 2. `fsync` each table file that was touched.
- 3. Empty the log and clear the index.

## Key Decisions

- D1. The pool learns a WAL for each file it opens. Alternative: a per-file object that is "log
  plus table file", which the pool reads and writes through blindly. Why: the smallest change to
  what milestone 6 built, and page 0 bootstrap and the startup sweep still need the raw file.
- D2. `kill -9` plus an in-process injection that discards writes never synced. Alternative:
  block-device fault injection with `dm-flakey`. Why: the kill proves [FR56] anywhere, and the
  injection proves recovery copes with a torn log, a half-written frame and a bad checksum,
  which is the code recovery exists for. [FR57] against a real disk stays unproven, and a
  release has to say so.
- D3. A checkpoint runs at the end of a statement, not on a thread of its own. Alternative: the
  background thread that `design.md` names. Why: there is no write lock until milestone 10, so a
  thread would truncate the log while a writer appends to it. The cost is a client waiting
  through one checkpoint, and milestone 10 moves it off the statement once the lock exists.
- D4. One log file for each database, not a list of segments. Alternative: the segments that
  `internals.md` shows. Why: a checkpoint empties the whole log, so nothing needs to truncate a
  prefix while the tail is being written. Segments earn their place when a reader holds a
  snapshot that pins the front of the log, which is milestone 10.
- D5. A frame names its table by the table id. Alternative: a `FileId`. Why: a `FileId` is
  handed out at runtime and means nothing to the next process.
- D6. A failed `fsync` marks the log broken and refuses every later commit, and the server keeps
  running. Alternative: stop the process, which `design.md` says. Why: [NFR9] wants reads to
  keep working after a write is refused, and a reader can still be served from pages that are
  already safe.
- D7. A reader passes the end of the log as its mark. Alternative: nothing, and read the newest
  frame always. Why: `read_page` takes the mark that milestone 10 needs for a snapshot, so the
  shape is right from the start and the behaviour today is the same.
- D8. Recovery runs in `Server::bind`, before the listener accepts. Alternative: on first use of
  each database. Why: [FR60] says connections wait for recovery, and [FR59] says an operator
  does nothing.

## Risks

- Two connections can still write to one table at the same time. The log serialises its own
  appends, but a B-tree split spans several pages and nothing yet makes that atomic against
  another writer. The write lock in milestone 10 closes it. It is no worse than milestone 8,
  which had neither a log nor a lock.
- `FrameIndex` is a map from a pair of `u32` to a `u64`. The budget in `design.md` allows 16 MB
  for an 8 GB log, which is about a million entries at 12 bytes each. A `HashMap` costs more
  than that per entry, so the index will want a tighter form before the log is allowed to reach
  8 GB.
- A checkpoint copying 64 MB holds up the statement that triggered it. [NFR16] asks for a
  thousand commits a second, and one stall of that size is visible in a P99.
- The 100 power cuts of the done-when are what D2 gives up. [T10] to [T13] are provable only in
  the process-crash and injected forms until the block-device harness exists.
