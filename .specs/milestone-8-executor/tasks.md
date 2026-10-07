# Milestone 8, the executor — Tasks

Source: `.specs/milestone-8-executor/design.md`, and `docs/projectplan.md` M8.
Requirements: [FR22]-[FR30], [FR36], [FR40]-[FR43]. Types: `docs/internals.md` section 3.

Crate `crates/server`, module `src/exec/`, plus `src/store/row.rs`. The milestone ends with
`INSERT` and `SELECT` working through the CLI. No transaction and no WAL yet.

Settled before this list. Do not reopen. Each one is a decision of the design, which holds the
alternative it was taken over.
- A write re-seeks after each row it changes, remembering the last key handled. Gathering every
  matching key first would break the memory limit at a billion rows.
- Every row of a statement is checked before any row is written, so a rejected statement has
  changed nothing. An I/O failure partway is the hole the WAL closes in milestone 9.
- Operators carry `Value`s and not bytes, so a chain is read once on the way in.
- A value over the inline limit moves to a chain. A row that still does not fit a page is
  refused rather than squeezed further.
- `Registry` owns the pool, because a session already holds the registry and the pool opens its
  files under the registry's data directory.
- `PkScan` bounds are a hint, and `Filter` applies the whole condition anyway, so a bound the
  planner misses costs a scan and never a wrong answer.
- A condition is evaluated by a function in `exec/eval.rs`, not a method on `Expr`. The AST holds
  no behaviour, and evaluation needs the columns to resolve a name to a position.
- `BEGIN`, `COMMIT` and `ROLLBACK` answer `TXN_ABORTED` with a message naming milestone 10.
  Refusing `BEGIN` up front means no client believes it holds a transaction it does not have.
- `ORDER BY` stays unanswered. `Sort` and the external sort are milestone 11, so a `SELECT` that
  names one is refused rather than answered in the wrong order.
- The 10 million row `SELECT` of the done-when belongs to milestone 12, as milestone 6 settled
  for every figure in section 3.1.

## 1. A row between values and bytes [serial]

- [x] 1.1 Add `src/store/row.rs` with `store`, which writes a chain for each value over
  `codec::INLINE_LIMIT`, encodes the row, and refuses a row still larger than `btree::MAX_RECORD`.
  Declare it in `src/store/mod.rs`.
- [x] 1.2 Add `load`, which decodes a row and reads each chain back into its `Value`.
- [x] 1.3 Add `free`, which frees the chain of every value of a row that is going. Gives [FR11]
  its reach into a row.
- [x] 1.4 Test the round trip against a temp directory: every type, a null, a 1 MB value through
  a chain, a row of a hundred columns, and a row too wide for a page refused.
- [x] 1.5 Test that `free` returns the pages of a row's chains, and that a row holding no chain
  frees nothing.

## 2. The pool reaches a table [serial, needs 1]

- [x] 2.1 Give `Registry` a `BufferPool` built from the memory limit, and a `pool` accessor.
  `Registry::new` takes the limit, and `Server::bind` passes `config.mem_limit`.
- [x] 2.2 Add `Registry::table_file`, which opens `<database>/<table id>.tbl` in the pool and
  returns its `FileId`. Opening the same table twice returns the same id.
- [x] 2.3 Test that two calls for one table give one `FileId`, that two tables give two, and
  that the file lands in the directory of its own database.

## 3. Operators [serial, needs 2]

- [x] 3.1 Add `src/exec/operator.rs` with the `Operator` trait, `next` returning
  `Result<Option<Row>, DbError>` and `schema` returning the columns. Declare `mod exec` in
  `main.rs`.
- [x] 3.2 Add `src/exec/eval.rs`: a condition against one row, answering none when a comparison
  meets a null. Covers every form the parser builds. Gives [FR33] to [FR36].
- [x] 3.3 Add `SeqScan` and `PkScan` to `src/exec/scan.rs`, over a B-tree cursor, the first
  unbounded and the second over a range of keys.
- [x] 3.4 Add `Filter`, `Project` and `Limit` to `src/exec/filter.rs`. `Filter` drops a row whose
  condition is not true, `Project` keeps the named columns in the order named, and `Limit` stops
  after its count. Gives [FR31], [FR32] and [FR40].
- [x] 3.5 Add `plan` to `src/exec/planner.rs`, which builds the tree and takes `PkScan` bounds
  from the comparisons on the primary key at the top of a `WHERE`.
- [x] 3.6 Test the operators: `eval` for each form and for a null, a filter that drops rows, a
  projection that reorders columns, a limit that stops, and a plan that picks `PkScan` for a key
  comparison and `SeqScan` otherwise.

## 4. The statements [serial, needs 3]

- [x] 4.1 Add `src/exec/dml.rs` with `insert`: check every row first for count, type, null in a
  `NOT NULL` column, a key the tree holds, and a key repeated in the statement, then store them
  all. Gives [FR22], [FR23], [FR28], [FR29], [FR30].
- [x] 4.2 Add `select`, which plans the statement and hands back the root operator, and refuses
  an `ORDER BY` with a message naming milestone 11.
- [x] 4.3 Add `update`: walk the matching keys, re-seeking after each, check the new row, and
  write it. A changed primary key is a delete and an insert. Gives [FR25] and [FR27].
- [x] 4.4 Add `delete`: walk the matching keys, re-seeking after each, and free the chains of
  every row that goes. Gives [FR26] and [FR27].
- [x] 4.5 Add `exec::run`, one entry the session calls, which sends a schema change to
  `catalog::ddl` and everything else here, and answers `TXN_ABORTED` for the three transaction
  statements.
- [x] 4.6 Test the statements against a temp directory: rows go in and come back, a change reaches
  every matching row, a delete frees its chains, a bad row leaves the table untouched, and each
  constraint answers its own code.

## 5. The session streams rows [serial, needs 4]

- [x] 5.1 Change `Session::run_statement` in `src/net/session.rs` to write to the socket rather
  than return one message: `RowDesc` from the schema, a `DataRow` for each row, then `Complete`.
  A write that fails ends the session as it does today.
- [x] 5.2 Keep a schema change answering `Complete` with its kind, and an error answering `Error`,
  so nothing about the existing suites changes.
- [x] 5.3 Update `crates/server/tests/cases/syntax.test`: the statements that answered
  `UNKNOWN_TABLE` for want of an executor now run or name a real failure.
- [x] 5.4 Add `crates/server/tests/cases/rows.test` for [S4]: insert one row and many, read them
  back, change rows, delete rows, and each of the three constraints refused.
- [x] 5.5 Add `crates/server/tests/cases/queries.test` for [S5]: chosen columns, every
  comparison, `AND`, `OR`, `NOT`, `IS NULL`, a comparison with a null returning no row, and
  `LIMIT`.
- [x] 5.6 Test over a real socket in `crates/server/tests/server.rs` that a `SELECT` of many rows
  arrives as `RowDesc`, then `DataRow`s, then `Complete`, and that a row survives a restart.

## Dependencies

- 1 blocks everything, because every statement stores or loads a row through it.
- 2 needs 1. 3 needs 2. 4 needs 3. 5 needs 4.
- 5.3 can land with 5.1, because the same change makes those cases wrong.

## Done when

- `INSERT` then `SELECT` through the CLI returns the rows, with their column names.
- A change or a delete reaches every row that matches its condition.
- A duplicate key, a null in a `NOT NULL` column, and a value of the wrong type are each
  refused with their own code, and the table is left as it was.
- A comparison with a null keeps the row out of the result.
- Case files for [S4] and [S5] pass, and the rows survive a restart.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
