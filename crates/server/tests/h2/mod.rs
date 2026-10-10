//! The concurrency runner: several connections, one scripted order.
//!
//! A statement is sent and its answer read in two steps, so a client can be
//! left waiting on a lock while another client runs. That is what makes an
//! interleaving repeat: the script says who acts, and nothing races.

use protocol::{ClientMsg, ErrorCode, ServerMsg, Value};

use crate::h1::{Conn, Server, render};

/// A set of connections to one server, each started up on its database.
pub struct Clients {
    conns: Vec<Conn>,
}

impl Clients {
    pub fn open(server: &Server, count: usize) -> Clients {
        let conns = (0..count)
            .map(|_| {
                let mut conn = server.connect();
                conn.start_up();
                conn
            })
            .collect();
        Clients { conns }
    }

    /// Sends a statement and leaves its answer unread.
    pub fn send(&mut self, client: usize, sql: &str) {
        self.conns[client].send(&ClientMsg::Query {
            sql: sql.to_string(),
        });
    }

    /// Reads the answer of the statement a client sent, waiting as long as it
    /// takes.
    pub fn answer(&mut self, client: usize) -> Vec<ServerMsg> {
        let conn = &mut self.conns[client];
        let mut answer = Vec::new();
        loop {
            let message = conn.expect();
            let done = matches!(message, ServerMsg::Ready { .. });
            answer.push(message);
            if done {
                return answer;
            }
        }
    }

    /// Sends a statement and reads its answer.
    pub fn run(&mut self, client: usize, sql: &str) -> Vec<ServerMsg> {
        self.send(client, sql);
        self.answer(client)
    }

    /// Runs a statement that must not fail.
    pub fn ok(&mut self, client: usize, sql: &str) {
        let answer = self.run(client, sql);
        assert_eq!(failure(&answer), None, "client {client}: {sql}");
    }

    /// Runs a statement that must fail, and gives back its code.
    pub fn error(&mut self, client: usize, sql: &str) -> ErrorCode {
        let answer = self.run(client, sql);
        failure(&answer)
            .unwrap_or_else(|| panic!("client {client}: {sql} was expected to fail"))
            .0
    }

    /// The rows of a read, rendered the way a case file writes them.
    pub fn rows(&mut self, client: usize, sql: &str) -> String {
        let answer = self.run(client, sql);
        assert_eq!(failure(&answer), None, "client {client}: {sql}");
        render(&answer)
    }

    /// The one integer a read of one row and one column gives back.
    pub fn number(&mut self, client: usize, sql: &str) -> i64 {
        let answer = self.run(client, sql);
        assert_eq!(failure(&answer), None, "client {client}: {sql}");
        let mut found = answer.iter().filter_map(|msg| match msg {
            ServerMsg::DataRow { values } => match values.as_slice() {
                [Value::Integer(n)] => Some(*n),
                other => panic!("client {client}: {sql} gave {other:?}"),
            },
            _ => None,
        });
        let only = found.next().expect("one row");
        assert_eq!(found.next(), None, "client {client}: {sql} gave two rows");
        only
    }
}

/// The error of an answer, when it holds one.
pub fn failure(answer: &[ServerMsg]) -> Option<(ErrorCode, String)> {
    answer.iter().find_map(|message| match message {
        ServerMsg::Error { code, message, .. } => Some((*code, message.clone())),
        _ => None,
    })
}
