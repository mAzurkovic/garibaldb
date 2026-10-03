//! The client against a server that answers from a script.
//!
//! A stub stands in for `garibaldb-server`, which knows no SQL until milestone 4 and
//! so cannot send a row or hold an answer back. The stub can do both, which is
//! what [FR75] and [FR79] need. It also keeps `cli` free of any dependency on
//! `server`.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use cli::conn::{Connection, StartupError};
use cli::repl::{self, Buffer};
use protocol::{
    ClientMsg, ColumnDesc, DataType, ErrorCode, PROTOCOL_VERSION, ServerMsg, TxState, Value,
};

/// A test that waits longer than this has hung.
const PATIENCE: Duration = Duration::from_secs(10);

/// A port that nothing listens on. An ephemeral port would not do, because
/// another test may bind the same number while this one runs.
const DEAD_PORT: u16 = 1;

/// One step of the answer to a `Query`.
#[derive(Clone)]
enum Step {
    Send(ServerMsg),
    /// Text that is not a message, which the client must refuse.
    Raw(&'static str),
    /// Hold the rest of the answer until a `Cancel` or a release arrives.
    Wait,
    /// Close the connection in the middle of the answer.
    Hangup,
}

/// What the stub answers.
#[derive(Clone)]
struct Script {
    /// The answer to a `Startup`. `None` closes the connection instead.
    startup: Option<ServerMsg>,
    query: Vec<Step>,
}

impl Default for Script {
    fn default() -> Script {
        Script {
            startup: Some(ready()),
            query: vec![
                Step::Send(error(ErrorCode::UnknownTable)),
                Step::Send(ready()),
            ],
        }
    }
}

fn ready() -> ServerMsg {
    ServerMsg::Ready {
        conn_id: 1,
        secret: "42".to_string(),
        tx: TxState::None,
    }
}

fn error(code: ErrorCode) -> ServerMsg {
    ServerMsg::Error {
        code,
        message: "the server holds no table".to_string(),
        position: None,
    }
}

/// A gate that one thread opens and another waits at.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn open(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            let (guard, timeout) = self.changed.wait_timeout(open, PATIENCE).unwrap();
            assert!(!timeout.timed_out(), "the gate never opened");
            open = guard;
        }
    }
}

/// A server that speaks the protocol from a script.
struct Stub {
    addr: SocketAddr,
    /// Opens once the stub is holding an answer back.
    reached: Arc<Gate>,
    /// Every message that reached the stub, on any connection, and a signal
    /// for a test waiting on one. A client that exits without an answer can
    /// outrun the read that records its last message.
    seen: Arc<(Mutex<Vec<ClientMsg>>, Condvar)>,
    gate: Arc<Gate>,
}

impl Stub {
    fn start(script: Script) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = listener.local_addr().expect("the bound address");
        let seen = Arc::new((Mutex::new(Vec::new()), Condvar::default()));
        let gate = Arc::new(Gate::default());
        let reached = Arc::new(Gate::default());

        {
            let (seen, gate, reached) = (seen.clone(), gate.clone(), reached.clone());
            thread::spawn(move || {
                for stream in listener.incoming() {
                    let stream = stream.expect("a connection");
                    let script = script.clone();
                    let (seen, gate, reached) = (seen.clone(), gate.clone(), reached.clone());
                    thread::spawn(move || serve(stream, &script, &seen, &gate, &reached));
                }
            });
        }

        Stub {
            addr,
            reached,
            seen,
            gate,
        }
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    fn connect(&self) -> Connection {
        Connection::connect("127.0.0.1", self.port()).expect("the stub accepts")
    }

    /// Lets a held answer continue, the way a `Cancel` does.
    fn release(&self) {
        self.gate.open();
    }

    /// Waits until the stub is holding the rest of an answer back.
    fn wait_until_held(&self) {
        self.reached.wait();
    }

    fn seen(&self) -> Vec<ClientMsg> {
        self.seen.0.lock().unwrap().clone()
    }

    /// Waits until the stub has read `wanted`. A message the client sends on
    /// its way out has no answer to prove it arrived.
    fn wait_until_seen(&self, wanted: &ClientMsg) {
        let (seen, arrived) = (&self.seen.0, &self.seen.1);
        let mut seen = seen.lock().unwrap();
        while !seen.contains(wanted) {
            let (guard, timeout) = arrived.wait_timeout(seen, PATIENCE).unwrap();
            assert!(!timeout.timed_out(), "the stub never read {wanted:?}");
            seen = guard;
        }
    }
}

