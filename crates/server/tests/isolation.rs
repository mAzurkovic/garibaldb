//! Isolation: the result of transactions that run at the same time equals the
//! result of running them one after the other.
//!
//! One test for each known anomaly that a serializable system has to prevent.
//! Every one drives real connections through a scripted interleaving, so a
//! client that has to wait for a lock really waits.

mod h1;
mod h2;

use std::thread;
use std::time::{Duration, Instant};

use protocol::ErrorCode;

use h1::{DataDir, Server};
use h2::Clients;

/// Two rows of ten and twenty, which every anomaly below starts from.
fn ledger(clients: &mut Clients) {
    clients.ok(
        0,
        "CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
    );
    clients.ok(0, "INSERT INTO t (id, n) VALUES (1, 10), (2, 20)");
}

fn n_of(clients: &mut Clients, client: usize, id: i64) -> i64 {
    clients.number(client, &format!("SELECT n FROM t WHERE id = {id}"))
}

/// Two clients change the same row and both commit. One change survives
/// whole, and the result is not a mix of the two.
#[test]
fn a_dirty_write_cannot_happen() {
    let server = Server::start();
    let mut clients = Clients::open(&server, 2);
    ledger(&mut clients);

    clients.ok(0, "BEGIN");
    clients.ok(0, "UPDATE t SET n = 11 WHERE id = 1");
    // The second writer waits at its BEGIN, because the lock is taken.
    clients.send(1, "BEGIN");
    clients.ok(0, "UPDATE t SET n = 21 WHERE id = 2");
    clients.ok(0, "COMMIT");

    assert_eq!(h2::failure(&clients.answer(1)), None, "the lock came free");
    clients.ok(1, "UPDATE t SET n = 12 WHERE id = 1");
    clients.ok(1, "UPDATE t SET n = 22 WHERE id = 2");
    clients.ok(1, "COMMIT");

    // Both rows come from the second transaction, not one from each.
    assert_eq!(
        (n_of(&mut clients, 0, 1), n_of(&mut clients, 0, 2)),
        (12, 22)
    );
}

/// One client changes a row, another reads it before the change commits. The
/// reader sees the old value.
#[test]
fn a_dirty_read_cannot_happen() {
    let server = Server::start();
    let mut clients = Clients::open(&server, 2);
    ledger(&mut clients);

    clients.ok(0, "BEGIN");
    clients.ok(0, "UPDATE t SET n = 11 WHERE id = 1");

    clients.ok(1, "BEGIN READ ONLY");
    assert_eq!(n_of(&mut clients, 1, 1), 10, "the change had not committed");

    clients.ok(0, "COMMIT");
    assert_eq!(
        n_of(&mut clients, 1, 1),
        10,
        "the reader keeps the state it began with"
    );

    clients.ok(1, "COMMIT");
    assert_eq!(n_of(&mut clients, 1, 1), 11, "and sees it once it ends");
}

/// One client reads a row twice while another changes it in between. Both
/// reads give the same value.
#[test]
fn a_non_repeatable_read_cannot_happen() {
    let server = Server::start();
    let mut clients = Clients::open(&server, 2);
    ledger(&mut clients);

    clients.ok(0, "BEGIN READ ONLY");
    assert_eq!(n_of(&mut clients, 0, 1), 10);

    clients.ok(1, "UPDATE t SET n = 11 WHERE id = 1");

    assert_eq!(n_of(&mut clients, 0, 1), 10, "the second read is the same");
    clients.ok(0, "COMMIT");
    assert_eq!(n_of(&mut clients, 0, 1), 11);
}

/// One client counts rows twice while another adds a matching row in
/// between. Both counts give the same rows.
#[test]
fn a_phantom_read_cannot_happen() {
    let server = Server::start();
    let mut clients = Clients::open(&server, 2);
    ledger(&mut clients);

    clients.ok(0, "BEGIN READ ONLY");
    let first = clients.rows(0, "SELECT id FROM t");

    clients.ok(1, "INSERT INTO t (id, n) VALUES (3, 30)");

    assert_eq!(
        clients.rows(0, "SELECT id FROM t"),
        first,
        "no row appeared"
    );
    clients.ok(0, "COMMIT");
    assert_ne!(clients.rows(0, "SELECT id FROM t"), first);
}

