# Milestone 9, the WAL and recovery — Tasks

Source: `.specs/milestone-9-wal/design.md`, and `docs/projectplan.md` M9.
Requirements: [FR56]-[FR60], [NFR18]-[NFR19]. Types: `docs/internals.md` section 4.

Crate `crates/server`, module `src/wal/`, plus changes to `src/store/pool.rs`. The milestone
ends with committed rows that outlive the process and uncommitted ones that leave no trace.

Settled before this list. Do not reopen. Each is a decision of the design, which holds the
alternative it was taken over.
- The pool learns a WAL for each file it opens. An eviction appends a frame, and a read asks the
  index before the file.
- The log is not optional. Every caller of `BufferPool::open` passes one, tests included, so the
  storage write path has no branch that production never takes.
- The pool holds its logs as `Arc<Wal>`. A borrow would put a lifetime on the pool, which every
  type that holds one would then carry.
- `kill -9` and an in-process injection that discards writes never synced. No block-device fault
  injection, so a real power cut stays unproven and a release has to say so.
- A checkpoint runs at the end of a statement, not on a thread. There is no write lock until
  milestone 10, and a thread would empty the log while a writer appended to it.
- One log file for each database, not a list of segments.
- A frame names its table by the table id, because a `FileId` means nothing to the next process.
- A failed `fsync` marks the log broken and refuses every later commit, and the server keeps
  running, because [NFR9] wants reads to carry on after a write is refused.
- A reader passes the end of the log as its mark. The shape is what milestone 10's snapshot
  needs, and the answer today is the same either way.
- `crc32fast` joins the dependencies of `crates/server`. The crate table in `docs/design.md`
  already assigns it there, for frame checksums.

## 1. The log [serial]

- [x] 1.1 Add `crc32fast` to the workspace dependencies and to `crates/server`, and add
  `src/wal/mod.rs` declaring `writer`, `index`, `recovery`, and `checkpoint`. Declare `mod wal`
  in `main.rs`. `cargo check` passes.
- [x] 1.2 Add the frame to `src/wal/writer.rs`: an LSN, a kind, a table id, a page number, a
  CRC32, then the page for a page frame and nothing for a commit frame. One function writes a
  frame and one reads it back, and a frame whose CRC disagrees reads as broken.
- [x] 1.3 Add `FrameIndex` to `src/wal/index.rs`: the offset of the newest frame for each table
  and page, and `newest` bounded by a mark. Gives the snapshot its shape.
- [x] 1.4 Add `Wal` with `open`, `append_page`, `commit`, `read_page`, `end`, `size`, and
  `truncate`. `commit` appends a commit frame and syncs once. Appends hold a lock, so two
  writers cannot interleave the bytes of a frame.
- [x] 1.5 Mark the log broken when a sync fails, and refuse every later commit with
  `STORAGE_FULL`. A read still works. Gives [NFR9] its reach into a broken log.
- [x] 1.6 Test the log against a temp directory: a page read back after an append, the newest of
  two frames for one page, a mark that hides the later one, a page the log holds not, a CRC that
  disagrees, and a broken log refusing a commit.

## 2. The pool writes through the log [serial, needs 1]

- [x] 2.1 Give `BufferPool::open` the table id and an `Arc<Wal>`, and keep both beside the file.
  Every caller passes them, `Registry::table_file` from its `TableDef` and its database.
- [x] 2.2 Append a frame when a dirty page is evicted, rather than writing the table file. The
  frame is uncommitted, and recovery drops it unless a commit follows.
- [x] 2.3 Read a page from the log first and the table file second, at the reader's mark. A page
  the pool already holds is still answered from memory.
- [x] 2.4 Add `BufferPool::commit`, which appends every dirty page of one log's files and then
  commits. It replaces the flush that milestone 8 put at the end of a statement.
- [x] 2.5 Add `BufferPool::write_to_file`, which a checkpoint uses to put a page where the log
  is not. Nothing else writes a table file.
