//! The statement runner, and the socket helpers that every suite shares.
//!
//! A case file holds statements and the result each one must give, which is
//! the shape `sqllogictest` uses. A test drives the server over TCP and reads
//! no data file of its own, so it proves the requirement and not the design.
#![allow(dead_code)] // Each suite that includes this module uses a part of it.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, ChildStderr, Command, Stdio};

use protocol::{ClientMsg, ErrorCode, PROTOCOL_VERSION, ServerMsg, Value};

/// One server process on a port that the operating system chose. `Drop` kills
/// it, so a failed test leaves no port held.
pub struct Server {
    child: Child,
    port: u16,
    /// The log of the process, which also reports what a `Cancel` matched.
    pub log: BufReader<ChildStderr>,
}

impl Server {
    /// Starts the binary on port 0 and reads the port back from its log.
    pub fn start() -> Server {
        Server::start_with(&[])
    }

    /// The same, with more flags. Used to make the connection cap small
    /// enough to reach in a test.
    pub fn start_with(flags: &[&str]) -> Server {
        let mut child = Command::new(env!("CARGO_BIN_EXE_garibaldb-server"))
            .args(["--port", "0"])
            .args(flags)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the garibaldb-server binary starts");
        let mut log = BufReader::new(child.stderr.take().expect("stderr is a pipe"));
        let line = wait_log(&mut log, "listening on port");
        let port = port_of(&line).unwrap_or_else(|| panic!("no port in {line:?}"));
        Server { child, port, log }
    }

