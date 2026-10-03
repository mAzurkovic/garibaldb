# Milestone 3, CLI — Tasks

Source: `docs/projectplan.md` M3. Types: `docs/internals.md` section 6.
Requirements: [FR71]-[FR80].

Crate `crates/cli`, binary `garibaldb`. It depends on `protocol`, never on `server`.
The server knows no SQL until milestone 4, so every real query answers
`Error(UNKNOWN_TABLE)`. A stub server carries the tests that need rows.

Settled before this list. Do not reopen.
- `rustyline` gives line editing, history, and multi-line input. `design.md` already names it.
- `ctrlc` gives the signal handler. `std` has none, and `rustyline` sees Ctrl-C only while it
  holds the prompt, not while the CLI waits on the socket. Add a row for it to the crate table
  in `design.md`.
- The `ctrlc` handler sends the `Cancel` itself. `ctrlc` starts a dedicated thread and runs the
  closure there, so the closure is ordinary code that may open a socket. The main thread stays
  in its read and sees the statement end, because the server ends it. Nothing polls a flag, and
  no read has to return early.
- Ctrl-C reaches exactly one of the two, decided by the terminal mode. At the prompt `rustyline`
  holds the terminal raw with signals off, so Ctrl-C is a byte and `rustyline` reports
  `Interrupted`. While a statement runs the terminal is back to normal, so Ctrl-C raises
  `SIGINT` and the handler thread answers. They never both fire.
- A test speaks to a stub server: a `TcpListener` inside the test that writes protocol lines by
  hand. `crates/cli` never depends on `crates/server`, not even as a dev-dependency. The stub
  also gives the slow answer that [FR79] needs, which the real server cannot give yet.
- `testplan.md` maps [S11] to harness [H1], which arrives in milestone 4 and drives the server,
  not the CLI. These tests use the stub instead. Point [S11] at them when [H1] lands.
- `TableWriter` measures widths from the header and the first 50 rows, prints those, then
  streams the rest. A later value that is wider overflows its column. [FR75] forbids a wait for
  the last row, so a width cannot depend on a row that has not arrived.
- `--database` defaults to `default`. [FR71] asks for the name, and the done-when runs
  `garibaldb -c "SELECT 1"` with no flag. The server takes any name until milestone 5.
- History lives in memory. No history file until someone asks for one.
- No meta-commands. Ctrl-D leaves the REPL and sends `Close`.
- Exit codes: 0 success, 1 a statement failed, 2 a bad flag or the connection failed. The
  server uses 2 for a bad flag already.

## 1. Crate skeleton and the command line [serial]

- [x] 1.1 Add `crates/cli` to the workspace members. `Cargo.toml` depends on `protocol`,
  `rustyline`, and `ctrlc`. Binary name `garibaldb`. `cargo check` passes.
- [x] 1.2 Add `ctrlc` to the crate table in `docs/design.md`, with the reason in one line.
- [x] 1.3 Add `Args` to `src/args.rs`. Reads `--host`, `--port`, `--database`, and `-c`. Hand
  written like `Config::parse` in the server. No `clap`. Defaults `127.0.0.1`, `5432`,
  `default`. Gives [FR71].
- [x] 1.4 Test `Args`. Each flag reads, the last one wins, a missing value is an error, an
  unknown flag names itself, and `-c` keeps a string with spaces and a `;` whole.
- [x] 1.5 Add `src/main.rs`. Reads the args, connects, then runs one statement for `-c` or the
  REPL for no `-c`. A bad flag prints the error and stops with code 2.

## 2. Connection [parallel, needs 1.1]

- [x] 2.1 Add `Connection` to `src/conn.rs`. `connect(host, port)` opens a `TcpStream` and wraps
  a clone in a `BufReader`, so a read and a write can both hold the socket.
- [x] 2.2 Add `startup(database)`. Sends `Startup` with `PROTOCOL_VERSION`, reads `Ready`, and
  keeps `conn_id` and `secret`. An `Error` in reply is a failure with its message. Gives [FR71].
- [x] 2.3 Add `query(sql)`. Sends `Query` and returns an iterator of `ServerMsg` that ends after
  `Complete` or `Error`. It returns each message as it arrives and never collects them, which
  is what [FR75] asks for.
- [x] 2.4 Add `CancelKey` to `src/conn.rs`. Holds the address, the `conn_id`, and the secret.
  `Clone`, and it owns everything it needs. `send()` opens a second socket, writes `Cancel`, and
  closes it. No handshake, because the server serves `Cancel` first. Gives [FR79].
- [x] 2.5 Add `Connection::cancel_key()`, which returns the key after a startup. The key exists
  apart from the `Connection` because the iterator from 2.3 borrows the connection while a
  statement runs, so the canceller cannot hold it too.
