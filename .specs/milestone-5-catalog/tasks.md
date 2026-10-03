# Milestone 5, Catalog and DDL — Tasks

Source: `docs/projectplan.md` M5. Types: `docs/internals.md` sections 1 and 2.
Requirements: [FR1]-[FR13]. Layout: the on-disk section and decision D8 of `docs/design.md`.

Crates `crates/protocol` and `crates/server`. The milestone ends with databases and tables that
survive a restart, and no row anywhere. Rows arrive in milestone 8.

Settled before this list. Do not reopen.
- Four codes join `ErrorCode`: `DATABASE_EXISTS`, `TABLE_EXISTS`, `DATABASE_IN_USE`, and
  `UNKNOWN_DATABASE`. [FR70] names a minimum, not a maximum, and milestone 2 already added
  `TOO_MANY_CONNECTIONS` on the same ground. [FR68] wants a client able to tell the cases apart
  without reading English.
- Only the four DDL statements run. `INSERT`, `SELECT`, `UPDATE`, and `DELETE` keep answering
  `UNKNOWN_TABLE` until milestone 8, even for a table that exists. The project plan puts the
  executor in milestone 8, and a statement with no storage behind it has no honest answer.
- `Database` holds its name and a `Mutex<Catalog>`. The write lock, the WAL, and the buffer pool
  join it in milestones 10, 9, and 6. `Registry` maps a name to an `Arc<Database>`.
- DDL takes no transaction, because [FR55] keeps schema changes out of one. So a `Mutex` is
  enough, and no WAL record is written.
- A table id comes from `next_table_id` in `catalog.json` and is never reused. A reused id would
  let a stale `.tbl` file pass for a live table.
- A database is a directory, and there is no server-level catalog. The databases that exist are
  the directories under the data directory.
- An identifier keeps its case, which milestone 4 settled. `Item` and `item` are two tables.
- `DROP TABLE` removes the catalog entry and the `.tbl` file. No `.tbl` exists until milestone 6,
  so the delete must not fail on a file that is not there.
- A crash injected between a write and a rename waits for the [H3] harness in milestone 9. This
  milestone proves the property it can: a leftover temp file is not a catalog, and a successful
  save leaves none behind.

## 1. The codes the lifecycle needs [serial]

- [x] 1.1 Add `DatabaseExists`, `TableExists`, `DatabaseInUse`, and `UnknownDatabase` to
  `ErrorCode` in `crates/protocol/src/error.rs`, each with its screaming-snake name in `as_str`.
  The round-trip test covers the four new names. Gives [FR68].
- [x] 1.2 Add `FromStr` to `ErrorCode`, reading the wire name that `as_str` writes. A test
  round-trips every code through text.
- [x] 1.3 Replace the hand-written list of codes in `crates/server/tests/h1/mod.rs` with that
  `FromStr`, so a later code reaches a case file without a second edit.
- [x] 1.4 Add the four failures to the [FR70] list in `docs/requirements.md`, so the document
  stays the authority.

## 2. The catalog [serial, needs 1]

- [x] 2.1 Add `ColumnDef` and `TableDef` to `crates/server/src/catalog/mod.rs`, and declare
  `mod catalog` in `main.rs`. `ColumnDef` holds a name, a `DataType`, and `not_null`. `TableDef`
  holds an id, a name, its columns, and `pk_index`. Both carry serde. Gives [FR7] to [FR9].
- [x] 2.2 Add `Catalog`, holding its path, its tables, and `next_table_id`. `Catalog::load`
  reads `catalog.json`, and a directory with no file yet reads as an empty catalog.
- [x] 2.3 Add `Catalog::save_atomic`. It writes a temp file, `fsync`s it, renames it over
  `catalog.json`, then `fsync`s the directory. Gives decision D8 its crash safety.
- [x] 2.4 Add `add_table`, which refuses a name the catalog already holds with `TABLE_EXISTS`
  and takes its id from `next_table_id`. Gives [FR6] and [FR12].
- [x] 2.5 Add `remove_table` and `table`, each answering `UNKNOWN_TABLE` for a name the catalog
  does not hold. Gives [FR10] and [FR13].
