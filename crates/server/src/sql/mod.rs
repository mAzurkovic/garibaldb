//! The SQL front end. Text in, a [`ast::Statement`] out, or a `DbError` that
//! names the character that broke the statement. See [FR67].
//!
//! Nothing here reads the catalog, so a statement that parses says nothing
//! about a table that exists. Milestone 5 adds the catalog, and milestone 8
//! runs the statement.

pub mod ast;
pub mod lexer;
pub mod parser;
pub mod token;

use protocol::{DbError, ErrorCode};

/// An error that names the character that broke the statement. See [FR67]
/// and [FR70].
pub fn syntax_error(message: impl Into<String>, position: u32) -> DbError {
    DbError {
        code: ErrorCode::SyntaxError,
        message: message.into(),
        position: Some(position),
    }
}
