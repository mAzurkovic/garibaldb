//! The `garibaldb` client. See `docs/design.md`.

use std::io;
use std::process::exit;

use cli::args::Args;
use cli::conn::{CancelKey, Connection};
use cli::repl;

/// Reads the command line, opens the connection, then runs one statement or
/// the prompt. A bad flag or a connection that never stands stops with code 2,
/// and a statement that failed with 1.
fn main() {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => fail(&e),
    };
    let mut conn = match Connection::connect(&args.host, args.port) {
        Ok(conn) => conn,
        Err(e) => fail(&format!("cannot reach {}:{}: {e}", args.host, args.port)),
    };
    let key = match conn.startup(&args.database) {
        Ok(key) => key,
        Err(e) => fail(&e.to_string()),
    };
    watch_for_cancel(key);

    let mut out = io::stdout();
    match args.command {
        Some(sql) => match repl::run_once(&mut conn, &sql, &mut out) {
            Ok(code) => exit(code),
            Err(e) => fail(&e.to_string()),
        },
        None => match repl::run(&mut conn) {
            Ok(code) => exit(code),
            Err(e) => fail(&e.to_string()),
        },
    }
}

/// Ctrl-C while a statement runs stops the statement and keeps the connection.
///
/// `ctrlc` answers the signal on a thread of its own, so this closure is
/// ordinary code and may open the second socket itself. See [FR79].
fn watch_for_cancel(key: CancelKey) {
    let installed = ctrlc::set_handler(move || {
        if let Err(e) = key.send() {
            eprintln!("garibaldb: the cancel did not reach the server: {e}");
        }
    });
    if let Err(e) = installed {
        eprintln!("garibaldb: ctrl-c will not stop a statement: {e}");
    }
}

fn fail(message: &str) -> ! {
    eprintln!("garibaldb: {message}");
    exit(2)
}