fn serve(
    stream: TcpStream,
    script: &Script,
    seen: &(Mutex<Vec<ClientMsg>>, Condvar),
    gate: &Gate,
    reached: &Gate,
) {
    let reader = BufReader::new(stream.try_clone().expect("the socket clones"));
    let mut writer = stream;
    for line in reader.lines() {
        let Ok(line) = line else { return };
        let Ok(msg) = ClientMsg::from_line(&line) else {
            return;
        };
        seen.0.lock().unwrap().push(msg.clone());
        seen.1.notify_all();
        match msg {
            ClientMsg::Startup { .. } => match &script.startup {
                Some(msg) => send(&mut writer, msg),
                None => return,
            },
            ClientMsg::Query { .. } => {
                for step in &script.query {
                    match step {
                        Step::Send(msg) => send(&mut writer, msg),
                        Step::Raw(text) => writer.write_all(text.as_bytes()).expect("the raw line"),
                        Step::Wait => {
                            reached.open();
                            gate.wait();
                        }
                        Step::Hangup => return,
                    }
                }
            }
            // A cancel arrives on its own connection and ends the statement of
            // the other one, which is what the real server does.
            ClientMsg::Cancel { .. } => {
                gate.open();
                return;
            }
            ClientMsg::Close => return,
        }
    }
}

fn send(writer: &mut TcpStream, msg: &ServerMsg) {
    writer
        .write_all(format!("{}\n", msg.to_line()).as_bytes())
        .expect("the answer writes");
}

/// A screen that another thread can read while the client still writes to it.
#[derive(Clone)]
struct Screen(Arc<(Mutex<String>, Condvar)>);

impl Screen {
    fn new() -> Screen {
        Screen(Arc::new((Mutex::new(String::new()), Condvar::default())))
    }

    /// Waits until `needle` has reached the screen.
    fn wait_for(&self, needle: &str) {
        let (text, changed) = (&self.0.0, &self.0.1);
        let mut text = text.lock().unwrap();
        while !text.contains(needle) {
            let (guard, timeout) = changed.wait_timeout(text, PATIENCE).unwrap();
            assert!(!timeout.timed_out(), "{needle:?} never reached the screen");
            text = guard;
        }
    }

    fn text(&self) -> String {
        self.0.0.lock().unwrap().clone()
    }
}

