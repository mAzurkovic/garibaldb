//! The server over a real socket, seen from outside the process.
//!
//! `server` is a binary crate, so a test cannot import its modules. Each test
//! therefore starts the `garibald` binary on port 0 and speaks the protocol
//! over TCP, which is what a client does. See [FR61] to [FR66] and [NFR14].
//!
//! Nothing here sleeps. The binary logs the bound port before it accepts, so
//! the log line is the signal that the socket stands.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStderr, Command, Stdio};

use protocol::{ClientMsg, ErrorCode, PROTOCOL_VERSION, ServerMsg, TxState};

/// One `garibald` process on a port that the operating system chose. `Drop`
/// kills it, so a failed test leaves no port held.
struct Server {
    child: Child,
    port: u16,
    /// The log of the process, which also reports what a `Cancel` matched.
    log: BufReader<ChildStderr>,
}

impl Server {
    /// Starts the binary on port 0 and reads the port back from its log.
    fn start() -> Server {
        Server::start_with(&[])
    }

    /// The same, with more flags. Used to make the connection cap small
    /// enough to reach in a test.
    fn start_with(flags: &[&str]) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_garibald"))
            .args(["--port", "0"])
            .args(flags)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the garibald binary starts");
        let mut log = BufReader::new(child.stderr.take().expect("stderr is a pipe"));
        let line = wait_log(&mut log, "listening on port");
        let port = port_of(&line).unwrap_or_else(|| panic!("no port in {line:?}"));
        Server { child, port, log }
    }

    /// One client connection. The listener is bound before it logs, so the
    /// connection stands even before `accept` runs.
    fn connect(&self) -> Conn {
        let stream = TcpStream::connect(("127.0.0.1", self.port)).expect("the server accepts");
        Conn {
            writer: stream.try_clone().expect("the socket clones"),
            reader: BufReader::new(stream),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The port of the startup line, which reads `... on port 39251, data in ...`.
fn port_of(line: &str) -> Option<u16> {
    line.split_once("listening on port ")?
        .1
        .split(',')
        .next()?
        .parse()
        .ok()
}

/// Reads log lines until one holds `needle`. The pipe ends when the process
/// stops, so a dead server fails the test instead of hanging it.
fn wait_log(log: &mut BufReader<ChildStderr>, needle: &str) -> String {
    for line in log.lines() {
        let line = line.expect("the log reads");
        if line.contains(needle) {
            return line;
        }
    }
    panic!("the server stopped before it logged {needle:?}");
}

/// One client socket. The connection closes when it drops.
struct Conn {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Conn {
    fn send(&mut self, msg: &ClientMsg) {
        writeln!(self.writer, "{}", msg.to_line()).expect("the message writes");
    }

    /// The next message, or `None` at the end of the connection.
    fn recv(&mut self) -> Option<ServerMsg> {
        let mut line = String::new();
        match self.reader.read_line(&mut line).expect("the answer reads") {
            0 => None,
            _ => Some(ServerMsg::from_line(line.trim_end()).expect("the answer is a message")),
        }
    }

    fn expect(&mut self) -> ServerMsg {
        self.recv().expect("an answer, not a closed connection")
    }

    /// Sends `Startup` and returns the id and the secret of the `Ready`.
    fn start_up(&mut self) -> (u64, String) {
        self.send(&startup(PROTOCOL_VERSION));
        match self.expect() {
            ServerMsg::Ready {
                conn_id, secret, ..
            } => (conn_id, secret),
            other => panic!("expected a ready, got {other:?}"),
        }
    }
}

fn startup(version: u16) -> ClientMsg {
    ClientMsg::Startup {
        version,
        database: "shop".to_string(),
    }
}

fn query() -> ClientMsg {
    ClientMsg::Query {
        sql: "SELECT 1".to_string(),
    }
}

fn code_of(msg: &ServerMsg) -> ErrorCode {
    match msg {
        ServerMsg::Error { code, .. } => *code,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// [FR62] and [FR64].
#[test]
fn a_startup_answers_ready_with_an_id_a_secret_and_no_transaction() {
    let server = Server::start();
    let mut conn = server.connect();
    conn.send(&startup(PROTOCOL_VERSION));
    let ServerMsg::Ready {
        conn_id,
        secret,
        tx,
    } = conn.expect()
    else {
        panic!("expected a ready");
    };
    assert_eq!((conn_id, tx), (1, TxState::None));
    assert!(secret.parse::<u64>().is_ok(), "the secret {secret:?}");
}

/// [FR62]. The server speaks version 1 only.
#[test]
fn a_startup_with_an_unknown_version_answers_an_error_and_closes() {
    let server = Server::start();
    let mut conn = server.connect();
    conn.send(&startup(0));
    assert_eq!(code_of(&conn.expect()), ErrorCode::SyntaxError);
    assert_eq!(conn.recv(), None, "the connection stays open");
}

/// The server knows no table until milestone 4, and a `Ready` follows every
/// statement.
#[test]
fn a_query_answers_unknown_table_and_then_ready() {
    let server = Server::start();
    let mut conn = server.connect();
    let (conn_id, secret) = conn.start_up();
    conn.send(&query());
    assert_eq!(code_of(&conn.expect()), ErrorCode::UnknownTable);
    assert_eq!(
        conn.expect(),
        ServerMsg::Ready {
            conn_id,
            secret,
            tx: TxState::None,
        }
    );
}

/// [FR63] and [NFR14]. Every connection stands open while the queries run.
#[test]
fn a_hundred_connections_run_a_query_at_the_same_time() {
    let server = Server::start();
    let mut conns: Vec<Conn> = (0..100).map(|_| server.connect()).collect();
    let mut ids: Vec<u64> = conns.iter_mut().map(|c| c.start_up().0).collect();
    for conn in &mut conns {
        conn.send(&query());
    }
    for conn in &mut conns {
        assert_eq!(code_of(&conn.expect()), ErrorCode::UnknownTable);
        assert!(matches!(conn.expect(), ServerMsg::Ready { .. }));
    }
    // Each connection got its own session, so no id repeats.
    ids.sort_unstable();
    assert_eq!(ids, (1..=100).collect::<Vec<u64>>());
}

/// [FR66]. The cancel connection carries no session, so the match reaches the
/// log and not the wire. The flag itself is unreadable until milestone 11.
#[test]
fn a_cancel_matches_only_with_the_right_secret() {
    let mut server = Server::start();
    let mut target = server.connect();
    let (conn_id, secret) = target.start_up();
    let wrong = secret.parse::<u64>().unwrap().wrapping_add(1).to_string();
    for (secret, want) in [(secret, "match true"), (wrong, "match false")] {
        let mut conn = server.connect();
        conn.send(&ClientMsg::Cancel { conn_id, secret });
        assert_eq!(conn.recv(), None, "a cancel connection answers nothing");
        assert!(wait_log(&mut server.log, "cancels").ends_with(want));
    }
}

/// [NFR14] caps the connections. Past the cap the server names the reason and
/// closes, so a client can tell a busy server from a full disk.
#[test]
fn a_connection_past_the_cap_is_refused() {
    let server = Server::start_with(&["--max-connections", "2"]);

    // Hold both slots open. A closed connection would free one.
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut conn = server.connect();
        conn.start_up();
        held.push(conn);
    }

    let mut extra = server.connect();
    assert_eq!(code_of(&extra.expect()), ErrorCode::TooManyConnections);
    assert_eq!(extra.recv(), None, "the server closes the extra connection");

    // A slot that frees up lets the next client in.
    held.pop();
    let mut after = server.connect();
    let (conn_id, _) = after.start_up();
    assert!(conn_id > 0);
}
