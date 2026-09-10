<p align="center">
<pre>
                        /\
                       /  \
                      / .. \
                     / .... \
                    /  ....  \
                   /          \        /\
                  /            \      /  \
                 /              \____/    \
                /                          \
               /____________________________\
</pre>
</p>

<h1 align="center">GaribalDB</h1>

<p align="center">
  <em>A relational database, written from nothing.</em>
</p>

---

A single-node SQL database server. Clients connect over TCP and send SQL. Tables are B-trees in
paged files. One writer for each database gives serializable transactions, and readers see a
snapshot, so they never wait.

No ORM, no embedded engine, no SQL library. The parser, the B-tree, the buffer pool, the
write-ahead log, and the wire protocol are all written here.

Written in Rust. Name comes from Mount Garibaldi, a stratovolcano in the Coast Mountains of British Columbia.

## Scope

`CREATE`/`DROP` for a database and a table. `INSERT`, `SELECT`, `UPDATE`, `DELETE`.
`BEGIN`, `COMMIT`, `ROLLBACK`. Types `INTEGER`, `TEXT`, `BOOLEAN`, and `DECIMAL(p, s)`.
Serializable transactions that survive a power failure. 100 GB in one table, inside a 1 GB
memory limit. 100 clients at the same time.

No `JOIN`, no indexes, no aggregates, no replication, no authentication.

## Status

Milestone 1 of 12 done. The protocol crate is complete and tested.
Milestone 8 is the first one that looks like a database.
See [docs/projectplan.md](docs/projectplan.md).

## Build

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Needs Rust 1.85 or newer.

## Layout

```
crates/protocol/   messages, values, error codes. No engine, no I/O.
crates/server/     the engine and the garibald binary
crates/cli/        the garibal binary
docs/              requirements, design, internals, test plan, project plan
```

## Documents

- [requirements.md](docs/requirements.md) — what the system must do
- [design.md](docs/design.md) — how it is built, and why
- [internals.md](docs/internals.md) — every type, as class diagrams
- [testplan.md](docs/testplan.md) — how the project proves ACID holds
- [projectplan.md](docs/projectplan.md) — the 12 milestones

## License

MIT. See [LICENSE](LICENSE).
