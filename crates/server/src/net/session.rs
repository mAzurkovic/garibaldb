//! One session for one connection: the handshake, the message loop, and the
//! disconnect. See [FR62], [FR64], [FR65], and [FR66].
//!
//! Framing is one JSON object for each line. Reads are buffered, and each reply
//! goes straight to the socket, because the client waits for it.

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;

use protocol::error::ErrorCode;
use protocol::message::{ClientMsg, PROTOCOL_VERSION, ServerMsg, TxState};

use crate::net::cancel::{CancelHandle, CancelRegistry, new_secret};
use crate::sql::parser;

/// One connection after its handshake. See [FR64].
pub struct Session {
    pub conn_id: u64,
    /// The secret of this connection. It reaches the wire as a string, because
    /// a JSON number loses digits above 2^53.
    pub secret: u64,
    /// The database that `Startup` named. Milestone 5 opens it.
    pub database: String,
    /// The flag that a `Cancel` on a second connection sets. See [FR66].
    /// The flag that a `Cancel` on a second connection sets. The operators
    /// read it between rows in milestone 11, so nothing reads it before then.
    #[allow(dead_code)]
    pub cancel: CancelHandle,
}

impl Session {
    /// Registers the connection and takes its cancel flag.
    fn start(conn_id: u64, database: String, cancels: &CancelRegistry) -> Session {
        let secret = new_secret();
        Session {
            conn_id,
            secret,
            database,
            cancel: cancels.register(conn_id, secret),
        }
    }

    /// The `Ready` that follows every statement. Milestone 10 reports a
    /// transaction state other than `None`.
    fn ready(&self) -> ServerMsg {
        ServerMsg::Ready {
            conn_id: self.conn_id,
            secret: self.secret.to_string(),
            tx: TxState::None,
        }
    }

    /// Answers each message until `Close`, a write error, a read error, or EOF.
    fn run(&self, stream: &TcpStream, lines: impl Iterator<Item = io::Result<String>>) {
        for line in lines {
            // A read error ends the session, like EOF.
            let Ok(line) = line else { break };
            let Some(answer) = answer(&line) else { break };
            if self.reply(stream, &answer).is_err() || self.reply(stream, &self.ready()).is_err() {
                break;
            }
        }
    }

    /// Frees the connection. The listener logs the close, because the line
    /// must follow the slot that the connection held. See [FR65].
    fn on_disconnect(&mut self, cancels: &CancelRegistry) {
        self.rollback_open_transaction();
        cancels.unregister(self.conn_id);
    }

    /// [FR65] rolls back the open transaction here. Milestone 10 adds the
    /// transaction, so this milestone has nothing to roll back.
    fn rollback_open_transaction(&mut self) {}

    /// Writes one message. An error reply also reaches the log. See [FR82].
    fn reply(&self, stream: &TcpStream, msg: &ServerMsg) -> io::Result<()> {
        if let ServerMsg::Error { code, message, .. } = msg {
            log::error!("connection {}: {code}: {message}", self.conn_id);
        }
        send(stream, msg)
    }
}

/// Reads the first message, then runs the loop. The connection closes when
/// this returns.
pub fn serve(stream: &TcpStream, conn_id: u64, cancels: &CancelRegistry) {
    let mut lines = BufReader::new(stream).lines();
    let first = match lines.next() {
        Some(Ok(line)) => first_message(&line),
        // EOF or a read error: the client left before it said anything.
        _ => return,
    };
    let database = match first {
        First::Start { database } => database,
        // A cancel connection never starts up, so it holds no session.
        First::Cancel { target, secret } => {
            let found = cancels.cancel(target, secret);
            log::info!("connection {conn_id} cancels {target}: match {found}");
            return;
        }
        First::Reject(error) => {
            if let ServerMsg::Error { code, message, .. } = &error {
                log::error!("connection {conn_id}: {code}: {message}");
            }
            let _ = send(stream, &error);
            return;
        }
    };
    let mut session = Session::start(conn_id, database, cancels);
    log::info!(
        "connection {conn_id} starts up on database {}",
        session.database
    );
    if session.reply(stream, &session.ready()).is_ok() {
        session.run(stream, &mut lines);
    }
    session.on_disconnect(cancels);
}

/// What the first line of a connection asks for. See [FR62] and [FR66].
#[derive(Debug, PartialEq)]
enum First {
    /// A `Startup` that this server answers.
    Start { database: String },
    /// A `Cancel` for another connection.
    Cancel { target: u64, secret: u64 },
    /// The error to write before the connection closes.
    Reject(ServerMsg),
}

