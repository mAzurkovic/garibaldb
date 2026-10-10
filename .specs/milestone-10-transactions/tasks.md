# Milestone 10, transactions — Tasks

Source: `.specs/milestone-10-transactions/design.md`, and `docs/projectplan.md` M10.
Requirements: [FR44]-[FR55]. Types: `docs/internals.md` section 4.

Crate `crates/server`, new module `src/txn/`, plus changes to `src/wal/`, `src/store/`,
`src/exec/dml.rs` and `src/net/session.rs`, and two codes in `crates/protocol`. The milestone
ends with the six anomalies of `docs/testplan.md` section 4.2 absent.

Settled before this list. Do not reopen. Each is a decision of the design, which holds the
alternative it was taken over.
- A snapshot read goes around the pool when the pool's copy is too new. The pool's copy of a page
  is always the newest one, so a reader asks the index whether that page changed since its mark.
- A reader's mark is the offset past the last commit frame, not `wal.end()`. An uncommitted
  eviction raises `end`.
- A statement with no transaction open is its own transaction. The case files of [S1] to [S5] and
  the CLI send single statements.
- `WriteLock` is a `Mutex<bool>` and a `Condvar`, and `WriteGuard` holds an `Arc<Database>`. A
  `MutexGuard` cannot be stored across statements, and the standard mutex has no timed lock.
- The checkpoint stays at the end of a statement and skips while a reader's mark is behind the
  log. No background thread in this milestone.
- `READ_ONLY_TXN` and `TXN_ALREADY_OPEN` join the error codes. `COMMIT` or `ROLLBACK` with nothing
  open answers `SYNTAX_ERROR`, because nothing was aborted and there is nothing to retry.
- [FR50] is held statement by statement. `COMMIT` re-checks no row.
- A rollback drops the database's pages out of the pool, and leaks the pages its transaction grew
  the file by.
- [FR52] needs no code. A connection sees exactly one database.
- [H2] is a Rust runner with `Send` and `Expect` steps, not another case-file format.
- [T6] replays the transactions in commit order, because one writer at a time means the commit
  order is the serial order.

## 1. A read at a mark [serial]

- [x] 1.1 Add `commit_end` to `Wal` with `committed()`, set from recovery at open and from the
  sync in `commit`. `src/wal/writer.rs`. A log whose tail never committed reports the offset of
  the last commit frame.
- [x] 1.2 Add `FrameIndex::newer_than(table, page_no, mark)` and `drop_from(mark)`.
  `src/wal/index.rs`. `newer_than` is true when the newest frame of a page starts at or past the
  mark.
- [x] 1.3 Add `Wal::truncate_to(mark)`, which cuts the file, drops the index entries at or past
  the mark, and leaves `commit_end` alone. `src/wal/writer.rs`. A log cut back to a mark reads
  back the pages that committed before it.
- [x] 1.4 Add `Mark` and `BufferPool::fetch_at(id, mark)` returning a `PageRef` that is either a
  pinned pool page or an owned copy. `src/store/pool.rs`. A dirty frame, or a page the index says
  changed since the mark, comes back as an owned copy.
- [x] 1.5 Add `BufferPool::discard(wal)`, which drops every page of that log's files from the
  pool. `src/store/pool.rs`. A page changed and then discarded reads back as it was.
- [x] 1.6 Give `BTree` the `Mark` its reads use, from `BTree::open`, and route every read of the
  tree through `fetch_at`. `src/store/btree.rs`. Every existing B-tree test passes unchanged in
  meaning with `Mark::Latest`.
- [x] 1.7 Test a read at a mark: a page changed after the mark reads as it was at the mark, a
  page an open writer dirtied is not visible, a cut log drops the uncommitted frames, and a scan
  at a mark returns the rows of that moment.

## 2. The lock and the transaction [serial, needs 1]

- [x] 2.1 Add `READ_ONLY_TXN` and `TXN_ALREADY_OPEN` to `ErrorCode`, its `as_str`, its `FromStr`,
  and the list in `docs/internals.md`. `crates/protocol/src/error.rs`. Both round-trip through the
  wire form.
- [x] 2.2 Add `--lock-timeout-ms` to the flags. `src/config.rs`. The flag sets
  `lock_timeout_ms`, and a bad value is refused like the others.
- [x] 2.3 Add `src/txn/lock.rs` with `WriteLock::acquire(timeout)`, `release()`, and `WriteGuard`
  holding an `Arc<Database>`. Declare `mod txn` in `main.rs`. A second acquire waits, and one
  that waits past the timeout answers `LOCK_TIMEOUT`.
- [x] 2.4 Add `src/txn/manager.rs` with `TxnKind`, `Readers`, `Lease`, and `Readers::oldest()`.
  A lease taken and dropped leaves no mark behind.
