//! The server over a real socket, seen from outside the process.
//!
//! `server` is a binary crate, so a test cannot import its modules. Each test
//! therefore starts the `garibaldb-server` binary on port 0 and speaks the
//! protocol over TCP, which is what a client does. The socket helpers live in
//! `h1`, because the statement runner needs the same ones.
//!
//! Nothing here sleeps. The binary logs the bound port before it accepts, so
//! the log line is the signal that the socket stands.

mod h1;

use protocol::{ClientMsg, ErrorCode, PROTOCOL_VERSION, ServerMsg, TxState};

use h1::{Conn, DataDir, Server, startup, wait_log};

fn query() -> ClientMsg {
    ClientMsg::Query {
        sql: "SELECT * FROM shelf".to_string(),
    }
}

/// A statement that the grammar has no form for.
fn broken_query() -> ClientMsg {
    ClientMsg::Query {
        sql: "SELECT a FROM".to_string(),
    }
}

/// The kind of the `Complete` in an answer, which says the statement ran.
fn kind_of(answer: &[ServerMsg]) -> String {
    answer
        .iter()
        .find_map(|msg| match msg {
            ServerMsg::Complete { kind, .. } => Some(kind.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a complete, got {answer:?}"))
}

fn code_of(msg: &ServerMsg) -> ErrorCode {
    match msg {
        ServerMsg::Error { code, .. } => *code,
        other => panic!("expected an error, got {other:?}"),
    }
}

/// [FR62] and [FR64].
#[test]
fn a_startup_answers_ready_with_an_id_a_secret_and_no_transaction() {
    let server = Server::start();
    let mut conn = server.connect();
    conn.send(&startup(PROTOCOL_VERSION));
    let ServerMsg::Ready {
        conn_id,
        secret,
        tx,
    } = conn.expect()
    else {
        panic!("expected a ready");
    };
    assert_eq!((conn_id, tx), (1, TxState::None));
    assert!(secret.parse::<u64>().is_ok(), "the secret {secret:?}");
}

/// [FR62]. The server speaks version 1 only.
#[test]
fn a_startup_with_an_unknown_version_answers_an_error_and_closes() {
    let server = Server::start();
    let mut conn = server.connect();
    conn.send(&startup(0));
    assert_eq!(code_of(&conn.expect()), ErrorCode::SyntaxError);
    assert_eq!(conn.recv(), None, "the connection stays open");
}

/// A statement that parses names a table that does not exist yet, and a
/// `Ready` follows every statement.
#[test]
fn a_query_answers_unknown_table_and_then_ready() {
    let server = Server::start();
    let mut conn = server.connect();
    let (conn_id, secret) = conn.start_up();
    conn.send(&query());
    assert_eq!(code_of(&conn.expect()), ErrorCode::UnknownTable);
    assert_eq!(
        conn.expect(),
        ServerMsg::Ready {
            conn_id,
            secret,
            tx: TxState::None,
        }
    );
}

/// [FR63] and [NFR14]. Every connection stands open while the queries run.
#[test]
fn a_hundred_connections_run_a_query_at_the_same_time() {
    let server = Server::start();
    let mut conns: Vec<Conn> = (0..100).map(|_| server.connect()).collect();
    let mut ids: Vec<u64> = conns.iter_mut().map(|c| c.start_up().0).collect();
    for conn in &mut conns {
        conn.send(&query());
    }
    for conn in &mut conns {
        assert_eq!(code_of(&conn.expect()), ErrorCode::UnknownTable);
        assert!(matches!(conn.expect(), ServerMsg::Ready { .. }));
    }
    // Each connection got its own session, so no id repeats.
    ids.sort_unstable();
    assert_eq!(ids, (1..=100).collect::<Vec<u64>>());
}

/// [FR66]. The cancel connection carries no session, so the match reaches the
/// log and not the wire. The flag itself is unreadable until milestone 11.
#[test]
fn a_cancel_matches_only_with_the_right_secret() {
    let mut server = Server::start();
    let mut target = server.connect();
    let (conn_id, secret) = target.start_up();
    let wrong = secret.parse::<u64>().unwrap().wrapping_add(1).to_string();
    for (secret, want) in [(secret, "match true"), (wrong, "match false")] {
        let mut conn = server.connect();
        conn.send(&ClientMsg::Cancel { conn_id, secret });
        assert_eq!(conn.recv(), None, "a cancel connection answers nothing");
        assert!(wait_log(&mut server.log, "cancels").ends_with(want));
    }
}

/// [NFR14] caps the connections. Past the cap the server names the reason and
/// closes, so a client can tell a busy server from a full disk.
#[test]
fn a_connection_past_the_cap_is_refused() {
    let mut server = Server::start_with(&["--max-connections", "2"]);

    // Hold both slots open. A closed connection would free one.
    let mut held = Vec::new();
    for _ in 0..2 {
        let mut conn = server.connect();
        conn.start_up();
        held.push(conn);
    }

    let mut extra = server.connect();
    assert_eq!(code_of(&extra.expect()), ErrorCode::TooManyConnections);
    assert_eq!(extra.recv(), None, "the server closes the extra connection");

    // A slot that frees up lets the next client in. Dropping the connection
    // only starts the release, so the test waits for the line that the server
    // writes once the slot is back.
    held.pop();
    wait_log(&mut server.log, "closed");
    let mut after = server.connect();
    let (conn_id, _) = after.start_up();
    assert!(conn_id > 0);
}

/// [FR67] and [FR70]. A statement the parser refuses names the character that
/// broke it, and the connection carries the next statement.
#[test]
fn a_malformed_statement_answers_a_syntax_error_with_its_position() {
    let server = Server::start();
    let mut conn = server.connect();
    let (conn_id, secret) = conn.start_up();

    conn.send(&broken_query());
    let ServerMsg::Error { code, position, .. } = conn.expect() else {
        panic!("expected an error");
    };
    assert_eq!(code, ErrorCode::SyntaxError);
    assert_eq!(position, Some(14), "the end of `SELECT a FROM`");
    assert!(matches!(conn.expect(), ServerMsg::Ready { .. }));

    // The session survives a statement it could not read.
    conn.send(&query());
    assert_eq!(code_of(&conn.expect()), ErrorCode::UnknownTable);
    assert_eq!(
        conn.expect(),
        ServerMsg::Ready {
            conn_id,
            secret,
            tx: TxState::None,
        }
    );
}

/// [FR6] and [FR10]. A schema outlives the process that made it, which is
/// what writing the catalog to disk is for.
#[test]
fn a_table_survives_a_restart() {
    let data = DataDir::new("restart");

    let first = Server::start_on(&data.0, &[]);
    let mut conn = first.connect();
    conn.start_up();
    assert_eq!(
        kind_of(&conn.run("CREATE TABLE item (id INTEGER PRIMARY KEY, label TEXT NOT NULL)")),
        "CREATE TABLE"
    );
    drop(conn);
    drop(first);

    let second = Server::start_on(&data.0, &[]);
    let mut conn = second.connect();
    conn.start_up();
    // A table the restart lost would answer UNKNOWN_TABLE here.
    assert_eq!(kind_of(&conn.run("DROP TABLE item")), "DROP TABLE");
    assert_eq!(
        code_of(&conn.run("DROP TABLE item")[0]),
        ErrorCode::UnknownTable
    );
}

/// [FR5]. Two connections on one server see their own database and no other.
#[test]
fn a_client_sees_only_the_database_it_connected_to() {
    let server = Server::start();
    let mut first = server.connect();
    first.start_up();
    assert_eq!(
        kind_of(&first.run("CREATE DATABASE other")),
        "CREATE DATABASE"
    );
    assert_eq!(
        kind_of(&first.run("CREATE TABLE here (id INTEGER PRIMARY KEY)")),
        "CREATE TABLE"
    );

    let mut second = server.connect();
    second.send(&ClientMsg::Startup {
        version: PROTOCOL_VERSION,
        database: "other".to_string(),
    });
    assert!(matches!(second.expect(), ServerMsg::Ready { .. }));
    assert_eq!(
        code_of(&second.run("DROP TABLE here")[0]),
        ErrorCode::UnknownTable,
        "the table belongs to the other database"
    );
}

/// A database that is not there closes the connection, the way a protocol
/// version the server does not speak closes it.
#[test]
fn a_startup_on_a_database_that_is_not_there_is_refused() {
    let server = Server::start();
    let mut conn = server.connect();
    conn.send(&ClientMsg::Startup {
        version: PROTOCOL_VERSION,
        database: "missing".to_string(),
    });
    assert_eq!(code_of(&conn.expect()), ErrorCode::UnknownDatabase);
    assert_eq!(conn.recv(), None, "the server closes the connection");
}

/// [FR4]. The database under a connection cannot be deleted while it holds
/// it, and can be once it lets go.
#[test]
fn a_database_cannot_be_dropped_while_a_client_holds_it() {
    let mut server = Server::start();
    let mut owner = server.connect();
    owner.start_up();
    owner.run("CREATE DATABASE shop");

    let mut guest = server.connect();
    guest.send(&ClientMsg::Startup {
        version: PROTOCOL_VERSION,
        database: "shop".to_string(),
    });
    assert!(matches!(guest.expect(), ServerMsg::Ready { .. }));
    assert_eq!(
        code_of(&owner.run("DROP DATABASE shop")[0]),
        ErrorCode::DatabaseInUse
    );

    // The guest leaves, and the server logs the close once the slot is back.
    drop(guest);
    wait_log(&mut server.log, "closed");
    assert_eq!(kind_of(&owner.run("DROP DATABASE shop")), "DROP DATABASE");
}

/// [FR43] and [FR75]. A read answers with the columns, then the rows, then
/// how many there were.
#[test]
fn a_read_answers_the_columns_then_the_rows_then_the_count() {
    let server = Server::start();
    let mut conn = server.connect();
    conn.start_up();
    conn.run("CREATE TABLE item (id INTEGER PRIMARY KEY, label TEXT NOT NULL)");
    conn.run("INSERT INTO item (id, label) VALUES (1, 'apple'), (2, 'pear'), (3, 'plum')");

    let answer = conn.run("SELECT label, id FROM item");
    let ServerMsg::RowDesc { cols } = &answer[0] else {
        panic!("expected the columns first, got {:?}", answer[0]);
    };
    assert_eq!(
        cols.iter()
            .map(|col| col.name.as_str())
            .collect::<Vec<&str>>(),
        vec!["label", "id"]
    );

    let rows: Vec<&Vec<protocol::Value>> = answer
        .iter()
        .filter_map(|msg| match msg {
            ServerMsg::DataRow { values } => Some(values),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0],
        &vec![
            protocol::Value::Text("apple".to_string()),
            protocol::Value::Integer(1)
        ]
    );
    assert_eq!(kind_of(&answer), "SELECT");
    let ServerMsg::Complete { rows, .. } = &answer[answer.len() - 2] else {
        panic!("expected a complete before the ready");
    };
    assert_eq!(*rows, 3);
}

/// [FR56] in the small. Rows outlive the process that wrote them.
#[test]
fn rows_survive_a_restart() {
    let data = DataDir::new("rows-restart");

    let first = Server::start_on(&data.0, &[]);
    let mut conn = first.connect();
    conn.start_up();
    conn.run("CREATE TABLE item (id INTEGER PRIMARY KEY, label TEXT NOT NULL)");
    conn.run("INSERT INTO item (id, label) VALUES (1, 'apple'), (2, 'pear')");
    drop(conn);
    drop(first);

    let second = Server::start_on(&data.0, &[]);
    let mut conn = second.connect();
    conn.start_up();
    let answer = conn.run("SELECT id FROM item");
    let rows: Vec<&Vec<protocol::Value>> = answer
        .iter()
        .filter_map(|msg| match msg {
            ServerMsg::DataRow { values } => Some(values),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 2, "the rows were lost across the restart");
    assert_eq!(rows[0], &vec![protocol::Value::Integer(1)]);
}

/// A statement that fails partway answers its error after the rows that
/// already went, and the connection carries the next statement.
#[test]
fn a_read_of_a_column_that_is_not_there_answers_an_error() {
    let server = Server::start();
    let mut conn = server.connect();
    conn.start_up();
    conn.run("CREATE TABLE item (id INTEGER PRIMARY KEY)");
    assert_eq!(
        code_of(&conn.run("SELECT nothing FROM item")[0]),
        ErrorCode::UnknownColumn
    );
    // The session is still good.
    assert_eq!(kind_of(&conn.run("SELECT id FROM item")), "SELECT");
}

/// Every value of one column, as integers.
fn ids(answer: &[ServerMsg]) -> Vec<i64> {
    answer
        .iter()
        .filter_map(|msg| match msg {
            ServerMsg::DataRow { values } => match values.first() {
                Some(protocol::Value::Integer(id)) => Some(*id),
                other => panic!("expected an integer, got {other:?}"),
            },
            _ => None,
        })
        .collect()
}

/// The transaction state of the `Ready` that closed an answer.
fn state(answer: &[ServerMsg]) -> TxState {
    answer
        .iter()
        .find_map(|msg| match msg {
            ServerMsg::Ready { tx, .. } => Some(*tx),
            _ => None,
        })
        .expect("every answer ends with a ready")
}

/// What a transaction committed is there after a restart, and what it rolled
/// back was never there.
#[test]
fn a_restart_keeps_what_committed_and_nothing_else() {
    let data = DataDir::new("txn-restart");

    let first = Server::start_on(&data.0, &[]);
    let mut conn = first.connect();
    conn.start_up();
    conn.run("CREATE TABLE item (id INTEGER PRIMARY KEY, label TEXT NOT NULL)");
    conn.run("BEGIN");
    conn.run("INSERT INTO item (id, label) VALUES (1, 'apple'), (2, 'pear')");
    conn.run("COMMIT");
    conn.run("BEGIN");
    conn.run("INSERT INTO item (id, label) VALUES (3, 'plum')");
    conn.run("DELETE FROM item WHERE id = 1");
    conn.run("ROLLBACK");
    drop(conn);
    drop(first);

    let second = Server::start_on(&data.0, &[]);
    let mut conn = second.connect();
    conn.start_up();
    assert_eq!(ids(&conn.run("SELECT id FROM item")), vec![1, 2]);
}

/// A client that leaves mid-transaction keeps none of it, and the next client
/// is free to write.
#[test]
fn a_disconnect_rolls_back_the_open_transaction() {
    let data = DataDir::new("txn-disconnect");
    let server = Server::start_on(&data.0, &[]);

    let mut conn = server.connect();
    conn.start_up();
    conn.run("CREATE TABLE item (id INTEGER PRIMARY KEY)");
    conn.run("INSERT INTO item (id) VALUES (1)");
    conn.run("BEGIN");
    conn.run("INSERT INTO item (id) VALUES (2)");
    drop(conn);

    let mut next = server.connect();
    next.start_up();
    // The write lock came back with the connection that held it.
    next.run("INSERT INTO item (id) VALUES (3)");
    assert_eq!(ids(&next.run("SELECT id FROM item")), vec![1, 3]);
}

/// `Ready` says what the connection holds, so a client knows whether it is in
/// a transaction without keeping count itself.
#[test]
fn ready_carries_the_transaction_state() {
    let server = Server::start();
    let mut conn = server.connect();
    conn.start_up();

    assert_eq!(
        state(&conn.run("CREATE TABLE item (id INTEGER PRIMARY KEY)")),
        TxState::None
    );
    assert_eq!(state(&conn.run("BEGIN")), TxState::Open);
    assert_eq!(
        state(&conn.run("INSERT INTO item (id) VALUES (1)")),
        TxState::Open
    );
    assert_eq!(state(&conn.run("COMMIT")), TxState::None);

    assert_eq!(state(&conn.run("BEGIN READ ONLY")), TxState::ReadOnly);
    assert_eq!(state(&conn.run("SELECT id FROM item")), TxState::ReadOnly);
    // A refused change leaves the transaction as it was.
    assert_eq!(
        state(&conn.run("INSERT INTO item (id) VALUES (2)")),
        TxState::ReadOnly
    );
    assert_eq!(state(&conn.run("ROLLBACK")), TxState::None);
}