    /// One client connection. The listener is bound before it logs, so the
    /// connection stands even before `accept` runs.
    pub fn connect(&self) -> Conn {
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
pub fn wait_log(log: &mut BufReader<ChildStderr>, needle: &str) -> String {
    for line in log.lines() {
        let line = line.expect("the log reads");
        if line.contains(needle) {
            return line;
        }
    }
    panic!("the server stopped before it logged {needle:?}");
}

/// One client socket. The connection closes when it drops.
pub struct Conn {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Conn {
    pub fn send(&mut self, msg: &ClientMsg) {
        writeln!(self.writer, "{}", msg.to_line()).expect("the message writes");
    }

    /// The next message, or `None` at the end of the connection.
    pub fn recv(&mut self) -> Option<ServerMsg> {
        let mut line = String::new();
        match self.reader.read_line(&mut line).expect("the answer reads") {
            0 => None,
            _ => Some(ServerMsg::from_line(line.trim_end()).expect("the answer is a message")),
        }
    }

    pub fn expect(&mut self) -> ServerMsg {
        self.recv().expect("an answer, not a closed connection")
    }

    /// Sends `Startup` and returns the id and the secret of the `Ready`.
    pub fn start_up(&mut self) -> (u64, String) {
        self.send(&startup(PROTOCOL_VERSION));
        match self.expect() {
            ServerMsg::Ready {
                conn_id, secret, ..
            } => (conn_id, secret),
            other => panic!("expected a ready, got {other:?}"),
        }
    }

    /// Sends one statement and reads every message up to the `Ready` that
    /// closes it.
    pub fn run(&mut self, sql: &str) -> Vec<ServerMsg> {
        self.send(&ClientMsg::Query {
            sql: sql.to_string(),
        });
        let mut answer = Vec::new();
        loop {
            let message = self.expect();
            let done = matches!(message, ServerMsg::Ready { .. });
            answer.push(message);
            if done {
                return answer;
            }
        }
    }
}

pub fn startup(version: u16) -> ClientMsg {
    ClientMsg::Startup {
        version,
        database: "shop".to_string(),
    }
}

/// What one case expects of its statement.
#[derive(Debug, PartialEq, Eq)]
pub enum Expected {
    /// The statement runs and reports no error.
    Ok,
    Error(ErrorCode),
    /// The rows the statement returns, as the text under the `----` line.
    Rows(String),
}

/// One statement of a case file, and the result it must give.
#[derive(Debug, PartialEq, Eq)]
pub struct Case {
    /// The line the case starts on, so a failure points into the file.
    pub line: usize,
    pub sql: String,
    pub expected: Expected,
}

/// Reads a case file. A blank line ends a case, and a `#` line is a comment.
pub fn read_cases(text: &str) -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    let mut lines = text.lines().enumerate().peekable();
    while let Some((index, header)) = lines.next() {
        let header = header.trim();
        if header.is_empty() || header.starts_with('#') {
            continue;
        }
        let at = index + 1;
        let expected = match header.split_whitespace().collect::<Vec<&str>>().as_slice() {
            ["statement", "ok"] => Expected::Ok,
            ["statement", "error", name] => Expected::Error(
                error_code(name).ok_or_else(|| format!("line {at}: unknown error code {name}"))?,
            ),
            ["query"] => Expected::Rows(String::new()),
            _ => return Err(format!("line {at}: {header:?} is not a case header")),
        };

        let mut sql = String::new();
        while let Some((_, line)) = lines.peek() {
            let line = line.trim_end();
            if line.trim().is_empty() || line.trim() == "----" {
                break;
            }
            if !sql.is_empty() {
                sql.push('\n');
            }
            sql.push_str(line);
            lines.next();
        }
        if sql.is_empty() {
            return Err(format!("line {at}: the case holds no statement"));
        }

        let expected = match expected {
            Expected::Rows(_) => Expected::Rows(read_rows(&mut lines, at)?),
            settled => settled,
        };
        cases.push(Case {
            line: at,
            sql,
            expected,
        });
    }
    Ok(cases)
}

/// The `----` line and the rows under it.
fn read_rows<'a>(
    lines: &mut std::iter::Peekable<impl Iterator<Item = (usize, &'a str)>>,
    at: usize,
) -> Result<String, String> {
    match lines.next() {
        Some((_, line)) if line.trim() == "----" => {}
        _ => return Err(format!("line {at}: a query needs a ---- line")),
    }
    let mut rows = String::new();
    while let Some((_, line)) = lines.peek() {
        if line.trim().is_empty() {
            break;
        }
        rows.push_str(line.trim_end());
        rows.push('\n');
        lines.next();
    }
    Ok(rows)
}

/// The code that a case file names. The wire name is the contract, so the
/// file spells a code the way the protocol does.
fn error_code(name: &str) -> Option<ErrorCode> {
    let codes = [
        ErrorCode::SyntaxError,
        ErrorCode::UnknownTable,
        ErrorCode::UnknownColumn,
        ErrorCode::TypeMismatch,
        ErrorCode::DuplicateKey,
        ErrorCode::NotNullViolation,
        ErrorCode::StorageFull,
        ErrorCode::TxnAborted,
        ErrorCode::SchemaChangeInTxn,
        ErrorCode::LockTimeout,
        ErrorCode::TooManyConnections,
    ];
    codes.into_iter().find(|code| code.as_str() == name)
}

/// The rows of an answer, written the way a case file writes them.
pub fn render(answer: &[ServerMsg]) -> String {
    let mut out = String::new();
    let mut rows = 0u64;
    for message in answer {
        if let ServerMsg::DataRow { values } = message {
            rows += 1;
            let cells: Vec<String> = values.iter().map(cell).collect();
            out.push_str(&cells.join(" | "));
            out.push('\n');
        }
    }
    let word = match rows {
        1 => "row",
        _ => "rows",
    };
    out.push_str(&format!("({rows} {word})\n"));
    out
}

fn cell(value: &Value) -> String {
    match value {
        Value::Integer(i) => i.to_string(),
        Value::Text(text) => text.clone(),
        Value::Boolean(b) => b.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Null => "NULL".to_string(),
    }
}

/// The error of an answer, when it holds one.
fn failure(answer: &[ServerMsg]) -> Option<(ErrorCode, String)> {
    answer.iter().find_map(|message| match message {
        ServerMsg::Error { code, message, .. } => Some((*code, message.clone())),
        _ => None,
    })
}

/// Checks one case against what the server answered.
fn check(case: &Case, answer: &[ServerMsg]) -> Result<(), String> {
    match (&case.expected, failure(answer)) {
        (Expected::Ok, None) => Ok(()),
        (Expected::Ok, Some((code, message))) => {
            Err(format!("expected no error, got {code}: {message}"))
        }
        (Expected::Error(want), Some((got, message))) => match *want == got {
            true => Ok(()),
            false => Err(format!("expected {want}, got {got}: {message}")),
        },
        (Expected::Error(want), None) => Err(format!("expected {want}, the statement succeeded")),
        (Expected::Rows(want), None) => {
            let got = render(answer);
            match got == *want {
                true => Ok(()),
                false => Err(format!("expected rows\n{want}but got\n{got}")),
            }
        }
        (Expected::Rows(_), Some((code, message))) => {
            Err(format!("expected rows, got {code}: {message}"))
        }
    }
}

/// Runs one case file against a server of its own, so every file starts from
/// an empty server. Reports every failing case, not only the first.
pub fn run_file(path: &Path) {
    let shown = path.display();
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{shown}: {e}"));
    let cases = read_cases(&text).unwrap_or_else(|e| panic!("{shown}: {e}"));
    assert!(!cases.is_empty(), "{shown} holds no case");

    let server = Server::start();
    let mut conn = server.connect();
    conn.start_up();
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|case| {
            let answer = conn.run(&case.sql);
            check(case, &answer)
                .err()
                .map(|report| format!("{shown}:{} {}\n    {report}", case.line, case.sql))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} cases failed\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_reads_its_three_case_forms() {
        let text = "\
# a comment, and a blank line below it

statement ok
CREATE TABLE t (a INTEGER PRIMARY KEY)

statement error DUPLICATE_KEY
INSERT INTO t (a) VALUES (1), (1)

query
SELECT a FROM t
ORDER BY a
----
1 | x
(1 row)
";
        assert_eq!(
            read_cases(text).unwrap(),
            vec![
                Case {
                    line: 3,
                    sql: "CREATE TABLE t (a INTEGER PRIMARY KEY)".to_string(),
                    expected: Expected::Ok,
                },
                Case {
                    line: 6,
                    sql: "INSERT INTO t (a) VALUES (1), (1)".to_string(),
                    expected: Expected::Error(ErrorCode::DuplicateKey),
                },
                Case {
                    line: 9,
                    sql: "SELECT a FROM t\nORDER BY a".to_string(),
                    expected: Expected::Rows("1 | x\n(1 row)\n".to_string()),
                },
            ]
        );
    }

    #[test]
    fn a_file_that_is_not_a_file_of_cases_is_an_error() {
        let cases = [
            ("select 1", "is not a case header"),
            ("statement maybe\nSELECT a FROM t", "is not a case header"),
            (
                "statement error NO_SUCH_CODE\nSELECT a FROM t",
                "unknown error code",
            ),
            ("statement ok\n", "holds no statement"),
            ("query\nSELECT a FROM t\n", "needs a ---- line"),
        ];
        for (text, reason) in cases {
            let e = read_cases(text).unwrap_err();
            assert!(e.contains(reason), "for {text:?}: {e}");
        }
    }

    #[test]
    fn every_error_code_reads_from_its_wire_name() {
        assert_eq!(error_code("UNKNOWN_TABLE"), Some(ErrorCode::UnknownTable));
        assert_eq!(error_code("unknown_table"), None, "the wire name is upper");
        assert_eq!(error_code("NOPE"), None);
    }

    #[test]
    fn an_answer_renders_its_rows_and_its_count() {
        let answer = [
            ServerMsg::DataRow {
                values: vec![Value::Integer(1), Value::Text("ada".to_string())],
            },
            ServerMsg::DataRow {
                values: vec![Value::Null, Value::Boolean(true)],
            },
        ];
        assert_eq!(render(&answer), "1 | ada\nNULL | true\n(2 rows)\n");
        assert_eq!(render(&answer[..1]), "1 | ada\n(1 row)\n");
        assert_eq!(render(&[]), "(0 rows)\n");
    }

    #[test]
    fn a_case_checks_against_what_the_server_answered() {
        let error = |code| ServerMsg::Error {
            code,
            message: "no".to_string(),
            position: None,
        };
        let case = |expected| Case {
            line: 1,
            sql: "SELECT a FROM t".to_string(),
            expected,
        };

        assert!(check(&case(Expected::Ok), &[]).is_ok());
        assert!(check(&case(Expected::Ok), &[error(ErrorCode::UnknownTable)]).is_err());

        let wanted = Expected::Error(ErrorCode::UnknownTable);
        assert!(check(&case(wanted), &[error(ErrorCode::UnknownTable)]).is_ok());
        let wanted = Expected::Error(ErrorCode::UnknownTable);
        assert!(check(&case(wanted), &[error(ErrorCode::SyntaxError)]).is_err());
        let wanted = Expected::Error(ErrorCode::UnknownTable);
        assert!(check(&case(wanted), &[]).is_err());

        let rows = Expected::Rows("(0 rows)\n".to_string());
        assert!(check(&case(rows), &[]).is_ok());
        let rows = Expected::Rows("1\n(1 row)\n".to_string());
        assert!(check(&case(rows), &[]).is_err());
        let rows = Expected::Rows("(0 rows)\n".to_string());
        assert!(check(&case(rows), &[error(ErrorCode::UnknownTable)]).is_err());
    }
}
