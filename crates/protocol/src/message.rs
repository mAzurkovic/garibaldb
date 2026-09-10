//! The messages that a client and the server exchange.
//!
//! One message is one JSON object on one line. A `type` field names the
//! variant, so a reader knows the shape before it reads the rest. The flow of a
//! statement is `Query` → `RowDesc` → `DataRow`... → `Complete` → `Ready`.

use serde::{Deserialize, Serialize};

use crate::error::{DbError, ErrorCode};
use crate::value::{DataType, Value};

/// The version that `Startup` carries. The server refuses a version it does
/// not know. `[FR61]`
pub const PROTOCOL_VERSION: u16 = 1;

/// A message from the client to the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClientMsg {
    /// The first message. `database` names the database. `[FR62]`
    Startup { version: u16, database: String },
    /// One SQL statement.
    Query { sql: String },
    /// Stops the statement that runs on another connection. `[FR66]`
    ///
    /// The client sends this on a second connection, with the id and the
    /// secret that `Ready` gave.
    Cancel { conn_id: u64, secret: String },
    /// The client leaves. The server rolls back an open transaction. `[FR65]`
    Close,
}

/// The transaction state that `Ready` reports. The CLI shows it. `[FR80]`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TxState {
    /// No transaction is open.
    None,
    Open,
    ReadOnly,
}

/// One column of a result. `[FR74]`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnDesc {
    pub name: String,
    pub ty: DataType,
}

/// A message from the server to the client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerMsg {
    /// The server waits for the next statement.
    Ready {
        conn_id: u64,
        secret: String,
        tx: TxState,
    },
    /// The columns of the rows that follow.
    RowDesc { cols: Vec<ColumnDesc> },
    /// One row. `[FR75]`
    DataRow { values: Vec<Value> },
    /// The statement ended. `kind` is the statement kind, such as `SELECT`.
    Complete { kind: String, rows: u64 },
    /// The statement failed. The three fields mirror [`DbError`]. `[FR67]`
    Error {
        code: ErrorCode,
        message: String,
        position: Option<u32>,
    },
}

impl From<DbError> for ServerMsg {
    fn from(e: DbError) -> Self {
        ServerMsg::Error {
            code: e.code,
            message: e.message,
            position: e.position,
        }
    }
}

impl ClientMsg {
    /// The message as one line of JSON, with no newline in it.
    pub fn to_line(&self) -> String {
        to_line(self)
    }

    /// Reads one line of JSON.
    pub fn from_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

impl ServerMsg {
    /// The message as one line of JSON, with no newline in it.
    pub fn to_line(&self) -> String {
        to_line(self)
    }

