# Milestone 8, the executor — Design

Sources: `docs/projectplan.md` M8, the query execution section of `docs/design.md`,
`docs/internals.md` section 3, and [FR22]-[FR30], [FR36], [FR40]-[FR43]. Built on the catalog
from milestone 5 and the store and B-tree from milestones 6 and 7.

## Overview

- SQL in, rows out, on disk. The first milestone where a statement reaches a row.
- An iterator over the B-tree: each operator hands back one row from `next()`, and the session
  writes each row to the socket as it arrives, so nothing holds a whole result.

## Components

- `Operator`: the trait, `next` and `schema`. `crates/server/src/exec/operator.rs`.
- `SeqScan` and `PkScan`: rows out of a B-tree cursor, over every key or over a range.
  `exec/scan.rs`.
- `Filter`, `Project`, `Limit`: one row at a time from the operator below. `exec/filter.rs`.
- `plan`: a `Statement` and a `TableDef` into a tree of operators. `exec/planner.rs`.
- `eval`: a condition against one row. `exec/eval.rs`.
- `dml`: runs `INSERT`, `UPDATE` and `DELETE`, and opens a `SELECT`. `exec/dml.rs`.
- `row`: one row between `Value`s and bytes, moving a large value to a chain and reading it back.
  `store/row.rs`.
- `Registry` gains the `BufferPool`. The pool is one for the whole server, and the registry
  already owns the data directory that every table file sits under.

## Data Model

- Nothing new on disk. The rows of a table are its B-tree, in `<database>/<table id>.tbl`, which
  the pool opens by path and keys by a `FileId` it hands out.
- A `Row` is a `Vec<Value>` in the order the columns of the table hold. Operators pass that and
  not bytes, so a chain is read once on the way in.

## Interfaces

- `Operator::next(&mut self) -> Result<Option<Row>, DbError>`.
- `Operator::schema(&self) -> &[ColumnDef]`, which gives `RowDesc` its names. [FR43]
- `plan(statement, table, pool, file) -> Result<Box<dyn Operator + '_>, DbError>`.
- `eval(condition, columns, row) -> Result<Option<bool>, DbError>`. None is a condition that is
  neither true nor false, which a null gives. [FR36]
- `row::store(pool, file, columns, values) -> Result<Vec<u8>, DbError>`: writes a chain for each
  value over the inline limit, then encodes.
- `row::load(pool, file, columns, bytes) -> Result<Row, DbError>`: reads those chains back.
- `row::free(pool, file, columns, bytes) -> Result<(), DbError>`: frees the chains of a row that
  is going.
- `dml::run(statement, db, pool) -> Result<Answer, DbError>`, where `Answer` is a row count for a
  write or an operator for a `SELECT`.

## Flow

`SELECT`:

- 1. The session finds the table in the catalog and opens its file in the pool.
- 2. `plan` builds `Project` over `Limit` over `Filter` over a scan, picking `PkScan` when the
  `WHERE` bounds the primary key and `SeqScan` otherwise.
- 3. The session writes `RowDesc` from the schema of the root operator.
- 4. It calls `next` until none comes back, writing one `DataRow` each time, then `Complete`.

`INSERT`:

- 1. Every row is checked first: one value for each column, each of the column's type, no null in
  a `NOT NULL` column, no key the tree already holds, and no key repeated inside the statement.
- 2. Only then is each row stored, its large values going to chains before it is encoded.

`UPDATE` and `DELETE`:

- 1. Walk the matching keys one at a time, remembering the last key handled.
- 2. Apply the change, then descend again for the first key above that one.
- 3. A change that moves the primary key is a delete and an insert, so a key already taken
  answers `DUPLICATE_KEY`. A row that leaves takes its chains with it.

## Key Decisions

- D1. A write re-seeks after each row it changes. Alternative: gather every matching key, then
  change them. Why: [NFR2] allows a billion rows, and a key list that size breaks the memory
  limit that [NFR10] sets.
- D2. Every row of a statement is checked before any row is written. Alternative: write each row
  as it validates. Why: [FR69] says a failed statement changes nothing, and there is no WAL to
  undo with until milestone 9. The hole left is an I/O failure partway through the writes, which
  only the WAL closes.
- D3. Operators carry `Value`s and not encoded bytes. Alternative: pass bytes and decode at the
  top. Why: `Filter` has to compare values anyway, and reading a chain belongs with the decode.
- D4. A value over the inline limit moves to a chain, and a row that still does not fit a page is
  refused. Alternative: push more values out until it fits. Why: 2 KB is the documented
  threshold, and only a row of a hundred nearly-full columns fails this way.
- D5. `Registry` owns the pool. Alternative: `Server` holds it beside the registry. Why: a
  session already holds the registry, and the data directory the pool opens files under is the
  registry's.
- D6. `PkScan` takes its bounds from the comparisons on the primary key at the top of a `WHERE`,
  and `Filter` still applies the whole condition. Alternative: a planner that rewrites the
  condition and drops what the bound already covers. Why: the bound is a hint, so a missed one
  costs a scan and never a wrong answer.
- D7. A `SELECT` answers through a boxed operator that borrows the pool. Alternative: collect the
  rows and hand back a list. Why: [FR42] wants a result larger than the memory of the server.
- D8. `Operator::next` returns a `Result`, not just an `Option`. Alternative: treat a read
  failure as the end of the rows. Why: a short read would otherwise look like an empty table.
- D9. `Filter` drops a row whose condition is neither true nor false. Alternative: read a null
  comparison as false. Why: [FR36] says the row must not appear, and `Value::compare` already
  answers none.
- D10. A condition is evaluated by a function in `exec/eval.rs`, not a method on `Expr`.
  Alternative: `Expr::eval`, which `internals.md` shows. Why: the AST holds no behaviour, which
  milestone 7 settled, and evaluation needs the column list to resolve a name to a position.

## Risks

- A statement that sets the primary key to a literal can move a row ahead of the walk, which
  meets it again and answers `DUPLICATE_KEY`. Nothing in scope sets a key from another column, so
  at most one row moves sensibly.
- [FR27] wants a change to reach every matching row. The re-seek walk leans on keys being unique
  and ordered, which the primary key gives it.
- The 10 million row `SELECT` of the done-when belongs to milestone 12, as milestone 6 settled
  for every figure in section 3.1 of `requirements.md`.
- `internals.md` section 3 shows a `Sort` operator. That is milestone 11, with the external sort
  it needs, and `ORDER BY` stays unanswered until then.