/// Reads the first line. Only `Startup` and `Cancel` stand here. The database
/// name travels as it comes, because databases arrive in milestone 5.
fn first_message(line: &str) -> First {
    match ClientMsg::from_line(line) {
        Ok(ClientMsg::Startup { version, database }) if version == PROTOCOL_VERSION => {
            First::Start { database }
        }
        Ok(ClientMsg::Startup { version, .. }) => First::Reject(error(
            ErrorCode::SyntaxError,
            format!("the server speaks protocol version {PROTOCOL_VERSION}, not {version}"),
        )),
        Ok(ClientMsg::Cancel { conn_id, secret }) => match secret.parse() {
            Ok(secret) => First::Cancel {
                target: conn_id,
                secret,
            },
            Err(_) => First::Reject(error(ErrorCode::SyntaxError, "the secret is not a number")),
        },
        Ok(_) => First::Reject(error(
            ErrorCode::SyntaxError,
            "the first message must be startup or cancel",
        )),
        Err(e) => First::Reject(error(
            ErrorCode::SyntaxError,
            format!("malformed message: {e}"),
        )),
    }
}

/// The answer to one line of a started session. `None` means the client leaves.
/// The caller writes `Ready` after the answer.
///
/// A statement that parses answers `UNKNOWN_TABLE`, because the catalog
/// arrives in milestone 5 and the executor in milestone 8.
fn answer(line: &str) -> Option<ServerMsg> {
    Some(match ClientMsg::from_line(line) {
        Ok(ClientMsg::Close) => return None,
        Ok(ClientMsg::Query { sql }) => match parser::parse(&sql) {
            // The statement is understood. Milestone 8 runs it.
            Ok(_statement) => error(ErrorCode::UnknownTable, "the server holds no table"),
            Err(e) => ServerMsg::from(e),
        },
        Ok(ClientMsg::Startup { .. }) => {
            error(ErrorCode::SyntaxError, "the connection already started up")
        }
        Ok(ClientMsg::Cancel { .. }) => {
            error(ErrorCode::SyntaxError, "a cancel needs a second connection")
        }
        // A malformed line is an error, and the connection stays open.
        Err(e) => error(ErrorCode::SyntaxError, format!("malformed message: {e}")),
    })
}

/// One error message with no position. A position needs a statement, which
/// milestone 4 parses.
fn error(code: ErrorCode, message: impl Into<String>) -> ServerMsg {
    ServerMsg::Error {
        code,
        message: message.into(),
        position: None,
    }
}

