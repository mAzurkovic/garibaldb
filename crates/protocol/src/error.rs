//! Error codes and the error type that every fallible function returns.
//!
//! The serde form of an [`ErrorCode`] is its screaming-snake name. The name is
//! the wire contract, so a new code never renumbers an old one.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A stable code for a failure. See [FR68] and [FR70].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    SyntaxError,
    UnknownTable,
    UnknownColumn,
    TypeMismatch,
    DuplicateKey,
    NotNullViolation,
    StorageFull,
    TxnAborted,
    /// A change asked for by a transaction that said it only reads.
    ReadOnlyTxn,
    /// A `BEGIN` on a connection that already holds a transaction.
    TxnAlreadyOpen,
    SchemaChangeInTxn,
    LockTimeout,
    /// A statement that a client stopped while it ran.
    Cancelled,
    TooManyConnections,
    DatabaseExists,
    TableExists,
    DatabaseInUse,
    UnknownDatabase,
}

impl ErrorCode {
    /// Every code. A name reads back through this, so a new code needs no
    /// second table anywhere.
    pub const ALL: [ErrorCode; 18] = [
        ErrorCode::SyntaxError,
        ErrorCode::UnknownTable,
        ErrorCode::UnknownColumn,
        ErrorCode::TypeMismatch,
        ErrorCode::DuplicateKey,
        ErrorCode::NotNullViolation,
        ErrorCode::StorageFull,
        ErrorCode::TxnAborted,
        ErrorCode::ReadOnlyTxn,
        ErrorCode::TxnAlreadyOpen,
        ErrorCode::SchemaChangeInTxn,
        ErrorCode::LockTimeout,
        ErrorCode::Cancelled,
        ErrorCode::TooManyConnections,
        ErrorCode::DatabaseExists,
        ErrorCode::TableExists,
        ErrorCode::DatabaseInUse,
        ErrorCode::UnknownDatabase,
    ];

    /// The wire name of the code. Equal to the serde form.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::SyntaxError => "SYNTAX_ERROR",
            ErrorCode::UnknownTable => "UNKNOWN_TABLE",
            ErrorCode::UnknownColumn => "UNKNOWN_COLUMN",
            ErrorCode::TypeMismatch => "TYPE_MISMATCH",
            ErrorCode::DuplicateKey => "DUPLICATE_KEY",
            ErrorCode::NotNullViolation => "NOT_NULL_VIOLATION",
            ErrorCode::StorageFull => "STORAGE_FULL",
            ErrorCode::TxnAborted => "TXN_ABORTED",
            ErrorCode::ReadOnlyTxn => "READ_ONLY_TXN",
            ErrorCode::TxnAlreadyOpen => "TXN_ALREADY_OPEN",
            ErrorCode::SchemaChangeInTxn => "SCHEMA_CHANGE_IN_TXN",
            ErrorCode::LockTimeout => "LOCK_TIMEOUT",
            ErrorCode::Cancelled => "CANCELLED",
            ErrorCode::TooManyConnections => "TOO_MANY_CONNECTIONS",
            ErrorCode::DatabaseExists => "DATABASE_EXISTS",
            ErrorCode::TableExists => "TABLE_EXISTS",
            ErrorCode::DatabaseInUse => "DATABASE_IN_USE",
            ErrorCode::UnknownDatabase => "UNKNOWN_DATABASE",
        }
    }
}

impl FromStr for ErrorCode {
    type Err = String;