- [x] 2.5 Add `Transaction` with `commit`, `rollback`, `check_writable` and `mark`, and
  `Database::begin(kind)`. `src/txn/manager.rs`, `src/catalog/mod.rs`. `begin(ReadWrite)` takes
  the lock, `begin(ReadOnly)` takes a lease and no lock.
- [x] 2.6 Give `Database` its `WriteLock` and `Readers`, built in `Registry`.
  `src/catalog/mod.rs`, `src/catalog/registry.rs`. Two databases hold two locks.
- [x] 2.7 Test the lock and the transaction: a writer blocks a second writer, a reader blocks
  neither, a timeout answers `LOCK_TIMEOUT`, a rollback cuts the log back, and a commit leaves
  nothing dirty.

## 3. The statements [serial, needs 2]

- [x] 3.1 Hold the open transaction on `Session`, and report `Open`, `ReadOnly` or `None` in
  every `Ready`. `src/net/session.rs`. The state follows the transaction a client opened.
- [x] 3.2 Run `BEGIN`, `COMMIT` and `ROLLBACK` in place of the refusal milestone 8 left.
  `src/exec/dml.rs`, `src/net/session.rs`. A second `BEGIN` answers `TXN_ALREADY_OPEN`, and a
  `COMMIT` with nothing open answers `SYNTAX_ERROR`.
- [x] 3.3 Refuse a change from a read-only transaction with `READ_ONLY_TXN`, and a `CREATE` or
  `DROP` of a database or a table while any transaction is open with `SCHEMA_CHANGE_IN_TXN`.
  `src/exec/dml.rs`. Gives [FR46] and [FR55].
- [x] 3.4 Take the write lock for a changing statement that has no transaction open, commit it,
  and release. `src/exec/dml.rs`. Two connections writing at once no longer overlap.
- [x] 3.5 Give a read its mark and its lease: `Latest` in a read-write transaction,
  the snapshot in a read-only one, and `wal.committed()` with a lease when none is open, carried
  by `Answer::Rows` until the last row has gone out. `src/exec/dml.rs`.
- [x] 3.6 Skip a checkpoint while `Readers::oldest()` is behind the log, and roll back the open
  transaction on disconnect. `src/wal/checkpoint.rs`, `src/exec/dml.rs`, `src/net/session.rs`.
  Gives [FR65] the rollback it has been missing.

## 4. One client [serial, needs 3]

- [x] 4.1 Add `crates/server/tests/cases/txn.test` for [S6]: a transaction committed, one rolled
  back, a read-only one refusing a change, a second `BEGIN`, a `COMMIT` with nothing open, and
  DDL while a transaction is open.
- [x] 4.2 Extend `tests/statements.rs` to run the new case file. Every case passes.
- [x] 4.3 Test over a socket in `tests/server.rs` that a rolled-back transaction leaves no row
  after a restart, and that a committed one keeps every row. Gives [FR48] and [FR49] with the log
  behind them.
- [x] 4.4 Test that `Ready` carries the transaction state through a whole transaction, and that a
  disconnect with one open leaves no row behind.

## 5. Many clients [serial, needs 3]

- [x] 5.1 Add `crates/server/tests/h2/mod.rs`: a runner over several `Conn`s with `Send` and
  `Expect` steps, so a client can leave a reply unread while another runs. A scripted
  interleaving repeats.
- [x] 5.2 Add `crates/server/tests/isolation.rs` for [S7] with one test for each of the six
  anomalies of section 4.2. None of the six occurs.
- [x] 5.3 Test that a writer waiting past the timeout answers `LOCK_TIMEOUT` and that a retry
  then succeeds, with the server started on a short `--lock-timeout-ms`. Gives [T7].
- [x] 5.4 Add the [T6] check: 1000 random transactions from 20 clients, every statement and
  result recorded, replayed in commit order. A read-write transaction's reads match the state
  before its commit, and a read-only transaction's reads match some prefix.
- [x] 5.5 Test that a long read-only transaction keeps its snapshot while a writer commits over
  it, and that the checkpoint waits for it.

## Dependencies

- 1 blocks everything. The mark is what a transaction hands to a read.
- 2 needs 1. 3 needs 2. 4 and 5 both need 3, and can land in either order.
- 2.1 and 2.2 are small and can land with 1.

## Done when

- A transaction commits all of its changes or none of them, and a rollback leaves no row.
- A read-only transaction reads the same rows from `BEGIN` to the end, whatever a writer commits
  over it, and refuses a change.
- A second `BEGIN` on one connection, and DDL with a transaction open, each answer their own code.
- None of the six anomalies of section 4.2 occurs.
- 1000 random transactions from 20 clients have a serial order that gives the same results.
- A writer that waits past `lock_timeout_ms` answers `LOCK_TIMEOUT`, and a retry succeeds.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
