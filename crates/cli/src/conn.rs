//! The socket to the server, and the second socket that stops a statement.
//!
//! Framing is one JSON object for each line, which is what the server speaks.
//! See [FR71], [FR75], and [FR79].

use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};

use protocol::{ClientMsg, DbError, PROTOCOL_VERSION, ServerMsg};

/// What a second connection needs to stop the statement of the first.
///
/// It owns everything it needs, so the thread that answers Ctrl-C holds one
/// while the main thread still reads the answer. See [FR79].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelKey {
    addr: SocketAddr,
    conn_id: u64,
    secret: String,
}

impl CancelKey {
    /// Opens a second socket and asks the server to stop the statement. The
    /// server serves a `Cancel` before any handshake, so this writes one line
    /// and closes.
    pub fn send(&self) -> io::Result<()> {
        let mut stream = TcpStream::connect(self.addr)?;
        let msg = ClientMsg::Cancel {
            conn_id: self.conn_id,
            secret: self.secret.clone(),
        };
        stream.write_all(format!("{}\n", msg.to_line()).as_bytes())
    }
}

/// Why a connection never started up.
#[derive(Debug)]
pub enum StartupError {
    /// The socket failed.
    Io(io::Error),
    /// The server answered an error instead of a `Ready`.
    Refused(DbError),
    /// The server answered something that is not a `Ready`.
    Unexpected(ServerMsg),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartupError::Io(e) => write!(f, "{e}"),
            StartupError::Refused(e) => write!(f, "{e}"),
            StartupError::Unexpected(msg) => write!(f, "the server answered {msg:?}, not a ready"),
        }
    }
}

impl From<io::Error> for StartupError {
    fn from(e: io::Error) -> Self {
        StartupError::Io(e)
    }
}

/// One connection to the server.
pub struct Connection {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Connection {
    /// Opens the socket. The reader and the writer hold their own handle, so
    /// a read and a write never wait for one another.
    pub fn connect(host: &str, port: u16) -> io::Result<Connection> {
        let stream = TcpStream::connect((host, port))?;
        Ok(Connection {
            writer: stream.try_clone()?,
            reader: BufReader::new(stream),
        })
    }

    /// Names the database and waits for the first `Ready`. The key it returns
    /// stops a statement later. See [FR71].
    pub fn startup(&mut self, database: &str) -> Result<CancelKey, StartupError> {
        self.send(&ClientMsg::Startup {
            version: PROTOCOL_VERSION,
            database: database.to_string(),
        })?;
        match read_message(&mut self.reader)? {
            Some(ServerMsg::Ready {
                conn_id, secret, ..
            }) => Ok(CancelKey {
                addr: self.writer.peer_addr()?,
                conn_id,
                secret,
            }),
            Some(ServerMsg::Error {
                code,
                message,
                position,
            }) => Err(StartupError::Refused(DbError {
                code,
                message,
                position,
            })),
            Some(other) => Err(StartupError::Unexpected(other)),
            None => Err(StartupError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the server closed the connection",
            ))),
        }
    }

    /// Sends one statement. The answer arrives one message at a time, so a row
    /// reaches the screen before the next one arrives. See [FR75].
    pub fn query(&mut self, sql: &str) -> io::Result<Answer<'_>> {
        self.send(&ClientMsg::Query {
            sql: sql.to_string(),
        })?;
        Ok(Answer {
            reader: &mut self.reader,
            done: false,
        })
    }

    /// Tells the server that the client leaves.
    pub fn close(&mut self) -> io::Result<()> {
        self.send(&ClientMsg::Close)
    }

    fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        self.writer
            .write_all(format!("{}\n", msg.to_line()).as_bytes())
    }
}

/// The answer to one statement. It ends after the `Ready` that closes the
/// statement, so the next statement starts on a clean socket.
pub struct Answer<'a> {
    reader: &'a mut BufReader<TcpStream>,
    done: bool,
}

impl Iterator for Answer<'_> {
    type Item = io::Result<ServerMsg>;

    fn next(&mut self) -> Option<io::Result<ServerMsg>> {
        if self.done {
            return None;
        }
        let item = match read_message(self.reader) {
            Ok(Some(msg)) => {
                self.done = matches!(msg, ServerMsg::Ready { .. });
                Ok(msg)
            }
            Ok(None) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the server closed the connection",
            )),
            Err(e) => Err(e),
        };
        self.done |= item.is_err();
        Some(item)
    }
}

/// Reads one message, or `None` at the end of the connection.
fn read_message(reader: &mut impl BufRead) -> io::Result<Option<ServerMsg>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    ServerMsg::from_line(line.trim_end())
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad message: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{ErrorCode, TxState};

    fn ready() -> ServerMsg {
        ServerMsg::Ready {
            conn_id: 1,
            secret: "42".to_string(),
            tx: TxState::None,
        }
    }

    #[test]
    fn a_message_reads_from_its_line() {
        let line = format!("{}\n", ready().to_line());
        assert_eq!(read_message(&mut line.as_bytes()).unwrap(), Some(ready()));
    }

    #[test]
    fn an_empty_reader_is_the_end_of_the_connection() {
        assert_eq!(read_message(&mut &b""[..]).unwrap(), None);
    }

    #[test]
    fn a_line_that_is_not_a_message_is_an_error() {
        let e = read_message(&mut &b"hello\n"[..]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_startup_error_shows_the_code_and_the_message() {
        let e = StartupError::Refused(DbError {
            code: ErrorCode::TooManyConnections,
            message: "the server is full".to_string(),
            position: None,
        });
        let shown = e.to_string();
        assert!(shown.contains("TOO_MANY_CONNECTIONS"), "{shown}");
        assert!(shown.contains("the server is full"), "{shown}");
    }

    #[test]
    fn an_io_startup_error_shows_the_reason() {
        let e = StartupError::from(io::Error::other("no route"));
        assert_eq!(e.to_string(), "no route");
    }

    #[test]
    fn an_unexpected_answer_names_itself() {
        let e = StartupError::Unexpected(ServerMsg::Complete {
            kind: "SELECT".to_string(),
            rows: 0,
        });
        assert!(e.to_string().contains("not a ready"), "{e}");
    }
}