/// Two clients read a value, add one, and write it back. The value grows by
/// two, because the second read happens after the first write committed.
#[test]
fn an_update_cannot_be_lost() {
    let server = Server::start();
    let mut clients = Clients::open(&server, 2);
    ledger(&mut clients);

    clients.ok(0, "BEGIN");
    let read = n_of(&mut clients, 0, 1);
    // The second client cannot even read its way into a lost update: its
    // transaction does not begin until the first one ends.
    clients.send(1, "BEGIN");
    clients.ok(0, &format!("UPDATE t SET n = {} WHERE id = 1", read + 1));
    clients.ok(0, "COMMIT");

    assert_eq!(h2::failure(&clients.answer(1)), None);
    let read = n_of(&mut clients, 1, 1);
    assert_eq!(
        read, 11,
        "the second transaction reads the first one's work"
    );
    clients.ok(1, &format!("UPDATE t SET n = {} WHERE id = 1", read + 1));
    clients.ok(1, "COMMIT");

    assert_eq!(n_of(&mut clients, 0, 1), 12, "the value grew by two");
}

/// Two clients each read both rows and change the other one. The result
/// matches one of the two serial orders, so the sum they each checked still
/// holds at the end.
#[test]
fn a_write_skew_cannot_happen() {
    let server = Server::start();
    let mut clients = Clients::open(&server, 2);
    ledger(&mut clients);

    clients.ok(0, "BEGIN");
    let (one, two) = (n_of(&mut clients, 0, 1), n_of(&mut clients, 0, 2));
    assert_eq!(one + two, 30);
    clients.send(1, "BEGIN");
    // Takes ten off the first row, which keeps the sum only if nothing else
    // moves at the same time.
    clients.ok(0, &format!("UPDATE t SET n = {} WHERE id = 1", one - 10));
    clients.ok(0, &format!("UPDATE t SET n = {} WHERE id = 2", two + 10));
    clients.ok(0, "COMMIT");

    assert_eq!(h2::failure(&clients.answer(1)), None);
    let (one, two) = (n_of(&mut clients, 1, 1), n_of(&mut clients, 1, 2));
    assert_eq!(one + two, 30, "it reads a whole state, not half of one");
    clients.ok(1, &format!("UPDATE t SET n = {} WHERE id = 1", one + 5));
    clients.ok(1, &format!("UPDATE t SET n = {} WHERE id = 2", two - 5));
    clients.ok(1, "COMMIT");

    assert_eq!(
        (n_of(&mut clients, 0, 1), n_of(&mut clients, 0, 2)),
        (5, 25),
        "both transfers happened, one after the other"
    );
}

/// A writer that waits past the timeout is told to try again, and a client
/// that does try again gets through.
#[test]
fn a_wait_past_the_timeout_is_refused_and_a_retry_succeeds() {
    let data = DataDir::new("lock-timeout");
    let server = Server::start_on(&data.0, &["--lock-timeout-ms", "100"]);
    let mut clients = Clients::open(&server, 2);
    ledger(&mut clients);

    clients.ok(0, "BEGIN");
    clients.ok(0, "UPDATE t SET n = 11 WHERE id = 1");

    let waited = Instant::now();
    assert_eq!(clients.error(1, "BEGIN"), ErrorCode::LockTimeout);
    assert!(
        waited.elapsed() < Duration::from_secs(1),
        "it waited its 100ms and no longer"
    );
    // A statement of its own waits on the same lock.
    assert_eq!(
        clients.error(1, "UPDATE t SET n = 99 WHERE id = 1"),
        ErrorCode::LockTimeout
    );

    clients.ok(0, "COMMIT");
    clients.ok(1, "BEGIN");
    clients.ok(1, "UPDATE t SET n = 12 WHERE id = 1");
    clients.ok(1, "COMMIT");
    assert_eq!(n_of(&mut clients, 0, 1), 12);
}

/// A reader keeps its snapshot while a writer commits over it, and the
/// checkpoint that would take its frames away waits for it to end.
#[test]
fn a_long_reader_keeps_its_snapshot() {
    let server = Server::start();
    let mut clients = Clients::open(&server, 2);
    ledger(&mut clients);

    clients.ok(0, "BEGIN READ ONLY");
    let seen = clients.rows(0, "SELECT id, n FROM t");

    for n in 11..=40 {
        clients.ok(1, &format!("UPDATE t SET n = {n} WHERE id = 1"));
    }

    assert_eq!(
        clients.rows(0, "SELECT id, n FROM t"),
        seen,
        "thirty commits later, the reader sees what it began with"
    );
    clients.ok(0, "COMMIT");
    assert_eq!(n_of(&mut clients, 0, 1), 40);
}

