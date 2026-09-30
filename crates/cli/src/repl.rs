//! The prompt, the statement buffer, and the run of one statement.
//!
//! See [FR72], [FR73], [FR77], [FR78], and [FR80].

use std::io::{self, Write};

use protocol::{DbError, ServerMsg, TxState};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::conn::Connection;
use crate::render::TableWriter;

/// Collects the lines of one statement. A `;` ends it. See [FR72] and [FR73].
#[derive(Debug, Default)]
pub struct Buffer {
    text: String,
}

impl Buffer {
    /// Whether a statement is half typed, which the prompt shows.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Adds one line, and returns the statement once a `;` ends it.
    pub fn push(&mut self, line: &str) -> Option<String> {
        let line = line.trim_end();
        if self.text.is_empty() && line.is_empty() {
            return None;
        }
        if !self.text.is_empty() {
            self.text.push('\n');
        }
        self.text.push_str(line);
        match line.ends_with(';') {
            true => Some(std::mem::take(&mut self.text)),
            false => None,
        }
    }

    /// Drops a half-typed statement.
    pub fn clear(&mut self) {
        self.text.clear();
    }
}

/// The prompt for the state of the connection. See [FR80].
pub fn prompt(tx: TxState, continued: bool) -> &'static str {
    if continued {
        return "...> ";
    }
    match tx {
        TxState::None => "garibaldb> ",
        TxState::Open => "garibaldb(tx)> ",
        TxState::ReadOnly => "garibaldb(ro)> ",
    }
}

/// What one statement left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// The transaction state of the `Ready` that closed the statement.
    pub tx: TxState,
    pub failed: bool,
}

/// Sends one statement and writes its answer as it arrives.
///
/// See [FR74], [FR75], and [FR76].
pub fn run_statement<W: Write>(
    conn: &mut Connection,
    sql: &str,
    out: &mut W,
) -> io::Result<Outcome> {
    let mut table = TableWriter::new(out);
    let mut outcome = Outcome {
        tx: TxState::None,
        failed: false,
    };
    let mut complete = false;
    for message in conn.query(sql)? {
        match message? {
            ServerMsg::RowDesc { cols } => table.header(&cols),
            ServerMsg::DataRow { values } => table.row(&values)?,
            ServerMsg::Complete { .. } => complete = true,
            ServerMsg::Error {
                code,
                message,
                position,
            } => {
                outcome.failed = true;
                table.error(&DbError {
                    code,
                    message,
                    position,
                })?;
            }
            ServerMsg::Ready { tx, .. } => outcome.tx = tx,
        }
    }
    if complete {
        table.footer()?;
    }
    Ok(outcome)
}

/// Runs one statement and stops. See [FR77] and [FR78].
pub fn run_once<W: Write>(conn: &mut Connection, sql: &str, out: &mut W) -> io::Result<i32> {
    let outcome = run_statement(conn, sql, out)?;
    conn.close()?;
    Ok(status(outcome.failed))
}

/// The status code of a client that ran a statement. See [FR78].
fn status(failed: bool) -> i32 {
    match failed {
        true => 1,
        false => 0,
    }
}

/// Takes one typed line, and runs the statement that it completes.
///
/// Returns the transaction state for the next prompt, which is the old one
/// while the statement is still half typed. See [FR72] and [FR80].
pub fn feed<W: Write>(
    conn: &mut Connection,
    buffer: &mut Buffer,
    tx: TxState,
    line: &str,
    out: &mut W,
) -> io::Result<TxState> {
    match buffer.push(line) {
        Some(sql) => Ok(run_statement(conn, &sql, out)?.tx),
        None => Ok(tx),
    }
}

/// Reads statements from the terminal until the user leaves.
///
/// This is the only part of the client that needs a terminal, so it holds no
/// decision of its own. Ctrl-C here drops a half-typed statement, because
/// `rustyline` keeps the signal to itself while it owns the prompt. Ctrl-C
/// while a statement runs reaches the handler that `main` installed.
pub fn run(conn: &mut Connection) -> io::Result<i32> {
    let mut editor = DefaultEditor::new().map_err(io::Error::other)?;
    let mut buffer = Buffer::default();
    let mut tx = TxState::None;
    let mut out = io::stdout();
    loop {
        match editor.readline(prompt(tx, !buffer.is_empty())) {
            Ok(line) => {
                let _ = editor.add_history_entry(&line);
                tx = feed(conn, &mut buffer, tx, &line, &mut out)?;
            }
            Err(ReadlineError::Interrupted) => buffer.clear(),
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(io::Error::other(e)),
        }
    }
    conn.close()?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_on_one_line_runs_at_its_semicolon() {
        let mut buffer = Buffer::default();
        assert_eq!(buffer.push("SELECT 1;"), Some("SELECT 1;".to_string()));
        assert!(buffer.is_empty(), "the buffer starts over");
    }

    #[test]
    fn a_statement_over_three_lines_runs_once_the_semicolon_arrives() {
        let mut buffer = Buffer::default();
        assert_eq!(buffer.push("SELECT a"), None);
        assert!(!buffer.is_empty(), "the statement is half typed");
        assert_eq!(buffer.push("FROM t"), None);
        assert_eq!(
            buffer.push("WHERE a = 1;"),
            Some("SELECT a\nFROM t\nWHERE a = 1;".to_string())
        );
    }

    #[test]
    fn an_empty_line_before_a_statement_adds_nothing() {
        let mut buffer = Buffer::default();
        assert_eq!(buffer.push(""), None);
        assert_eq!(buffer.push("   "), None);
        assert!(buffer.is_empty());
        assert_eq!(buffer.push("SELECT 1;"), Some("SELECT 1;".to_string()));
    }

    #[test]
    fn an_empty_line_inside_a_statement_keeps_the_statement() {
        let mut buffer = Buffer::default();
        assert_eq!(buffer.push("SELECT 1"), None);
        assert_eq!(buffer.push(""), None);
        assert_eq!(buffer.push(";"), Some("SELECT 1\n\n;".to_string()));
    }

    #[test]
    fn trailing_spaces_do_not_hide_the_semicolon() {
        let mut buffer = Buffer::default();
        assert_eq!(buffer.push("SELECT 1;  "), Some("SELECT 1;".to_string()));
    }

    #[test]
    fn a_cleared_buffer_drops_the_half_typed_statement() {
        let mut buffer = Buffer::default();
        buffer.push("SELECT a");
        buffer.clear();
        assert!(buffer.is_empty());
    }

    #[test]
    fn the_prompt_shows_the_transaction_state() {
        assert_eq!(prompt(TxState::None, false), "garibaldb> ");
        assert_eq!(prompt(TxState::Open, false), "garibaldb(tx)> ");
        assert_eq!(prompt(TxState::ReadOnly, false), "garibaldb(ro)> ");
    }

    #[test]
    fn a_half_typed_statement_changes_the_prompt() {
        for tx in [TxState::None, TxState::Open, TxState::ReadOnly] {
            assert_eq!(prompt(tx, true), "...> ");
        }
    }

    #[test]
    fn a_failed_statement_stops_with_a_non_zero_code() {
        assert_eq!(status(true), 1);
        assert_eq!(status(false), 0);
    }
}