- [x] 2.6 Add `close()`. Sends `Close` and drops the socket.
- [x] 2.7 Test against the stub from 6.1. A handshake keeps the id and the secret. A reply to a
  version the server refuses is an error. `CancelKey::send` reaches the stub on a second socket.

## 3. Rendering [parallel, needs 1.1]

- [x] 3.1 Add `TableWriter` to `src/render.rs`. `header(cols)` holds the names until the widths
  are known. Gives [FR74].
- [x] 3.2 Add `row(values)`. Buffers the first 50 rows, then prints the header, the separator,
  and every buffered row. After that each row prints as it arrives. Gives [FR75].
- [x] 3.3 Render a value: `Null` as `NULL`, `Boolean` as `true` or `false`, `Decimal` through
  its own `to_string`, so no digit is lost. Numbers align right, everything else left.
- [x] 3.4 Add `footer(count)`. Prints `(N rows)`, and `(1 row)` for one.
- [x] 3.5 Add `error(code, message, position)`. Prints the code, the message, and the position
  when there is one. Gives [FR76].
- [x] 3.6 Test the output. A table of two columns, a `NULL`, a value wider than its header, a
  value wider than the first 50 rows that overflows its column, zero rows, and an error line.

## 4. REPL, prompt, and one-shot mode [serial, needs 2 and 3]

- [x] 4.1 Add `Repl` to `src/repl.rs`. Holds a `rustyline` editor, the buffer, and the
  transaction state from the last `Ready`.
- [x] 4.2 Add `read_statement`. Collects lines until a line ends with `;`. An empty line adds
  nothing. Gives [FR72] and [FR73].
- [x] 4.3 Prompt from the transaction state: `garibaldb> ` for none, `garibaldb(tx)> ` for open,
  `garibaldb(ro)> ` for read only, and `...> ` for a line after the first. Gives [FR80].
- [x] 4.4 Run one statement: send it, render each message as it arrives, print the footer, and
  take the new transaction state from `Ready`.
- [x] 4.5 Ctrl-C at the prompt clears the line and stays in the REPL. Ctrl-D sends `Close` and
  leaves with code 0.
- [x] 4.6 Add one-shot mode for `-c`. Startup, one statement, render, `Close`, stop. Gives
  [FR77].
- [x] 4.7 Stop with 0 when the statement succeeded and 1 when it failed. Gives [FR78].

## 5. Cancel and Ctrl-C [serial, needs 4]

- [x] 5.1 Hold the `CancelKey` in an `Arc<Mutex<Option<CancelKey>>>`. It is empty until the
  startup fills it, and the handler thread reads it.
- [x] 5.2 Install the `ctrlc` handler once in `main`, after the startup. It takes the key and
  calls `send`. A failure prints a line and nothing more, because a lost cancel must not stop
  the CLI. Gives [FR79].
- [x] 5.3 No flag, and no read that ends early. The main thread stays in its read, and the
  server ends the statement, so the normal path prints what the server sends and returns to the
  prompt with the connection open.
- [x] 5.4 A `Cancel` between statements reaches a server with nothing to stop. The server
  ignores it, so the CLI needs no guard for that case.

## 6. Tests [serial, needs 5]

- [x] 6.1 Add `tests/stub.rs`. A stub server on port 0 that returns its real port, reads client
  lines, and writes a scripted reply. It can hold a row back, which makes a slow answer.
- [x] 6.2 Test the one-shot path end to end: the stub answers `Error(UNKNOWN_TABLE)`, the CLI
  prints the code and the message, and stops with 1. This is the done-when of this milestone.
- [x] 6.3 Test streaming: the stub sends 60 rows, then holds the rest back. The CLI prints the
  first 50 before the last row arrives, because 50 rows fix the widths. Fewer than 50 would
  print nothing until the end, so the count in this test and the one in 3.2 must match.
  Gives [FR75].
- [x] 6.4 Test cancel: the stub stalls mid-answer, the test calls `CancelKey::send` the way the
  handler thread would, and a `Cancel` with the right id and secret arrives on a second socket
  while the first stays open. The test does not raise a real `SIGINT`, which would stop the
  test runner. Gives [FR79].
- [x] 6.5 Test multi-line input: a statement typed over three lines runs once the `;` arrives.
- [x] 6.6 `cargo test --workspace` passes. Clippy is clean with `-D warnings`.

## Dependencies

- 1.1 blocks everything.
- 2 and 3 run at the same time.
- 4 needs 2 and 3.
- 5 needs 4.
- 6 needs 5. Task 6.1 lands early, because 2.7 already uses the stub.

## Done when

- `garibaldb -c "SELECT 1"` against a real `garibaldb-server` prints the server error and stops with a
  non-zero code.
- A statement typed over more than one line runs when the `;` arrives.
- Ctrl-C during a slow answer returns the prompt with the connection open.
- The prompt shows when a transaction is open.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
