# Milestone 11, query completeness — Tasks

Source: `.specs/milestone-11-queries/design.md`, and `docs/projectplan.md` M11.
Requirements: [FR37]-[FR40], [FR66]. Types: `docs/internals.md` sections 3 and 5.

Crate `crates/server`, new files `src/exec/sort.rs` and `src/store/extsort.rs`, plus changes to
`src/store/btree.rs`, `src/exec/`, `src/net/`, `src/catalog/registry.rs` and `src/config.rs`, and
one code in `crates/protocol`. The milestone ends with `ORDER BY` on a result larger than memory,
and a statement a client can stop.

Settled before this list. Do not reopen. Each is a decision of the design, which holds the
alternative it was taken over.
- A descending order on the primary key walks the leaves backwards. The leaves are doubly linked
  already.
- `CANCELLED` joins the error codes. `TXN_ABORTED` tells a client to try again, which is the
  opposite of what a cancel asked for.
- The budget counts encoded bytes, not the heap a `Vec<Row>` takes.
- `ExternalSort::new` takes the fan-in as well as the budget, so a test can force several merge
  passes without thousands of rows.
- The budget is a setting with a 10 MB default, because a test cannot otherwise reach a spill
  without writing the whole budget a build is allowed.
- A null sorts after every value ascending, and before them descending.
- `Sort` drains its whole input even under a `LIMIT`. Top-N with a heap is an optimisation
  nothing asked for.
- The cancel flag reaches the operators as a parameter of the plan, not as a context type.
- The 100 GB of the done-when is milestone 12's, as every figure in section 3.1 has been since
  milestone 6.

## 1. Order out of the tree [serial]

- [x] 1.1 Add `Direction` and `BTree::rightmost`, the mirror of `leftmost`. `src/store/btree.rs`.
  The rightmost leaf of a tree of one page is that page.
- [x] 1.2 Give `Cursor` its direction: a descending walk starts at the last slot, steps down, and
  follows `prev` at the end of a leaf. `src/store/btree.rs`. A descending walk of every key gives
  the keys of an ascending walk, reversed.
- [x] 1.3 Make the bound that `Cursor::beyond` checks the one it is walking towards, so a
  descending walk stops at the lower bound. `src/store/btree.rs`. A range reads the same rows
  either way round.
- [x] 1.4 Carry the direction through `BTree::cursor` and `Scan::new`. `src/store/btree.rs`,
  `src/exec/scan.rs`. Every existing caller asks for `Ascending` and reads what it read before.
- [x] 1.5 Test the walk both ways: every key, a range, one page, many pages, an empty tree, and a
  descending walk of a range that starts inside a leaf.

## 2. Runs on disk [serial, needs 1]

- [x] 2.1 Add `sort` to `Limits` and `checkpoint_bytes`'s neighbour `--sort-bytes` to the flags,
  default 10 MB. `src/catalog/registry.rs`, `src/config.rs`. The flag sets it and a bad value is
  refused.
- [x] 2.2 Add `src/store/extsort.rs` with `RunFile::write`, which sorts rows and writes them to
  `tmp/` as a length and a record each, and `Drop`, which removes the file. Declare it in
  `src/store/mod.rs`. A run that is dropped leaves no file.
- [x] 2.3 Add `RunFile::head` and `advance`, reading through a 64 KB buffer. Rows come back in the
  order they were written, and the end gives none.
- [x] 2.4 Add `ExternalSort::new` and `add`, which holds rows until they pass the budget and then
  spills a run. Nothing is written while the rows fit.
- [x] 2.5 Add `finish` and the merge: the held rows become the last run, then runs merge `fan_in`
  at a time until one is left. Rows come out in order, and a merge of one run reads it straight.
- [x] 2.6 Clear `<database>/tmp/` in `Registry::sweep`. `src/catalog/registry.rs`. A run file left
  by a crash is gone after a start, and the directory is made when a sort needs it.