- [x] 2.6 Test the catalog against a temp directory: a save and a load round-trip every field, a
  second table takes the next id, a dropped id is never handed out again, a successful save
  leaves no temp file, and a leftover temp file changes nothing that `load` reads.

## 3. The registry and the data directory [serial, needs 2]

- [x] 3.1 Add `Database` to `crates/server/src/catalog/mod.rs`, holding a name and a
  `Mutex<Catalog>`.
- [x] 3.2 Add `Registry` to `crates/server/src/catalog/registry.rs`, holding the data directory
  and the databases it has opened. `open` returns an `Arc<Database>`, reading the catalog once
  and sharing it after that. An unknown name answers `UNKNOWN_DATABASE`.
- [x] 3.3 Add `create` and `drop`. `create` makes the directory and an empty catalog, and
  refuses a name that exists with `DATABASE_EXISTS`. `drop` removes the directory and everything
  under it. Gives [FR1], [FR2], [FR3], and [FR11].
- [x] 3.4 Count the connections of each database in `Registry`, and refuse a `drop` while one
  stands, with `DATABASE_IN_USE`. The count falls when the session ends, the way the connection
  slot does. Gives [FR4].
- [x] 3.5 Add the startup sweep. For each database directory, delete every `.tbl` file that its
  catalog does not name, and log what it deleted.
- [x] 3.6 Test the registry against a temp directory: a create then an open, a create that
  repeats a name, an open of a name that is not there, a drop that frees the name, a drop
  refused while a connection stands, and a sweep that deletes an orphan and keeps a named file.

## 4. The session speaks DDL [serial, needs 3]

- [x] 4.1 Give `Server` in `src/net/listener.rs` a `Registry` built from the config, run the
  sweep from 3.5 before it accepts, and hand the registry to each session.
- [x] 4.2 Open the database that `Startup` names in `src/net/session.rs`. A name that is not
  there answers `Error(UNKNOWN_DATABASE)` and closes, the way a bad protocol version does.
  Gives [FR5].
- [x] 4.3 Run `CREATE DATABASE` and `DROP DATABASE` through the registry. A client may create or
  drop a database other than its own, and dropping its own is refused while it is connected.
- [x] 4.4 Run `CREATE TABLE` and `DROP TABLE` through the catalog of the session's database,
  saving the catalog on each change. A `CREATE TABLE` with no primary key, or with two, is
  refused. Gives [FR6] to [FR13].
- [x] 4.5 Leave `INSERT`, `SELECT`, `UPDATE`, and `DELETE` answering `UNKNOWN_TABLE`, with a
  comment naming milestone 8.
- [x] 4.6 Test the session: each DDL statement answers no error, each refusal answers its own
  code, and a `Ready` follows every statement.

## 5. Case files and a restart [serial, needs 4]

- [x] 5.1 Add `crates/server/tests/cases/database.test` for [S1]: create, create again, open,
  drop, drop again. Every case passes.
- [x] 5.2 Add `crates/server/tests/cases/table.test` for [S2]: create with every column type and
  both constraints, create again, drop, drop again, and a statement against a table that is not
  there. Every case passes.
- [x] 5.3 Give `h1::Server` a way to start on a data directory the test names, so a test can
  stop a server and start another on the same files.
- [x] 5.4 Test the restart in `crates/server/tests/server.rs`: create a table, stop the server,
  start it on the same directory, and `DROP TABLE` finds the table. This is the done-when of the
  milestone.
- [ ] 5.5 Test that a `.tbl` file the catalog does not name is gone after a restart, and that a
  file it does name survives.

## Dependencies

- 1 blocks everything, because every later task names one of the new codes.
- 2 needs 1. 3 needs 2. 4 needs 3. 5 needs 4.
- 1.4 is a document edit and can land at any point in parent 1.

## Done when

- `CREATE TABLE` survives a stop and a start, and `DROP TABLE` then finds the table.
- A create that repeats a name, a drop of a name that is not there, and a connect to a database
  that is not there each answer their own code.
- `DROP DATABASE` is refused while a client is connected to it.
- A `.tbl` file that no catalog names is gone after a restart.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
