//! One session for one connection: the handshake, the message loop, and the
//! disconnect. See [FR62], [FR64], [FR65], and [FR66].
//!
//! Framing is one JSON object for each line. Reads are buffered, and each reply
//! goes straight to the socket, because the client waits for it.

use std::io::{self, BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;

use protocol::error::ErrorCode;
use protocol::message::{ClientMsg, ColumnDesc, PROTOCOL_VERSION, ServerMsg, TxState};

use crate::catalog::registry::{Connected, Registry};
use crate::exec::dml::{self, Answer};
use crate::net::cancel::{CancelHandle, CancelRegistry, new_secret};
use crate::sql::parser;

/// One connection after its handshake. See [FR64].
pub struct Session {
    pub conn_id: u64,
    /// The secret of this connection. It reaches the wire as a string, because
    /// a JSON number loses digits above 2^53.
    pub secret: u64,
    /// The one database this connection sees. See [FR5]. Dropping it frees
    /// the database for a `DROP DATABASE`.
    pub held: Connected,
    /// The flag that a `Cancel` on a second connection sets. The operators
    /// read it between rows in milestone 11, so nothing reads it before then.
    #[allow(dead_code)]
    pub cancel: CancelHandle,
}

impl Session {
    /// Registers the connection and takes its cancel flag.
    fn start(conn_id: u64, held: Connected, cancels: &CancelRegistry) -> Session {
        let secret = new_secret();
        Session {
            conn_id,
            secret,
            held,
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
    fn run(
        &self,
        stream: &TcpStream,
        lines: impl Iterator<Item = io::Result<String>>,
        registry: &Registry,
    ) {
        for line in lines {
            // A read error ends the session, like EOF.
            let Ok(line) = line else { break };
            let written = match read_line(&line) {
                Asked::Leave => break,
                Asked::Reply(msg) => self.reply(stream, &msg),
                Asked::Statement(sql) => self.run_statement(stream, &sql, registry),
            };
            if written.is_err() || self.reply(stream, &self.ready()).is_err() {
                break;
            }
        }
    }

    /// Reads one statement, runs it, and writes its answer.
    ///
    /// Rows go out as they arrive, so a result larger than the memory of the
    /// server still reaches the client. The error of a row that fails partway
    /// follows the rows that already went.
    fn run_statement(&self, stream: &TcpStream, sql: &str, registry: &Registry) -> io::Result<()> {
        let answer =
            parser::parse(sql).and_then(|statement| dml::run(&statement, &self.held.db, registry));
        let mut plan = match answer {
            Err(e) => return self.reply(stream, &ServerMsg::from(e)),
            Ok(Answer::Changed { kind, rows }) => {
                return self.reply(
                    stream,
                    &ServerMsg::Complete {
                        kind: kind.to_string(),
                        rows,
                    },
                );
            }
            Ok(Answer::Rows(plan)) => plan,
        };
        let cols = plan.schema().iter().map(describe).collect();
        self.reply(stream, &ServerMsg::RowDesc { cols })?;
        let mut rows = 0;
        loop {
            match plan.next() {
                Err(e) => return self.reply(stream, &ServerMsg::from(e)),
                Ok(None) => break,
                Ok(Some(values)) => {
                    rows += 1;
                    self.reply(stream, &ServerMsg::DataRow { values })?;
                }
            }
        }
        self.reply(
            stream,
            &ServerMsg::Complete {
                kind: "SELECT".to_string(),
                rows,
            },
        )
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

/// Reads the first message, opens the database it names, then runs the loop.
/// The connection closes when this returns.
pub fn serve(stream: &TcpStream, conn_id: u64, cancels: &CancelRegistry, registry: &Arc<Registry>) {
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
    // A database that is not there closes the connection, the way a protocol
    // version the server does not speak closes it.
    let held = match Registry::connect(registry, &database) {
        Ok(held) => held,
        Err(e) => {
            log::error!("connection {conn_id}: {e}");
            let _ = send(stream, &ServerMsg::from(e));
            return;
        }
    };
    let mut session = Session::start(conn_id, held, cancels);
    log::info!(
        "connection {conn_id} starts up on database {}",
        session.held.db.name
    );
    if session.reply(stream, &session.ready()).is_ok() {
        session.run(stream, &mut lines, registry);
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

/// Reads the first line. Only `Startup` and `Cancel` stand here.
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

/// What one line of a started session asks for.
#[derive(Debug, PartialEq)]
enum Asked {
    /// The client leaves.
    Leave,
    /// A statement to run.
    Statement(String),
    /// The answer, which needs no database.
    Reply(ServerMsg),
}

/// Reads one line of a started session.
fn read_line(line: &str) -> Asked {
    match ClientMsg::from_line(line) {
        Ok(ClientMsg::Close) => Asked::Leave,
        Ok(ClientMsg::Query { sql }) => Asked::Statement(sql),
        Ok(ClientMsg::Startup { .. }) => Asked::Reply(error(
            ErrorCode::SyntaxError,
            "the connection already started up",
        )),
        Ok(ClientMsg::Cancel { .. }) => Asked::Reply(error(
            ErrorCode::SyntaxError,
            "a cancel needs a second connection",
        )),
        // A malformed line is an error, and the connection stays open.
        Err(e) => Asked::Reply(error(
            ErrorCode::SyntaxError,
            format!("malformed message: {e}"),
        )),
    }
}

/// One column of a result, as the wire names it.
fn describe(column: &crate::catalog::ColumnDef) -> ColumnDesc {
    ColumnDesc {
        name: column.name.clone(),
        ty: column.ty,
    }
}

/// One error message with no position. A position belongs to a statement, and
/// the parser puts it there.
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
    use crate::catalog::testing::Dir;

    fn code_of(msg: &ServerMsg) -> ErrorCode {
        match msg {
            ServerMsg::Error { code, .. } => *code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    fn query(sql: &str) -> String {
        ClientMsg::Query {
            sql: sql.to_string(),
        }
        .to_line()
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
        let First::Reject(msg) = first_message(&query("SELECT * FROM item")) else {
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

    #[test]
    fn a_query_is_a_statement_to_run() {
        assert_eq!(
            read_line(&query("SELECT * FROM item")),
            Asked::Statement("SELECT * FROM item".to_string())
        );
    }

    #[test]
    fn a_close_leaves_the_loop() {
        assert_eq!(read_line(&ClientMsg::Close.to_line()), Asked::Leave);
    }

    #[test]
    fn a_second_startup_is_an_error() {
        let line = ClientMsg::Startup {
            version: PROTOCOL_VERSION,
            database: "shop".to_string(),
        }
        .to_line();
        let Asked::Reply(msg) = read_line(&line) else {
            panic!("expected a reply");
        };
        assert_eq!(code_of(&msg), ErrorCode::SyntaxError);
    }

    #[test]
    fn a_cancel_in_a_started_session_is_an_error() {
        let line = ClientMsg::Cancel {
            conn_id: 1,
            secret: "1".to_string(),
        }
        .to_line();
        let Asked::Reply(msg) = read_line(&line) else {
            panic!("expected a reply");
        };
        assert_eq!(code_of(&msg), ErrorCode::SyntaxError);
    }

    #[test]
    fn a_malformed_line_is_an_error_and_panics_nothing() {
        for line in ["", "{", "null", r#"{"type":"query"}"#] {
            let Asked::Reply(msg) = read_line(line) else {
                panic!("expected a reply for {line:?}");
            };
            assert_eq!(code_of(&msg), ErrorCode::SyntaxError, "for {line:?}");
        }
    }

    #[test]
    fn ready_carries_the_id_the_secret_and_no_transaction() {
        let dir = Dir::new("session-ready");
        let (_registry, held) = dir.shop();
        let cancels = CancelRegistry::new();
        let mut session = Session::start(3, held, &cancels);
        session.secret = u64::MAX;
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
        let dir = Dir::new("session-cancel");
        let (_registry, held) = dir.shop();
        let cancels = CancelRegistry::new();
        let session = Session::start(5, held, &cancels);
        assert!(cancels.cancel(5, session.secret));
        assert!(session.cancel.stopped());
    }

    #[test]
    fn a_disconnect_unregisters_the_connection() {
        let dir = Dir::new("session-disconnect");
        let (_registry, held) = dir.shop();
        let cancels = CancelRegistry::new();
        let mut session = Session::start(5, held, &cancels);
        let secret = session.secret;
        session.on_disconnect(&cancels);
        assert!(!cancels.cancel(5, secret));
    }
}
