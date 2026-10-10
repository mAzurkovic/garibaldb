//! What a connection holds between its `BEGIN` and the end of it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::{DbError, ErrorCode};

use crate::catalog::{Database, named};
use crate::store::pool::{BufferPool, Mark};
use crate::txn::lock::WriteGuard;

/// What a transaction said it would do when it began. The declaration is what
/// keeps the server from ever upgrading a lock, so it needs no deadlock
/// detection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TxnKind {
    ReadOnly,
    ReadWrite,
}

/// The marks the live readers hold.
///
/// A checkpoint empties the log into the table files, so it may only run when
/// no reader is still looking at a frame it would take away.
#[derive(Default)]
pub struct Readers {
    marks: Mutex<BTreeMap<u64, usize>>,
}

impl Readers {
    /// Records a mark until the lease drops.
    pub fn lease(self: &Arc<Self>, mark: u64) -> Lease {
        *self
            .marks
            .lock()
            .expect("the reader lock holds")
            .entry(mark)
            .or_insert(0) += 1;
        Lease {
            readers: Arc::clone(self),
            mark,
        }
    }

    /// The mark of the reader that is furthest behind, or none when no reader
    /// is live.
    pub fn oldest(&self) -> Option<u64> {
        self.marks
            .lock()
            .expect("the reader lock holds")
            .keys()
            .next()
            .copied()
    }

    fn give_up(&self, mark: u64) {
        let mut marks = self.marks.lock().expect("the reader lock holds");
        // The mark is always there, because a lease put it there and only a
        // lease takes it away.
        if let Some(count) = marks.get_mut(&mark) {
            *count -= 1;
            if *count == 0 {
                marks.remove(&mark);
            }
        }
    }
}

/// One live reader's mark, given up when this drops.
pub struct Lease {
    readers: Arc<Readers>,
    mark: u64,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.readers.give_up(self.mark);
    }
}

/// An open transaction.
pub struct Transaction {
    kind: TxnKind,
    /// Where the log stood when it began, which a rollback cuts back to.
    start: u64,
    /// The mark its reads take, for a transaction that only reads.
    snapshot: u64,
    /// Set when a statement of this transaction failed partway. Its changes
    /// are still in the pool, where no commit may settle them, so the only
    /// way out is a rollback.
    aborted: bool,
    /// The write lock, held until the transaction ends, and the mark a
    /// checkpoint waits for. Both do their work by dropping.
    _guard: Option<WriteGuard>,
    _lease: Option<Lease>,
}

impl Transaction {
    pub fn kind(&self) -> TxnKind {
        self.kind
    }

    pub fn is_aborted(&self) -> bool {
        self.aborted
    }

    /// Ends the transaction without ending the connection. Nothing more of it
    /// runs, and a commit of it rolls back instead.
    pub fn abort(&mut self) {
        self.aborted = true;
    }

    /// Which form of a page its reads take. A writer reads the newest,
    /// because it has to see its own work.
    pub fn mark(&self) -> Mark {
        match self.kind {
            TxnKind::ReadWrite => Mark::Latest,
            TxnKind::ReadOnly => Mark::At(self.snapshot),
        }
    }

    /// Refuses a change from a transaction that said it only reads.
    pub fn check_writable(&self) -> Result<(), DbError> {
        match self.kind {
            TxnKind::ReadWrite => Ok(()),
            TxnKind::ReadOnly => Err(named(
                ErrorCode::ReadOnlyTxn,
                "this transaction began as read only",
            )),
        }
    }

    /// Settles every change of the transaction.
    ///
    /// It takes a borrow and not the transaction itself, so the caller holds
    /// the write lock until it drops the transaction. A checkpoint runs in
    /// that window, where nothing else can append to the log.
    pub fn commit(&self, pool: &BufferPool, db: &Database) -> Result<(), DbError> {
        match self.kind {
            TxnKind::ReadOnly => Ok(()),
            TxnKind::ReadWrite => pool.commit(&db.wal),
        }
    }

    /// Leaves no trace of the transaction. Nothing uncommitted reached a
    /// table file, so there is nothing to undo: the pool gives up the pages
    /// it changed and the log gives up the frames behind them.
    pub fn rollback(&self, pool: &BufferPool, db: &Database) -> Result<(), DbError> {
        match self.kind {
            TxnKind::ReadOnly => Ok(()),
            TxnKind::ReadWrite => {
                pool.discard(&db.wal)?;
                db.wal.truncate_to(self.start)
            }
        }
    }
}

