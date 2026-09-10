# Milestone 2, Server — Tasks

Source: `docs/projectplan.md` M2. Types: `docs/internals.md` section 1.
Requirements: [FR61]-[FR66], [FR81]-[FR83], [NFR14], [NFR20], [NFR21].

Crate `crates/server`, binary `garibald`. It knows no SQL. Every `Query` answers
`Error(UNKNOWN_TABLE)` until milestone 4.

Settled before this list. Do not reopen.
- The server holds a `u64` secret and formats it to a `String` at the wire boundary.
- `max_connections` defaults to 100. The memory budget in `design.md` gives 100 sessions 25 MB.
- [FR65] has no transaction to roll back until milestone 10. Build the hook and the test only.
- [FR83] has no recovery until milestone 9. Log startup and shutdown only.

## 1. Crate skeleton and config [serial]

- [x] 1.1 Add `crates/server` to the workspace members. `Cargo.toml` depends on `protocol` and `log`. Binary name `garibald`. `cargo check` passes.
- [x] 1.2 Add `Config` to `src/config.rs`. Fields `data_dir`, `port`, `mem_limit`, `wal_max_bytes`, `lock_timeout_ms`, `max_connections`. `Default` gives port 5432, 1 GB, 8 GB, 5000 ms, 100.
- [x] 1.3 Add `Config::from_args`. Reads `--data-dir`, `--port`, `--max-connections` with `std::env::args`. An unknown flag returns an error. No `clap`.
- [x] 1.4 Add `src/main.rs`. Reads the config, starts the logger, runs the server. A bad flag prints the error and stops with code 2.

## 2. Logging [parallel, needs 1.1]

- [x] 2.1 Add `utc_now` to `src/log.rs`. Turns `SystemTime` into `YYYY-MM-DDTHH:MM:SSZ`. A pure function, so a test can pass it a fixed instant.
- [x] 2.2 Add `Logger` to `src/log.rs`. Implements `log::Log` and writes `<time> <LEVEL> <message>` to stderr, one line each.
- [x] 2.3 Add `log::init`. Sets the logger and the maximum level. Called once from `main`. A second call is an error, not a panic.
- [x] 2.4 Log a line at startup with the port and the data directory, and a line at shutdown. Gives [FR83] for this milestone.

## 3. Cancel registry [parallel, needs 1.1]

- [x] 3.1 Add `CancelHandle` to `src/net/cancel.rs`. Wraps `Arc<AtomicBool>`. `stop` sets it, `stopped` reads it.
- [x] 3.2 Add `CancelRegistry`. A `Mutex<HashMap<u64, (u64, CancelHandle)>>` holding the secret and the handle for each connection id.
- [x] 3.3 Add `register`, `unregister`, and `cancel(conn_id, secret)`. `cancel` returns false for an unknown id or a wrong secret, and sets the flag for a match.
- [x] 3.4 Add `new_secret`. Returns a random `u64` from `RandomState`, which the operating system seeds. No dependency, and no clock.
- [x] 3.5 Test that a wrong secret leaves the flag clear, a right secret sets it, and an unregistered id returns false.

## 4. Listener and sessions [serial, needs 2 and 3]

- [x] 4.1 Add `Server` to `src/net/listener.rs`. Binds a `TcpListener`, accepts, and starts one `std::thread` for each connection. Gives [FR61] and [FR63].
- [x] 4.2 Count live connections with an `AtomicUsize`. Past `max_connections`, write an `Error(STORAGE_FULL)` line, close, and log. Gives [NFR14].
- [x] 4.3 Add `Session` to `src/net/session.rs`. Holds `conn_id`, `secret`, `database`, and the `CancelHandle`. Gives [FR64].
- [x] 4.4 Handle the first message. `Startup` with a version other than `PROTOCOL_VERSION` returns an error and closes. `Cancel` is served without a handshake, because the second connection never starts up. Gives [FR62] and [FR66].
- [x] 4.5 Run the message loop. `Query` answers `Error(UNKNOWN_TABLE)` then `Ready`. `Close` leaves the loop. A `Ready` follows every statement.
- [x] 4.6 On disconnect, unregister the connection and log it. Leave a `rollback_open_transaction` hook with a comment naming [FR65] and milestone 10.

## 5. Tests [serial, needs 4]

- [x] 5.1 Add `tests/server.rs`. A helper starts the server on port 0 and returns the real port, so tests never clash.
- [x] 5.2 Test the handshake. `Startup` gets `Ready` with a connection id, a secret, and `tx: none`.
- [x] 5.3 Test that a `Startup` with version 0 returns an error and closes the connection.
- [x] 5.4 Test that `Query` returns `Error(UNKNOWN_TABLE)` and then `Ready`.
- [x] 5.5 Test 100 connections at the same time, each running a `Query`. Gives [NFR14].
- [x] 5.6 Test cancel. Connection A starts up, connection B sends `Cancel` with A's id and secret, and A's flag is set. A wrong secret leaves it clear.

## Dependencies

- 1.1 blocks everything.
- 2 and 3 run at the same time.
- 4 needs 2 and 3.
- 5 needs 4.

## Done when

- `nc` connects, sends `Startup`, gets `Ready`, sends `Query`, gets `Error` and `Ready`.
- 100 connections run at the same time.
- A `Cancel` on a second socket sets the flag on the first.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