impl Write for Screen {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .0
            .lock()
            .unwrap()
            .push_str(&String::from_utf8_lossy(buf));
        self.0.1.notify_all();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// An answer of `rows` rows, held back after the fiftieth.
fn held_answer(rows: usize) -> Vec<Step> {
    let cols = vec![ColumnDesc {
        name: "a".to_string(),
        ty: DataType::Text,
    }];
    let mut steps = vec![Step::Send(ServerMsg::RowDesc { cols })];
    for i in 0..rows {
        if i == 50 {
            steps.push(Step::Wait);
        }
        steps.push(Step::Send(ServerMsg::DataRow {
            values: vec![Value::Text(format!("r{i}"))],
        }));
    }
    steps
}

/// [FR71]. The startup names the database and takes the id and the secret.
#[test]
fn a_startup_names_the_database_and_the_protocol_version() {
    let stub = Stub::start(Script::default());
    let mut conn = stub.connect();
    conn.startup("shop").expect("the stub answers ready");
    assert_eq!(
        stub.seen(),
        vec![ClientMsg::Startup {
            version: PROTOCOL_VERSION,
            database: "shop".to_string(),
        }]
    );
}

/// A server that refuses the connection says why, and the client repeats it.
#[test]
fn a_refused_startup_carries_the_server_message() {
    let stub = Stub::start(Script {
        startup: Some(ServerMsg::Error {
            code: ErrorCode::TooManyConnections,
            message: "the server is full".to_string(),
            position: None,
        }),
        ..Script::default()
    });
    let mut conn = stub.connect();
    let e = conn.startup("shop").expect_err("the stub refuses");
    assert!(matches!(e, StartupError::Refused(_)), "{e}");
    assert!(e.to_string().contains("TOO_MANY_CONNECTIONS"), "{e}");
}

/// An answer that is not a `Ready` is not a startup either.
#[test]
fn a_startup_answered_with_another_message_is_an_error() {
    let stub = Stub::start(Script {
        startup: Some(ServerMsg::Complete {
            kind: "SELECT".to_string(),
            rows: 0,
        }),
        ..Script::default()
    });
    let mut conn = stub.connect();
    let e = conn.startup("shop").expect_err("the stub answers wrongly");
    assert!(matches!(e, StartupError::Unexpected(_)), "{e}");
}

/// A server that closes instead of answering the startup is an error.
#[test]
fn a_startup_that_the_server_never_answers_is_an_error() {
    let stub = Stub::start(Script {
        startup: None,
        query: vec![],
    });
    let mut conn = stub.connect();
    let e = conn.startup("shop").expect_err("the stub closes");
    assert!(matches!(e, StartupError::Io(_)), "{e}");
}

#[test]
fn a_connection_to_a_closed_port_fails() {
    assert!(Connection::connect("127.0.0.1", DEAD_PORT).is_err());
}

/// [FR75]. The rows that fix the widths reach the screen while the rest of the
/// answer is still on its way.
#[test]
fn rows_reach_the_screen_before_the_last_row_arrives() {
    let stub = Stub::start(Script {
        startup: Some(ready()),
        query: {
            let mut steps = held_answer(60);
            steps.push(Step::Send(ServerMsg::Complete {
                kind: "SELECT".to_string(),
                rows: 60,
            }));
            steps.push(Step::Send(ready()));
            steps
        },
    });
    let mut conn = stub.connect();
    conn.startup("shop").expect("the stub answers ready");

    let screen = Screen::new();
    let running = {
        let mut screen = screen.clone();
        thread::spawn(move || {
            let outcome = repl::run_statement(&mut conn, "SELECT a FROM t;", &mut screen)
                .expect("the answer arrives");
            (conn, outcome)
        })
    };

    screen.wait_for(" r49");
    assert!(
        !screen.text().contains("r50"),
        "the held rows must still be on their way: {}",
        screen.text()
    );

    stub.release();
    let (_conn, outcome) = running.join().expect("the statement ends");
    assert!(!outcome.failed);
    let shown = screen.text();
    assert!(shown.contains(" r59"), "{shown}");
    assert!(shown.ends_with("(60 rows)\n"), "{shown}");
}

/// [FR79]. Ctrl-C reaches the server on a second connection, and the first
/// connection stays open.
#[test]
fn a_cancel_travels_on_a_second_connection_and_the_first_stays_open() {
    let stub = Stub::start(Script {
        startup: Some(ready()),
        query: {
            let mut steps = held_answer(60);
            steps.push(Step::Send(ServerMsg::Error {
                code: ErrorCode::TxnAborted,
                message: "the statement was canceled".to_string(),
                position: None,
            }));
            steps.push(Step::Send(ready()));
            steps
        },
    });
    let mut conn = stub.connect();
    let key = conn.startup("shop").expect("the stub answers ready");

    let screen = Screen::new();
    let running = {
        let mut screen = screen.clone();
        thread::spawn(move || {
            let outcome = repl::run_statement(&mut conn, "SELECT a FROM t;", &mut screen)
                .expect("the answer arrives");
            (conn, outcome)
        })
    };
    screen.wait_for(" r49");

    // The handler thread does exactly this when Ctrl-C arrives.
    key.send().expect("the cancel reaches the stub");

    let (mut conn, outcome) = running.join().expect("the statement ends");
    assert!(outcome.failed, "a canceled statement failed");
    assert!(
        screen
            .text()
            .contains("TXN_ABORTED: the statement was canceled"),
        "{}",
        screen.text()
    );

    // The connection is still good, so the client can still leave politely.
    conn.close().expect("the first connection stays open");

    assert!(stub.seen().contains(&ClientMsg::Cancel {
        conn_id: 1,
        secret: "42".to_string(),
    }));
}

/// A server that stops in the middle of an answer is an error, not a hang.
#[test]
fn an_answer_that_the_server_cuts_short_is_an_error() {
    let cols = vec![ColumnDesc {
        name: "a".to_string(),
        ty: DataType::Text,
    }];
    let stub = Stub::start(Script {
        startup: Some(ready()),
        query: vec![Step::Send(ServerMsg::RowDesc { cols }), Step::Hangup],
    });
    let mut conn = stub.connect();
    conn.startup("shop").expect("the stub answers ready");
    let mut screen = Screen::new();
    let e = repl::run_statement(&mut conn, "SELECT a FROM t;", &mut screen)
        .expect_err("the answer stops");
    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
}

/// A line that is not a message is an error, and not a panic.
#[test]
fn an_answer_that_is_not_a_message_is_an_error() {
    let stub = Stub::start(Script {
        startup: Some(ready()),
        query: vec![Step::Raw("hello\n")],
    });
    let mut conn = stub.connect();
    conn.startup("shop").expect("the stub answers ready");
    let mut screen = Screen::new();
    let e = repl::run_statement(&mut conn, "SELECT a FROM t;", &mut screen)
        .expect_err("the answer is not a message");
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
}

/// [FR77] and [FR78]. One statement from the command line, then the status
/// code that says it failed.
#[test]
fn the_one_shot_client_prints_the_error_and_stops_with_a_non_zero_code() {
    let stub = Stub::start(Script::default());
    let out = Command::new(env!("CARGO_BIN_EXE_garibaldb"))
        .args(["--host", "127.0.0.1"])
        .args(["--port", &stub.port().to_string()])
        .args(["-c", "SELECT 1;"])
        .output()
        .expect("the garibaldb binary runs");
    let shown = String::from_utf8(out.stdout).expect("the output is text");
    assert_eq!(shown, "UNKNOWN_TABLE: the server holds no table\n");
    assert_eq!(out.status.code(), Some(1));
    assert!(stub.seen().contains(&ClientMsg::Query {
        sql: "SELECT 1;".to_string()
    }));
}

/// A statement that succeeds stops with zero.
#[test]
fn the_one_shot_client_stops_with_zero_when_the_statement_succeeds() {
    let cols = vec![ColumnDesc {
        name: "a".to_string(),
        ty: DataType::Integer,
    }];
    let stub = Stub::start(Script {
        startup: Some(ready()),
        query: vec![
            Step::Send(ServerMsg::RowDesc { cols }),
            Step::Send(ServerMsg::DataRow {
                values: vec![Value::Integer(1)],
            }),
            Step::Send(ServerMsg::Complete {
                kind: "SELECT".to_string(),
                rows: 1,
            }),
            Step::Send(ready()),
        ],
    });
    let out = Command::new(env!("CARGO_BIN_EXE_garibaldb"))
        .args(["--port", &stub.port().to_string()])
        .args(["-c", "SELECT 1;"])
        .output()
        .expect("the garibaldb binary runs");
    assert_eq!(
        String::from_utf8(out.stdout).expect("the output is text"),
        " a\n---\n 1\n(1 row)\n"
    );
    assert_eq!(out.status.code(), Some(0));
}

/// A bad flag stops before any connection, with the code the server uses.
#[test]
fn a_bad_flag_stops_with_code_two() {
    let out = Command::new(env!("CARGO_BIN_EXE_garibaldb"))
        .args(["--nonsense"])
        .output()
        .expect("the garibaldb binary runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown flag `--nonsense`"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A server that is not there stops with the same code as a bad flag.
#[test]
fn a_server_that_is_not_there_stops_with_code_two() {
    let out = Command::new(env!("CARGO_BIN_EXE_garibaldb"))
        .args(["--port", &DEAD_PORT.to_string()])
        .args(["-c", "SELECT 1;"])
        .output()
        .expect("the garibaldb binary runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot reach"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// [FR72] and [FR73]. A statement typed over three lines runs at the `;`.
#[test]
fn a_statement_typed_over_three_lines_runs_once_the_semicolon_arrives() {
    let stub = Stub::start(Script::default());
    let mut conn = stub.connect();
    conn.startup("shop").expect("the stub answers ready");

    let mut buffer = Buffer::default();
    let mut screen = Screen::new();
    let tx = repl::feed(
        &mut conn,
        &mut buffer,
        TxState::None,
        "SELECT a",
        &mut screen,
    )
    .expect("the line is held");
    assert!(
        screen.text().is_empty(),
        "a half-typed statement runs nothing"
    );
    repl::feed(&mut conn, &mut buffer, tx, "FROM t", &mut screen).expect("the line is held");
    assert!(
        screen.text().is_empty(),
        "a half-typed statement runs nothing"
    );
    repl::feed(&mut conn, &mut buffer, tx, "WHERE a = 1;", &mut screen).expect("the answer");

    assert!(screen.text().contains("UNKNOWN_TABLE"), "{}", screen.text());
    assert!(stub.seen().contains(&ClientMsg::Query {
        sql: "SELECT a\nFROM t\nWHERE a = 1;".to_string(),
    }));
}

/// A server that refuses the startup stops the client before any statement.
#[test]
fn a_refused_startup_stops_with_code_two() {
    let stub = Stub::start(Script {
        startup: Some(ServerMsg::Error {
            code: ErrorCode::TooManyConnections,
            message: "the server is full".to_string(),
            position: None,
        }),
        ..Script::default()
    });
    let out = Command::new(env!("CARGO_BIN_EXE_garibaldb"))
        .args(["--port", &stub.port().to_string()])
        .args(["-c", "SELECT 1;"])
        .output()
        .expect("the garibaldb binary runs");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("TOO_MANY_CONNECTIONS"),
        "{:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The prompt leaves when the input ends, which is what Ctrl-D does, and it
/// says goodbye to the server on the way out.
#[test]
fn the_prompt_leaves_when_the_input_ends() {
    let stub = Stub::start(Script::default());
    let out = Command::new(env!("CARGO_BIN_EXE_garibaldb"))
        .args(["--port", &stub.port().to_string()])
        .stdin(Stdio::null())
        .output()
        .expect("the garibaldb binary runs");
    assert_eq!(out.status.code(), Some(0));
    stub.wait_until_seen(&ClientMsg::Close);
}

/// [FR79] end to end. Ctrl-C reaches the running client as a signal, the
/// client stops the statement on a second connection, and the first
/// connection carries the answer that follows.
#[cfg(unix)]
#[test]
fn ctrl_c_stops_a_running_statement() {
    let stub = Stub::start(Script {
        startup: Some(ready()),
        query: {
            let mut steps = held_answer(60);
            steps.push(Step::Send(ServerMsg::Error {
                code: ErrorCode::TxnAborted,
                message: "the statement was canceled".to_string(),
                position: None,
            }));
            steps.push(Step::Send(ready()));
            steps
        },
    });
    let child = Command::new(env!("CARGO_BIN_EXE_garibaldb"))
        .args(["--port", &stub.port().to_string()])
        .args(["-c", "SELECT a FROM t;"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("the garibaldb binary starts");

    // The handler is installed before the statement goes out, so the signal
    // cannot arrive before the client can answer it.
    stub.wait_until_held();
    let signalled = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(signalled.success(), "the signal reached the client");

    let out = child.wait_with_output().expect("the client ends");
    let shown = String::from_utf8(out.stdout).expect("the output is text");
    assert!(
        shown.contains(" r49"),
        "the rows before the cancel: {shown}"
    );
    assert!(
        shown.contains("TXN_ABORTED"),
        "the answer after it: {shown}"
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stub.seen()
            .iter()
            .any(|m| matches!(m, ClientMsg::Cancel { .. })),
        "the cancel reached the server: {:?}",
        stub.seen()
    );
}

/// A column list and nothing more. The server stops in the middle.
fn cut_short() -> Script {
    Script {
        startup: Some(ready()),
        query: vec![
            Step::Send(ServerMsg::RowDesc {
                cols: vec![ColumnDesc {
                    name: "a".to_string(),
                    ty: DataType::Text,
                }],
            }),
            Step::Hangup,
        ],
    }
}

/// A server that stops mid-answer ends the one-shot client with the code that
/// stands for a connection that failed.
#[test]
fn a_server_that_stops_mid_answer_stops_the_one_shot_client_with_code_two() {
    let stub = Stub::start(cut_short());
    let out = Command::new(env!("CARGO_BIN_EXE_garibaldb"))
        .args(["--port", &stub.port().to_string()])
        .args(["-c", "SELECT a FROM t;"])
        .output()
        .expect("the garibaldb binary runs");
    assert_eq!(out.status.code(), Some(2));
}

/// The same at the prompt. The statement comes in on the input, so the prompt
/// runs it before the server stops.
#[test]
fn a_server_that_stops_mid_answer_stops_the_prompt_with_code_two() {
    let stub = Stub::start(cut_short());
    let mut child = Command::new(env!("CARGO_BIN_EXE_garibaldb"))
        .args(["--port", &stub.port().to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the garibaldb binary starts");
    child
        .stdin
        .take()
        .expect("the input is a pipe")
        .write_all(b"SELECT a FROM t;\n")
        .expect("the statement is typed");
    let out = child.wait_with_output().expect("the client ends");
    assert_eq!(out.status.code(), Some(2));
    assert!(stub.seen().contains(&ClientMsg::Query {
        sql: "SELECT a FROM t;".to_string(),
    }));
}