impl Database {
    /// Opens a transaction. A read-write one takes the write lock, a
    /// read-only one takes a mark and nothing else, so it never waits.
    pub fn begin(
        self: &Arc<Self>,
        kind: TxnKind,
        timeout: Duration,
    ) -> Result<Transaction, DbError> {
        match kind {
            TxnKind::ReadWrite => {
                let guard = WriteGuard::take(self, timeout)?;
                Ok(Transaction {
                    kind,
                    // Past the lock, so no other writer's frames can be
                    // below this.
                    start: self.wal.end(),
                    snapshot: self.wal.committed(),
                    aborted: false,
                    _guard: Some(guard),
                    _lease: None,
                })
            }
            TxnKind::ReadOnly => {
                let mark = self.wal.committed();
                Ok(Transaction {
                    kind,
                    start: mark,
                    snapshot: mark,
                    aborted: false,
                    _guard: None,
                    _lease: Some(self.readers.lease(mark)),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use protocol::{DataType, Value};

    use super::*;
    use crate::catalog::testing::Dir;
    use crate::catalog::{ddl, registry::Connected, registry::Registry};
    use crate::sql::ast::ColumnSpec;
    use crate::store::page::PageId;

    const PATIENCE: Duration = Duration::from_secs(5);
    const BRIEF: Duration = Duration::from_millis(50);

    /// A database holding one table, and the registry it belongs to.
    fn shop(label: &str) -> (Dir, Arc<Registry>, Connected) {
        let dir = Dir::new(label);
        let (registry, held) = dir.shop();
        ddl::create_table(
            &held.db,
            "items",
            &[ColumnSpec {
                name: "id".to_string(),
                ty: DataType::Integer,
                not_null: true,
                primary_key: true,
            }],
        )
        .expect("the table is new");
        (dir, registry, held)
    }

    #[test]
    fn a_writer_holds_the_lock_and_a_second_one_waits() {
        let dir = Dir::new("one-writer");
        let (_registry, held) = dir.shop();
        let txn = held.db.begin(TxnKind::ReadWrite, PATIENCE).unwrap();

        let e = held.db.begin(TxnKind::ReadWrite, BRIEF).err().unwrap();
        assert_eq!(e.code, ErrorCode::LockTimeout);

        drop(txn);
        held.db
            .begin(TxnKind::ReadWrite, BRIEF)
            .expect("the lock came free");
    }

    #[test]
    fn a_reader_takes_no_lock_and_waits_for_no_one() {
        let dir = Dir::new("readers-free");
        let (_registry, held) = dir.shop();
        let writer = held.db.begin(TxnKind::ReadWrite, PATIENCE).unwrap();

        let one = held.db.begin(TxnKind::ReadOnly, BRIEF).unwrap();
        let two = held.db.begin(TxnKind::ReadOnly, BRIEF).unwrap();
        assert_eq!(one.kind(), TxnKind::ReadOnly);

        // And a reader holds nothing a writer waits for either.
        drop((writer, one, two));
        held.db.begin(TxnKind::ReadWrite, BRIEF).unwrap();
    }

    #[test]
    fn a_reader_reads_at_its_mark_and_a_writer_reads_the_newest() {
        let dir = Dir::new("marks");
        let (_registry, held) = dir.shop();
        let mark = held.db.wal.committed();

        let reader = held.db.begin(TxnKind::ReadOnly, PATIENCE).unwrap();
        assert_eq!(reader.mark(), Mark::At(mark));
        let writer = held.db.begin(TxnKind::ReadWrite, PATIENCE).unwrap();
        assert_eq!(writer.mark(), Mark::Latest);
    }

    #[test]
    fn a_reader_refuses_a_change_and_a_writer_does_not() {
        let dir = Dir::new("read-only");
        let (_registry, held) = dir.shop();

        let reader = held.db.begin(TxnKind::ReadOnly, PATIENCE).unwrap();
        let e = reader.check_writable().err().unwrap();
        assert_eq!(e.code, ErrorCode::ReadOnlyTxn);
        assert!(e.message.contains("read only"), "{e}");

        drop(reader);
        held.db
            .begin(TxnKind::ReadWrite, PATIENCE)
            .unwrap()
            .check_writable()
            .expect("a writer may write");
    }

    #[test]
    fn a_rollback_leaves_the_log_and_the_pool_as_they_were() {
        let (_dir, registry, held) = shop("rollback");
        let table = held
            .db
            .catalog
            .lock()
            .unwrap()
            .table("items")
            .cloned()
            .expect("the table is there");
        let file = registry.table_file(&held.db, &table).unwrap();
        let pool = registry.pool();

        let txn = held.db.begin(TxnKind::ReadWrite, PATIENCE).unwrap();
        let start = held.db.wal.end();
        let page = pool.allocate(file).unwrap();
        pool.fetch(page).unwrap().bytes_mut()[0] = 42;
        txn.rollback(pool, &held.db).unwrap();

        assert_eq!(held.db.wal.end(), start, "the log went back");
        assert_eq!(
            pool.fetch(PageId::new(file, page.page_no)).unwrap().bytes()[0],
            0,
            "the pool gave the page up"
        );
    }

    #[test]
    fn a_commit_writes_the_pages_and_a_read_only_commit_writes_nothing() {
        let (_dir, registry, held) = shop("commit");
        let table = held
            .db
            .catalog
            .lock()
            .unwrap()
            .table("items")
            .cloned()
            .expect("the table is there");
        let file = registry.table_file(&held.db, &table).unwrap();
        let pool = registry.pool();

        let txn = held.db.begin(TxnKind::ReadWrite, PATIENCE).unwrap();
        let page = pool.allocate(file).unwrap();
        pool.fetch(page).unwrap().bytes_mut()[0] = 42;
        txn.commit(pool, &held.db).unwrap();
        let committed = held.db.wal.committed();
        assert!(committed > 0, "the pages reached the log");

        held.db
            .begin(TxnKind::ReadOnly, PATIENCE)
            .unwrap()
            .commit(pool, &held.db)
            .unwrap();
        assert_eq!(held.db.wal.committed(), committed, "nothing was written");
    }

    #[test]
    fn the_oldest_reader_is_the_one_furthest_behind() {
        let readers = Arc::new(Readers::default());
        assert_eq!(readers.oldest(), None);

        let early = readers.lease(10);
        let late = readers.lease(30);
        let also_late = readers.lease(30);
        assert_eq!(readers.oldest(), Some(10));

        drop(early);
        assert_eq!(readers.oldest(), Some(30));
        drop(late);
        assert_eq!(readers.oldest(), Some(30), "one lease of that mark is left");
        drop(also_late);
        assert_eq!(readers.oldest(), None);
    }

    #[test]
    fn a_transaction_gives_up_its_mark_when_it_ends() {
        let dir = Dir::new("lease-ends");
        let (_registry, held) = dir.shop();

        let reader = held.db.begin(TxnKind::ReadOnly, PATIENCE).unwrap();
        assert_eq!(held.db.readers.oldest(), Some(held.db.wal.committed()));
        drop(reader);
        assert_eq!(held.db.readers.oldest(), None);
    }

    #[test]
    fn a_writer_of_one_database_leaves_another_alone() {
        let dir = Dir::new("two-databases");
        let registry = dir.registry();
        registry.create("shop").unwrap();
        registry.create("depot").unwrap();
        let shop = Registry::connect(&registry, "shop").unwrap();
        let depot = Registry::connect(&registry, "depot").unwrap();

        let held = shop.db.begin(TxnKind::ReadWrite, PATIENCE).unwrap();
        depot
            .db
            .begin(TxnKind::ReadWrite, BRIEF)
            .expect("another database has a lock of its own");
        drop(held);
    }

    #[test]
    fn a_lock_freed_by_one_thread_reaches_the_waiter_in_another() {
        let dir = Dir::new("hand-over");
        let (_registry, held) = dir.shop();
        let txn = held.db.begin(TxnKind::ReadWrite, PATIENCE).unwrap();

        let (took, waiting) = mpsc::channel();
        let db = Arc::clone(&held.db);
        let waiter = thread::spawn(move || {
            let answer = db.begin(TxnKind::ReadWrite, PATIENCE);
            took.send(()).expect("the test is listening");
            answer.map(|txn| txn.kind())
        });
        assert!(waiting.recv_timeout(BRIEF).is_err(), "the lock was held");

        drop(txn);
        assert_eq!(
            waiter.join().expect("the waiter ends").unwrap(),
            TxnKind::ReadWrite
        );
    }

    #[test]
    fn a_value_is_what_a_transaction_reads_at_its_own_mark() {
        let (_dir, registry, held) = shop("snapshot");
        let table = held
            .db
            .catalog
            .lock()
            .unwrap()
            .table("items")
            .cloned()
            .expect("the table is there");
        let file = registry.table_file(&held.db, &table).unwrap();
        let pool = registry.pool();
        let tree = crate::store::btree::BTree::open(pool, file, &table, Mark::Latest);
        tree.insert(
            &crate::store::row::store(pool, file, &table.columns, &[Value::Integer(1)]).unwrap(),
        )
        .unwrap();
        pool.commit(&held.db.wal).unwrap();

        let reader = held.db.begin(TxnKind::ReadOnly, PATIENCE).unwrap();
        let writer = held.db.begin(TxnKind::ReadWrite, PATIENCE).unwrap();
        tree.insert(
            &crate::store::row::store(pool, file, &table.columns, &[Value::Integer(2)]).unwrap(),
        )
        .unwrap();
        writer.commit(pool, &held.db).unwrap();

        let then = crate::store::btree::BTree::open(pool, file, &table, reader.mark());
        assert!(then.get(&Value::Integer(1)).unwrap().is_some());
        assert!(
            then.get(&Value::Integer(2)).unwrap().is_none(),
            "the row arrived after the reader's mark"
        );
    }
}