/// Many clients, each running random transactions at the same time.
///
/// Every transfer moves one unit between two accounts, so the total never
/// changes. A reader that saw a total other than the one it started with
/// would have read half of a transaction, and an account whose end value
/// disagreed with the transfers that committed would mean one was lost or
/// applied twice. Neither can happen if the transactions fell in some serial
/// order, and the commits are in that order.
#[test]
fn many_clients_leave_a_result_that_some_serial_order_gives() {
    const CLIENTS: usize = 20;
    const EACH: usize = 50;
    const ACCOUNTS: i64 = 5;
    const START: i64 = 100;

    // A small checkpoint size, so a thousand commits keep the log in hand
    // instead of holding every frame of them.
    let data = DataDir::new("many-clients");
    let server = Server::start_on(&data.0, &["--checkpoint-bytes", "262144"]);
    let mut setup = Clients::open(&server, 1);
    setup.ok(
        0,
        "CREATE TABLE account (id INTEGER PRIMARY KEY, n INTEGER NOT NULL)",
    );
    for id in 1..=ACCOUNTS {
        setup.ok(
            0,
            &format!("INSERT INTO account (id, n) VALUES ({id}, {START})"),
        );
    }
    drop(setup);

    let moved: Vec<Vec<i64>> = thread::scope(|scope| {
        let workers: Vec<_> = (0..CLIENTS)
            .map(|client| {
                let server = &server;
                scope.spawn(move || {
                    let mut clients = Clients::open(server, 1);
                    // A cheap, repeatable spread of keys and kinds, one
                    // sequence for each client.
                    let mut seed = client as u64 * 7919 + 13;
                    let mut next = move || {
                        seed = seed
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        (seed >> 33) as i64
                    };
                    let mut moved = vec![0; ACCOUNTS as usize + 1];
                    for _ in 0..EACH {
                        match next() % 4 {
                            // One transaction in four only reads, and checks
                            // that what it reads is a whole state.
                            0 => read_only(&mut clients, ACCOUNTS, START),
                            _ => {
                                let from = next() % ACCOUNTS + 1;
                                let to = (from + next() % (ACCOUNTS - 1)) % ACCOUNTS + 1;
                                if transfer(&mut clients, from, to) {
                                    moved[from as usize] -= 1;
                                    moved[to as usize] += 1;
                                }
                            }
                        }
                    }
                    moved
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("a client ends"))
            .collect()
    });

    let mut clients = Clients::open(&server, 1);
    let mut total = 0;
    for id in 1..=ACCOUNTS {
        let want: i64 = START + moved.iter().map(|client| client[id as usize]).sum::<i64>();
        let got = clients.number(0, &format!("SELECT n FROM account WHERE id = {id}"));
        assert_eq!(got, want, "account {id} disagrees with the transfers made");
        total += got;
    }
    assert_eq!(total, ACCOUNTS * START, "the total never moves");
}

/// One transfer, retried for as long as the lock keeps it out. True when it
/// committed.
fn transfer(clients: &mut Clients, from: i64, to: i64) -> bool {
    loop {
        match try_transfer(clients, from, to) {
            Some(moved) => return moved,
            // Another writer held the lock. A client that tries again gets
            // through, which is what the retry code is for.
            None => continue,
        }
    }
}

fn try_transfer(clients: &mut Clients, from: i64, to: i64) -> Option<bool> {
    let retry = |answer: &[protocol::ServerMsg]| {
        matches!(
            h2::failure(answer),
            Some((ErrorCode::LockTimeout | ErrorCode::TxnAborted, _))
        )
    };
    let answer = clients.run(0, "BEGIN");
    if retry(&answer) {
        return None;
    }
    assert_eq!(h2::failure(&answer), None);

    let out = clients.number(0, &format!("SELECT n FROM account WHERE id = {from}"));
    let into = clients.number(0, &format!("SELECT n FROM account WHERE id = {to}"));
    if out <= 0 {
        clients.ok(0, "ROLLBACK");
        return Some(false);
    }
    clients.ok(
        0,
        &format!("UPDATE account SET n = {} WHERE id = {from}", out - 1),
    );
    clients.ok(
        0,
        &format!("UPDATE account SET n = {} WHERE id = {to}", into + 1),
    );
    let answer = clients.run(0, "COMMIT");
    assert_eq!(h2::failure(&answer), None);
    Some(true)
}

/// A transaction that only reads, which must see a whole state twice over.
fn read_only(clients: &mut Clients, accounts: i64, start: i64) {
    clients.ok(0, "BEGIN READ ONLY");
    let first = clients.rows(0, "SELECT id, n FROM account");
    let total: i64 = (1..=accounts)
        .map(|id| clients.number(0, &format!("SELECT n FROM account WHERE id = {id}")))
        .sum();
    assert_eq!(total, accounts * start, "a reader saw half of a transfer");
    assert_eq!(
        clients.rows(0, "SELECT id, n FROM account"),
        first,
        "a reader saw a change that came after it began"
    );
    clients.ok(0, "COMMIT");
}