    /// Reads a wire name. The name is the contract, so nothing else reads.
    fn from_str(name: &str) -> Result<ErrorCode, String> {
        ErrorCode::ALL
            .into_iter()
            .find(|code| code.as_str() == name)
            .ok_or_else(|| format!("unknown error code: {name}"))
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failed statement. The statement changes no data. See [FR69].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbError {
    pub code: ErrorCode,
    pub message: String,
    /// The character offset in the statement. `None` when the error has no
    /// position.
    pub position: Option<u32>,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        match self.position {
            Some(p) => write!(f, " at position {p}"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for DbError {}

#[cfg(test)]
mod tests {
    use super::*;

    const NAMES: [(ErrorCode, &str); 18] = [
        (ErrorCode::SyntaxError, "SYNTAX_ERROR"),
        (ErrorCode::UnknownTable, "UNKNOWN_TABLE"),
        (ErrorCode::UnknownColumn, "UNKNOWN_COLUMN"),
        (ErrorCode::TypeMismatch, "TYPE_MISMATCH"),
        (ErrorCode::DuplicateKey, "DUPLICATE_KEY"),
        (ErrorCode::NotNullViolation, "NOT_NULL_VIOLATION"),
        (ErrorCode::StorageFull, "STORAGE_FULL"),
        (ErrorCode::TxnAborted, "TXN_ABORTED"),
        (ErrorCode::ReadOnlyTxn, "READ_ONLY_TXN"),
        (ErrorCode::TxnAlreadyOpen, "TXN_ALREADY_OPEN"),
        (ErrorCode::SchemaChangeInTxn, "SCHEMA_CHANGE_IN_TXN"),
        (ErrorCode::LockTimeout, "LOCK_TIMEOUT"),
        (ErrorCode::Cancelled, "CANCELLED"),
        (ErrorCode::TooManyConnections, "TOO_MANY_CONNECTIONS"),
        (ErrorCode::DatabaseExists, "DATABASE_EXISTS"),
        (ErrorCode::TableExists, "TABLE_EXISTS"),
        (ErrorCode::DatabaseInUse, "DATABASE_IN_USE"),
        (ErrorCode::UnknownDatabase, "UNKNOWN_DATABASE"),
    ];

    #[test]
    fn code_writes_its_screaming_snake_name() {
        for (code, name) in NAMES {
            assert_eq!(serde_json::to_string(&code).unwrap(), format!("\"{name}\""));
        }
    }

    #[test]
    fn code_reads_back_from_its_name() {
        for (code, name) in NAMES {
            let read: ErrorCode = serde_json::from_str(&format!("\"{name}\"")).unwrap();
            assert_eq!(read, code);
        }
    }

    #[test]
    fn as_str_matches_the_serde_name() {
        for (code, name) in NAMES {
            assert_eq!(code.as_str(), name);
        }
    }

    #[test]
    fn every_code_sits_in_the_table_that_names_it() {
        assert_eq!(ErrorCode::ALL.len(), NAMES.len());
        for (code, _) in NAMES {
            assert!(ErrorCode::ALL.contains(&code), "{code} is missing from ALL");
        }
    }

    #[test]
    fn a_code_reads_back_from_its_wire_name() {
        for (code, name) in NAMES {
            assert_eq!(name.parse::<ErrorCode>().unwrap(), code);
        }
    }

    #[test]
    fn a_name_that_is_not_a_code_fails_to_parse() {
        for name in ["", "NO_SUCH_CODE", "SyntaxError", "syntax_error"] {
            let e = name.parse::<ErrorCode>().unwrap_err();
            assert!(e.contains("unknown error code"), "{name:?}: {e}");
        }
    }

    #[test]
    fn an_unknown_code_name_fails_to_read() {
        assert!(serde_json::from_str::<ErrorCode>("\"NO_SUCH_CODE\"").is_err());
    }

    #[test]
    fn a_rust_style_code_name_fails_to_read() {
        assert!(serde_json::from_str::<ErrorCode>("\"SyntaxError\"").is_err());
    }

    #[test]
    fn an_error_with_a_position_survives_a_json_round_trip() {
        let e = DbError {
            code: ErrorCode::SyntaxError,
            message: "unexpected token".to_string(),
            position: Some(12),
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<DbError>(&json).unwrap(), e);
    }

    #[test]
    fn an_error_without_a_position_survives_a_json_round_trip() {
        let e = DbError {
            code: ErrorCode::StorageFull,
            message: "disk is full".to_string(),
            position: None,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<DbError>(&json).unwrap(), e);
    }

    #[test]
    fn display_shows_the_code_the_message_and_the_position() {
        let e = DbError {
            code: ErrorCode::UnknownTable,
            message: "no table \"t\"".to_string(),
            position: Some(7),
        };
        assert_eq!(e.to_string(), "UNKNOWN_TABLE: no table \"t\" at position 7");
    }

    #[test]
    fn display_without_a_position_shows_the_code_and_the_message() {
        let e = DbError {
            code: ErrorCode::TxnAborted,
            message: "transaction aborted".to_string(),
            position: None,
        };
        assert_eq!(e.to_string(), "TXN_ABORTED: transaction aborted");
    }
}