- [x] 2.6 Give every store test fixture a log: `pool.rs`, `overflow.rs`, `btree.rs`, `row.rs`.
  Every one of their tests passes unchanged in meaning.
- [x] 2.7 Test the pool through a log: a page written, evicted and read back, a page in the log
  but not the file, a commit that leaves nothing dirty, and a read at a mark before a write.

## 3. Recovery [serial, needs 2]

- [x] 3.1 Add `recover` to `src/wal/recovery.rs`: read a log forward, check each CRC, stop at the
  first that disagrees, and remember the last commit frame before it.
- [x] 3.2 Cut the log back to just after that commit frame, so no uncommitted frame remains.
  Gives [FR58].
- [x] 3.3 Build the `FrameIndex` from the frames that survived, and hand it to the `Wal` the
  database opens with. Gives [FR56].
- [x] 3.4 Open and recover each database in `Registry`, and run it from `Server::bind` before the
  listener accepts. Log that recovery started and finished. Gives [FR59], [FR60], [FR83].
- [x] 3.5 Test recovery: a log of committed frames comes back whole, a tail with no commit is
  dropped, a half-written frame at the end is dropped, a bad CRC in the middle stops the read
  there, and an empty or missing log recovers to an empty index.
- [x] 3.6 Test that recovery of one database leaves the others alone, and that a second recovery
  of an already-recovered log changes nothing.

## 4. The checkpoint [serial, needs 3]

- [x] 4.1 Add `checkpoint` to `src/wal/checkpoint.rs`: write every page the index names into its
  table file, `fsync` each file it touched, then empty the log and clear the index.
- [x] 4.2 Run it at the end of a statement when the log has passed 64 MB, from the same place
  that commits. A checkpoint that fails leaves the log alone, so the data is still in it.
- [x] 4.3 Refuse an append that would take the log past its limit, with `STORAGE_FULL`, and keep
  answering reads. Gives [NFR8] and [NFR9] for the log.
- [x] 4.4 Test the checkpoint: pages reach their files, the log empties, a read after it comes
  from the file, and the rows are the same across it.
- [x] 4.5 Test that a checkpoint is triggered by size and not by every statement, and that a log
  at its limit refuses a write but answers a read.

## 5. Crashes [serial, needs 4]

- [x] 5.1 Add the injection to `src/wal/writer.rs` under `cfg(test)`: a log that forgets every
  write that was never synced, which is what a power cut leaves behind.
- [x] 5.2 Test with it that a committed statement survives, that an uncommitted one leaves no
  row, and that a cut in the middle of a frame leaves the frames before it readable.
- [x] 5.3 Test a cut during recovery: recover a log, cut it again before the recovery finished,
  and recover once more to the same rows.
- [x] 5.4 Extend `crates/server/tests/server.rs`: rows written then the server killed and started
  again hold every committed row, which is [FR56] over a real socket.
- [x] 5.5 Test over a socket that a statement refused leaves no row behind, which is [FR69] with
  a log to undo with for the first time.
- [x] 5.6 Note in `docs/testplan.md` that [T10] to [T13] hold in the process-crash and injected
  forms, and that the block-device form is still to come.

## Dependencies

- 1 blocks everything, because the pool, recovery and the checkpoint all read frames.
- 2 needs 1. 3 needs 2. 4 needs 3. 5 needs 4.
- 2.6 is churn across the store tests and can land with 2.1.
- 5.6 is a document edit and can land at any point in parent 5.

## Done when

- Rows written and committed are there after the server is killed and started again.
- A statement that failed leaves no row, and a frame with no commit after it leaves none either.
- A half-written frame or a bad checksum stops recovery there and loses nothing committed before
  it.
- A log past 64 MB is checkpointed into its table files and emptied, and the rows do not change
  across it.
- Recovery finishes before the first connection is accepted.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
