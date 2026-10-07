# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

GaribalDB is a single-node SQL database server, written from scratch in Rust: no ORM, no
embedded engine, no SQL library, no async runtime. The parser, B-tree, buffer pool, WAL, and
wire protocol are all implemented in this repo. Clients talk to the server over TCP with
newline-delimited JSON messages.

Needs Rust 1.85+ (edition 2024).

## Commands

```sh
cargo test --workspace                              # run everything
cargo test --workspace -- test_name                  # run one test by name (substring match)
cargo test -p server test_name                       # run a test in one crate only
cargo test -p protocol                               # run one crate's tests only
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo fmt --all                                      # actually fix formatting
cargo run --bin garibaldb-server -- --port 5432 --data-dir ./data
```

CI (`.github/workflows/ci.yml`) runs `cargo fmt --all -- --check`, then clippy with
`-D warnings`, then `cargo test --workspace`, in that order. All three must pass.

## Repo layout

```
crates/protocol/   the wire contract: messages, Value/DataType/Decimal, error codes.
                   No engine code, no std::fs. Depended on by both server and cli.
crates/server/     the engine and the `garibaldb-server` binary.
crates/cli/        the `garibaldb` client binary.
docs/              requirements, design, internals, test plan, project plan (see below)
.specs/            per-milestone task checklists, one directory per milestone
```

Dependencies go one way only: `server` and `cli` both depend on `protocol`; `protocol` depends
on neither. `cli` cannot reach storage/engine types because it never depends on `server`.

## Where the real docs live — read before designing anything non-trivial

This project is spec-driven. Before implementing a feature, check whether it's already decided
in one of these:

- **`docs/requirements.md`** — numbered functional/non-functional requirements (`[FR..]`, `[NFR..]`).
  Code comments and tests reference these tags directly (e.g. `// [FR83] for this milestone`).
- **`docs/design.md`** — the architecture and the *why* behind it: components, on-disk layout,
  concurrency model, durability/recovery, memory budget, and a numbered "Key Decisions" (D1–D9)
  log with the alternative considered and the reason rejected. Check this before proposing a
  different approach to something already decided here.
- **`docs/internals.md`** — the concrete types for every layer (struct/enum fields, method
  signatures) as class diagrams, plus the intended file layout per module. Treat this as the
  target shape of the code, one level more concrete than design.md.
- **`docs/testplan.md`** — how requirements map to tests: test harnesses (`[H1]`-`[H6]`), test
  suites (`[S1]`-`[S17]`), and rules like "a test MUST use the network protocol, MUST NOT read a
  data file" (`[TR3]`) and "MUST start from an empty server" (`[TR5]`).
- **`docs/projectplan.md`** — the 12 milestones, their scope, dependencies, and "done when"
  criteria. Current status: milestone 6 of 12 done (see README "Status" and the latest commit).
- **`.specs/milestone-N-*/tasks.md`** — the working checklist for a given milestone, generated
  from projectplan.md. Tasks already checked `[x]` are "settled, do not reopen" — notably each
  file has a "Settled before this list" section calling out decisions not to revisit.

When adding a feature, find its milestone in `projectplan.md`, then its task file in `.specs/`,
rather than inventing scope.

## Architecture essentials

- **One writer per database, snapshot reads.** A read-write transaction takes a single
  per-database write lock at `BEGIN` and holds it until commit/rollback (writers never overlap).
  A read-only transaction takes no lock; it records the WAL frame count at `BEGIN` and only ever
  reads frames below that mark, so it never blocks and never blocks on a writer.
- **Redo-only WAL, no undo log.** A changed page is appended to the WAL; the `.tbl` file never
  holds an uncommitted page, so rollback is just `wal.truncate_to(start_frame)` — nothing to
  undo. A page read checks the WAL index first, then the `.tbl` file (this is what makes
  snapshots work).
- **Storage:** B-tree keyed by primary key, 8 KB slotted pages, fixed-size clock-eviction buffer
  pool, overflow chains for values over 2 KB. No secondary indexes — a `WHERE` not on the
  primary key is always a full `SeqScan`. One buffer pool serves the whole server, because the
  memory limit is server-wide; a `PageId` names its file by an id the pool assigns at runtime.
- **Execution:** iterator/Volcano model (`Operator::next()` returns one row at a time); rows
  stream to the socket as they're produced, nothing buffers a full result set. `ORDER BY` on the
  primary key is free (B-tree order); any other `ORDER BY` goes through an external merge sort
  spilling to `tmp/`.
- **Connections:** one OS thread per TCP connection, blocking I/O (no async runtime) — chosen
  because disk reads block the thread anyway and the target is 100 concurrent connections, not
  10k.
- **Protocol:** newline-delimited JSON, one object per message (`message.rs` in `protocol`).
  `Decimal` always serializes as a JSON string, never a number, to avoid losing precision.
  `Value`, `DataType`, and `Decimal` live in `protocol`, not `server`, because both the server
  and the CLI need to handle them without the CLI depending on the engine.
- **Catalog:** `catalog.json` per database, held fully in memory, rewritten whole via
  temp-file + `fsync` + rename on every DDL change (no WAL record for schema changes).

## Testing conventions

- Tests are black-box: they drive the server over TCP (or, once it exists, the CLI) and never
  read a data file directly (`[TR3]`) — this proves the requirement, not the implementation.
- Server integration tests (`crates/server/tests/`) start the server bound to port 0 and read
  back the OS-assigned port, so parallel tests never collide on a fixed port.
- Config parsing and other pure-function unit tests live inline in `#[cfg(test)] mod tests` next
  to the code they test (see `crates/server/src/config.rs`).
