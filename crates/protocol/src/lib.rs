//! The contract between the server and the CLI.
//!
//! This crate holds message types, values, and error codes. It has no file I/O
//! and no socket, so the CLI never depends on the storage engine.

pub mod error;
pub mod message;
pub mod value;

pub use error::{DbError, ErrorCode};
pub use message::{ClientMsg, ColumnDesc, PROTOCOL_VERSION, ServerMsg, TxState};
pub use value::{DataType, Decimal, MAX_PRECISION, Value};