    /// Reads one line of JSON.
    pub fn from_line(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

/// Writes JSON. `serde_json` escapes a newline, so the text holds one line.
///
/// A message holds only strings, numbers, and lists of them, so the write
/// cannot fail.
fn to_line<T: Serialize>(msg: &T) -> String {
    serde_json::to_string(msg).expect("a message always writes as JSON")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Decimal;

    fn client_msgs() -> Vec<(ClientMsg, &'static str)> {
        vec![
            (
                ClientMsg::Startup {
                    version: PROTOCOL_VERSION,
                    database: "shop".to_string(),
                },
                "startup",
            ),
            (
                ClientMsg::Query {
                    sql: "SELECT 1".to_string(),
                },
                "query",
            ),
            (
                ClientMsg::Cancel {
                    conn_id: 7,
                    secret: "s3cret".to_string(),
                },
                "cancel",
            ),
            (ClientMsg::Close, "close"),
        ]
    }

    fn server_msgs() -> Vec<(ServerMsg, &'static str)> {
        vec![
            (
                ServerMsg::Ready {
                    conn_id: 7,
                    secret: "s3cret".to_string(),
                    tx: TxState::Open,
                },
                "ready",
            ),
            (
                ServerMsg::RowDesc {
                    cols: vec![ColumnDesc {
                        name: "price".to_string(),
                        ty: DataType::Decimal { p: 10, s: 2 },
                    }],
                },
                "rowdesc",
            ),
            (
                ServerMsg::DataRow {
                    values: vec![
                        Value::Integer(1),
                        Value::Text("x".to_string()),
                        Value::Decimal(Decimal {
                            units: 1220,
                            scale: 2,
                        }),
                        Value::Null,
                    ],
                },
                "datarow",
            ),
            (
                ServerMsg::Complete {
                    kind: "SELECT".to_string(),
                    rows: 3,
                },
                "complete",
            ),
            (
                ServerMsg::Error {
                    code: ErrorCode::SyntaxError,
                    message: "unexpected token".to_string(),
                    position: Some(12),
                },
                "error",
            ),
        ]
    }

    #[test]
    fn a_client_message_carries_its_type_tag() {
        for (msg, tag) in client_msgs() {
            let json: serde_json::Value = serde_json::from_str(&msg.to_line()).unwrap();
            assert_eq!(json["type"], tag, "{msg:?}");
        }
    }

    #[test]
    fn a_server_message_carries_its_type_tag() {
        for (msg, tag) in server_msgs() {
            let json: serde_json::Value = serde_json::from_str(&msg.to_line()).unwrap();
            assert_eq!(json["type"], tag, "{msg:?}");
        }
    }

    #[test]
    fn a_line_holds_no_newline() {
        let msgs = vec![
            ClientMsg::Query {
                sql: "SELECT\n1".to_string(),
            },
            ClientMsg::Close,
        ];
        for msg in msgs {
            assert!(!msg.to_line().contains('\n'), "{msg:?}");
        }
        let msg = ServerMsg::Error {
            code: ErrorCode::SyntaxError,
            message: "two\nlines".to_string(),
            position: None,
        };
        assert!(!msg.to_line().contains('\n'));
    }

    #[test]
    fn a_message_reads_back_from_its_line() {
        for (msg, _) in client_msgs() {
            assert_eq!(ClientMsg::from_line(&msg.to_line()).unwrap(), msg);
        }
        for (msg, _) in server_msgs() {
            assert_eq!(ServerMsg::from_line(&msg.to_line()).unwrap(), msg);
        }
    }

    #[test]
    fn an_unknown_type_fails_to_read() {
        assert!(ClientMsg::from_line(r#"{"type":"hello"}"#).is_err());
        assert!(ServerMsg::from_line(r#"{"type":"hello"}"#).is_err());
        // The tag of the other direction is unknown too.
        assert!(ClientMsg::from_line(r#"{"type":"ready"}"#).is_err());
    }

    #[test]
    fn a_missing_field_fails_to_read() {
        assert!(ClientMsg::from_line(r#"{"type":"startup","version":1}"#).is_err());
        assert!(ClientMsg::from_line(r#"{"type":"query"}"#).is_err());
        assert!(ServerMsg::from_line(r#"{"type":"complete","kind":"SELECT"}"#).is_err());
    }

    #[test]
    fn a_line_with_no_type_fails_to_read() {
        assert!(ClientMsg::from_line(r#"{"sql":"SELECT 1"}"#).is_err());
    }

    #[test]
    fn a_transaction_state_travels_as_a_lowercase_name() {
        let cases = [
            (TxState::None, "\"none\""),
            (TxState::Open, "\"open\""),
            (TxState::ReadOnly, "\"readonly\""),
        ];
        for (tx, json) in cases {
            assert_eq!(serde_json::to_string(&tx).unwrap(), json);
            assert_eq!(serde_json::from_str::<TxState>(json).unwrap(), tx);
        }
    }

    #[test]
    fn a_db_error_becomes_the_error_variant() {
        let e = DbError {
            code: ErrorCode::UnknownTable,
            message: "no table \"t\"".to_string(),
            position: Some(7),
        };
        assert_eq!(
            ServerMsg::from(e),
            ServerMsg::Error {
                code: ErrorCode::UnknownTable,
                message: "no table \"t\"".to_string(),
                position: Some(7),
            }
        );
    }

    #[test]
    fn a_db_error_without_a_position_keeps_the_empty_position() {
        let e = DbError {
            code: ErrorCode::StorageFull,
            message: "disk is full".to_string(),
            position: None,
        };
        assert_eq!(
            ServerMsg::from(e),
            ServerMsg::Error {
                code: ErrorCode::StorageFull,
                message: "disk is full".to_string(),
                position: None,
            }
        );
    }

    #[test]
    fn the_protocol_version_is_one() {
        assert_eq!(PROTOCOL_VERSION, 1);
    }
}