- [x] 2.7 Test the sort against a temp directory: rows that fit, rows that spill one run, rows
  that spill several, a merge over more runs than the fan-in, every type as a key, and the files
  gone when the sort drops.

## 3. The sort operator [serial, needs 2]

- [x] 3.1 Add `src/exec/sort.rs` with `Sort`, which drains its input on the first `next()` and
  hands back rows in order. Declare it in `src/exec/mod.rs`. Gives [FR37].
- [x] 3.2 Order the values with `Value::compare`, a null after every value ascending and before
  them descending. `src/exec/sort.rs`. Gives [FR38] and [FR39] their reach into a sort.
- [x] 3.3 Resolve the `ORDER BY` column in `planner::plan` and answer `UNKNOWN_COLUMN` for a
  column the table has not got. `src/exec/planner.rs`.
- [x] 3.4 Put the operators in order in `planner::plan`: scan, condition, sort, count, columns.
  `src/exec/planner.rs`. A `LIMIT` with an `ORDER BY` returns the first rows of the order, and a
  sort on a column the selection leaves out works.
- [x] 3.5 Leave the sort out when the scan already gives that order, and take the direction from
  the `ORDER BY`. `src/exec/planner.rs`. An order on the primary key builds no sort.
- [x] 3.6 Drop the refusal that milestone 8 left in `exec::dml::dispatch`, and pass the sort
  directory and the budget down. `src/exec/dml.rs`. `ORDER BY` runs.

## 4. A statement a client can stop [serial, needs 3]

- [x] 4.1 Add `CANCELLED` to `ErrorCode`, its `as_str`, its `FromStr` and the list in
  `docs/internals.md`. `crates/protocol/src/error.rs`. It round-trips through the wire form.
- [x] 4.2 Add `CancelHandle::clear`, and clear it before each statement. `src/net/cancel.rs`,
  `src/net/session.rs`. A connection runs a statement after one was cancelled.
- [x] 4.3 Pass the handle through `dml::run` and `planner::plan` to the operators.
  `src/exec/dml.rs`, `src/exec/planner.rs`, `src/net/session.rs`.
- [x] 4.4 Read the flag between rows in `Scan`, and in the fill and the merge of `Sort`, and
  answer `CANCELLED`. `src/exec/scan.rs`, `src/exec/sort.rs`. Gives [FR66] its reach into a
  statement.
- [x] 4.5 Test cancel: a scan that stops partway, a sort that stops while it fills, a sort that
  stops while it merges, the run files gone afterwards, and the next statement on that connection
  running.

## 5. The suites [serial, needs 4]

- [x] 5.1 Extend `crates/server/tests/cases/queries.test`: an order ascending and descending, on
  the primary key and on another column, on text, on a decimal where `12.20` and `12.2` are one
  value, with nulls, and with a `LIMIT`.
- [x] 5.2 Test over a socket in `crates/server/tests/server.rs` that an order on a column that is
  not the key returns every row in order with a budget small enough to spill.
- [x] 5.3 Test over a socket that a `Cancel` on a second connection stops a slow statement,
  answers `CANCELLED`, and leaves the connection able to run the next one.
- [x] 5.4 Test that `tmp/` holds nothing once a statement has ended, and nothing after a start.

## Dependencies

- 1 blocks everything, because the planner asks the scan for an order before it builds a sort.
- 2 needs 1. 3 needs 2. 4 needs 3. 5 needs 4.
- 4.1 and 4.2 are small and can land with 3.

## Done when

- `ORDER BY` returns every row in order, ascending or descending, on any column and any type.
- A result larger than the sort budget is ordered by spilling to `tmp/` and merging, inside the
  memory limit.
- An order on the primary key builds no sort and reads the tree in the direction asked for.
- `12.20` and `12.2` sort as one value, and text sorts by its UTF-8 bytes.
- A `LIMIT` with an `ORDER BY` gives the first rows of the order.
- A cancel stops a statement, answers `CANCELLED`, frees the run files, and leaves the connection
  open.
- `tmp/` is empty when no sort is running, and after a start.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