/// Writes one message and its newline. `TcpStream` holds no buffer, so the
/// line reaches the client at once, and the client that waits for it reads it.
pub fn send(mut stream: &TcpStream, msg: &ServerMsg) -> io::Result<()> {
    stream.write_all(format!("{}\n", msg.to_line()).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code_of(msg: &ServerMsg) -> ErrorCode {
        match msg {
            ServerMsg::Error { code, .. } => *code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn a_startup_with_the_known_version_starts_the_session() {
        let line = ClientMsg::Startup {
            version: PROTOCOL_VERSION,
            database: "shop".to_string(),
        }
        .to_line();
        assert_eq!(
            first_message(&line),
            First::Start {
                database: "shop".to_string()
            }
        );
    }

    #[test]
    fn a_startup_with_another_version_is_rejected() {
        let line = ClientMsg::Startup {
            version: 0,
            database: "shop".to_string(),
        }
        .to_line();
        let First::Reject(msg) = first_message(&line) else {
            panic!("expected a reject");
        };
        assert_eq!(code_of(&msg), ErrorCode::SyntaxError);
    }

    #[test]
    fn a_database_name_travels_as_it_comes() {
        let line = ClientMsg::Startup {
            version: PROTOCOL_VERSION,
            database: "no such database".to_string(),
        }
        .to_line();
        assert_eq!(
            first_message(&line),
            First::Start {
                database: "no such database".to_string()
            }
        );
    }

    #[test]
    fn a_cancel_stands_as_the_first_message() {
        let line = ClientMsg::Cancel {
            conn_id: 7,
            secret: "18446744073709551615".to_string(),
        }
        .to_line();
        assert_eq!(
            first_message(&line),
            First::Cancel {
                target: 7,
                secret: u64::MAX
            }
        );
    }

    #[test]
    fn a_cancel_with_a_secret_that_is_not_a_number_is_rejected() {
        let line = ClientMsg::Cancel {
            conn_id: 7,
            secret: "abc".to_string(),
        }
        .to_line();
        let First::Reject(msg) = first_message(&line) else {
            panic!("expected a reject");
        };
        assert_eq!(code_of(&msg), ErrorCode::SyntaxError);
    }

    #[test]
    fn a_query_as_the_first_message_is_rejected() {
        let line = ClientMsg::Query {
            sql: "SELECT 1".to_string(),
        }
        .to_line();
        let First::Reject(msg) = first_message(&line) else {
            panic!("expected a reject");
        };
        assert_eq!(code_of(&msg), ErrorCode::SyntaxError);
    }

    #[test]
    fn a_close_as_the_first_message_is_rejected() {
        assert!(matches!(
            first_message(&ClientMsg::Close.to_line()),
            First::Reject(_)
        ));
    }

    #[test]
    fn a_malformed_first_line_is_rejected_and_panics_nothing() {
        for line in ["", "{", "null", "[1,2]", r#"{"type":"hello"}"#, "\u{0}"] {
            let First::Reject(msg) = first_message(line) else {
                panic!("expected a reject for {line:?}");
            };
            assert_eq!(code_of(&msg), ErrorCode::SyntaxError);
        }
    }

    fn query(sql: &str) -> String {
        ClientMsg::Query {
            sql: sql.to_string(),
        }
        .to_line()
    }

    #[test]
    fn a_statement_that_parses_answers_unknown_table() {
        for sql in [
            "SELECT * FROM t",
            "INSERT INTO t (a) VALUES (1)",
            "CREATE TABLE t (a INTEGER PRIMARY KEY)",
            "BEGIN READ ONLY",
            "COMMIT",
        ] {
            assert_eq!(
                code_of(&answer(&query(sql)).unwrap()),
                ErrorCode::UnknownTable,
                "for {sql}"
            );
        }
    }

    #[test]
    fn a_statement_that_does_not_parse_answers_a_syntax_error_and_its_position() {
        let ServerMsg::Error {
            code,
            message,
            position,
        } = answer(&query("SELECT a FROM")).unwrap()
        else {
            panic!("expected an error");
        };
        assert_eq!(code, ErrorCode::SyntaxError);
        assert_eq!(position, Some(14));
        assert!(message.contains("a name"), "{message}");
    }

    #[test]
    fn a_statement_the_grammar_has_no_form_for_answers_a_syntax_error() {
        for sql in [
            "SELECT 1",
            "",
            "DROP TABLE IF EXISTS t",
            "SELECT a + 1 FROM t",
        ] {
            assert_eq!(
                code_of(&answer(&query(sql)).unwrap()),
                ErrorCode::SyntaxError,
                "for {sql:?}"
            );
        }
    }

    #[test]
    fn a_close_leaves_the_loop() {
        assert_eq!(answer(&ClientMsg::Close.to_line()), None);
    }

    #[test]
    fn a_second_startup_is_an_error() {
        let line = ClientMsg::Startup {
            version: PROTOCOL_VERSION,
            database: "shop".to_string(),
        }
        .to_line();
        assert_eq!(code_of(&answer(&line).unwrap()), ErrorCode::SyntaxError);
    }

    #[test]
    fn a_cancel_in_a_started_session_is_an_error() {
        let line = ClientMsg::Cancel {
            conn_id: 1,
            secret: "1".to_string(),
        }
        .to_line();
        assert_eq!(code_of(&answer(&line).unwrap()), ErrorCode::SyntaxError);
    }

    #[test]
    fn a_malformed_line_is_an_error_and_panics_nothing() {
        for line in ["", "{", "null", r#"{"type":"query"}"#] {
            assert_eq!(
                code_of(&answer(line).unwrap()),
                ErrorCode::SyntaxError,
                "for {line:?}"
            );
        }
    }

    #[test]
    fn ready_carries_the_id_the_secret_and_no_transaction() {
        let session = Session {
            conn_id: 3,
            secret: u64::MAX,
            database: "shop".to_string(),
            cancel: CancelHandle::new(),
        };
        assert_eq!(
            session.ready(),
            ServerMsg::Ready {
                conn_id: 3,
                secret: "18446744073709551615".to_string(),
                tx: TxState::None,
            }
        );
    }

    #[test]
    fn a_session_registers_its_secret_so_a_cancel_finds_it() {
        let cancels = CancelRegistry::new();
        let session = Session::start(5, "shop".to_string(), &cancels);
        assert!(cancels.cancel(5, session.secret));
        assert!(session.cancel.stopped());
    }

    #[test]
    fn a_disconnect_unregisters_the_connection() {
        let cancels = CancelRegistry::new();
        let mut session = Session::start(5, "shop".to_string(), &cancels);
        let secret = session.secret;
        session.on_disconnect(&cancels);
        assert!(!cancels.cancel(5, secret));
    }
}
