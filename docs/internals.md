# GaribalDB — Internals

Version 1. Date 2026-09-09.

Types for each layer in `design.md`. Rust, so a "class" is a struct, an enum, or a trait.
Field types are shortened for the diagrams. `Vec~T~` reads as `Vec<T>`.

## Workspace Layout

```
Cargo.toml               workspace members
crates/
  protocol/              shared contract, no engine code
    src/
      lib.rs
      message.rs         ClientMsg, ServerMsg
      value.rs           Value, DataType, Decimal
      error.rs           ErrorCode, DbError
  server/                binary: garibaldb-server
    src/
      main.rs
      config.rs
      net/
        listener.rs      Server
        session.rs       Session
        cancel.rs        CancelRegistry
      sql/
        token.rs         Token
        lexer.rs         Lexer
        parser.rs        Parser
        ast.rs           Statement, Expr
      catalog/
        mod.rs           Catalog, TableDef, ColumnDef
      exec/
        planner.rs       plan()
        operator.rs      Operator trait
        scan.rs          SeqScan, PkScan
        filter.rs        Filter, Project, Limit
        sort.rs          Sort
      txn/
        manager.rs       Transaction, TxnKind
        lock.rs          WriteLock, WriteGuard
      wal/
        writer.rs        Wal, Frame
        index.rs         FrameIndex
        checkpoint.rs    Checkpointer
      store/
        pool.rs          BufferPool, PoolFrame
        page.rs          SlottedPage, PageHeader, PageId
        btree.rs         BTree, Cursor
        overflow.rs      OverflowChain
        codec.rs         encode_row, decode_row
        extsort.rs       ExternalSort, RunFile
  cli/                   binary: garibaldb
    src/
      main.rs            Cli
      conn.rs            Connection
      repl.rs            Repl
      render.rs          TableWriter
```

Dependencies go one way only.

```mermaid
graph LR
  CLI["cli"] --> PROTO["protocol"]
  SRV["server"] --> PROTO
```

`protocol` holds the message types, `Value`, `DataType`, `Decimal`, and `ErrorCode`. It has no
engine code and no `std::fs`. `cli` cannot reach a `BTree`, because it does not depend on `server`.

## 1. Connection Layer

```mermaid
classDiagram
    class Server {
        +config: Config
        +databases: Registry
        +cancels: CancelRegistry
        +run()
        +accept_loop()
    }
    class Session {
        +conn_id: u64
        +secret: String
        +db: ArcDatabase
        +txn: OptionTransaction
        +cancel: AtomicBool
        +run()
        +execute(sql) ServerMsg
        +on_disconnect()
    }
    class Config {
        +data_dir: PathBuf
        +port: u16
        +mem_limit: u64
        +wal_max_bytes: u64
        +lock_timeout_ms: u64
    }
    class Registry {
        +open: Map~String_ArcDatabase~
        +open_db(name) ArcDatabase
        +create_db(name)
        +drop_db(name)
        +conn_count(name) usize
    }
    class CancelRegistry {
        +map: Map~u64_CancelHandle~
        +register(conn_id) CancelHandle
        +cancel(conn_id, secret) bool
    }
    class ClientMsg {
        <<enumeration>>
        Startup
        Query
        Cancel
        Close
    }
    class ServerMsg {
        <<enumeration>>
        Ready
        RowDesc
        DataRow
        Complete
        Error
    }
    Server *-- Config
    Server *-- Registry
    Server *-- CancelRegistry
    Server --> Session : one thread each
    Session --> ClientMsg : reads
    Session --> ServerMsg : writes
```

- `Session::run` is the thread body. Read a message, run it, write the reply, repeat.
- `Session::on_disconnect` rolls back an open transaction. This gives `[FR63]`.
- `Registry::conn_count` lets `DROP DATABASE` refuse while a client is connected. This gives `[FR4]`.
- `cancel` is an `AtomicBool`. Operators check it between rows.

## 2. SQL Layer

```mermaid
classDiagram
    class Lexer {
        +input: str
        +pos: usize
        +next_token() Token
    }
    class Parser {
        +tokens: Vec~Token~
        +pos: usize
        +parse() Statement
        +parse_select() Statement
        +parse_expr() Expr
    }
    class Statement {
        <<enumeration>>
        CreateDatabase
        DropDatabase
        CreateTable
        DropTable
        Insert
        Select
        Update
        Delete
        Begin
        Commit
        Rollback
    }
    class Expr {
        <<enumeration>>
        Column
        Literal
        Compare
        And
        Or
        Not
        IsNull
        +eval(row) Value
    }
    class Value {
        <<enumeration>>
        Integer
        Text
        Boolean
        Decimal
        Null
        +type_of() DataType
        +compare(other) OptionOrdering
    }
    class Decimal {
        +units: i128
        +scale: u8
        +compare(other) Ordering
        +to_string() String
    }
    class Catalog {
        +path: PathBuf
        +tables: Map~String_TableDef~
        +load()
        +save_atomic()
        +add_table(def)
        +remove_table(name)
    }
    class TableDef {
        +id: u32
        +name: String
        +columns: Vec~ColumnDef~
        +pk_index: usize
    }
    class ColumnDef {
        +name: String
        +ty: DataType
        +not_null: bool
    }
    Lexer --> Parser : Token
    Parser --> Statement
    Statement *-- Expr
    Expr --> Value
    Value *-- Decimal
    Catalog *-- TableDef
    TableDef *-- ColumnDef
```

- `Value`, `DataType`, and `Decimal` live in the `protocol` crate, not in `server`. Both programs
  need them, and the CLI must render a `Decimal` without losing digits.
- `Value::compare` returns an option. A comparison with `Null` returns none, which gives `[FR36]`.
- `Decimal` holds a 128-bit integer and a scale, so 38 digits stay exact. This gives `[FR19]`.
- `Catalog::save_atomic` writes a temp file, `fsync`s it, renames it, then `fsync`s the directory.

## 3. Execution

```mermaid
classDiagram
    class Operator {
        <<interface>>
        +next() OptionRow
        +schema() Vec~ColumnDef~
    }
    class SeqScan {
        +cursor: Cursor
        +next() OptionRow
    }
    class PkScan {
        +btree: BTree
        +key_range: Range
        +next() OptionRow
    }
    class Filter {
        +input: BoxOperator
        +predicate: Expr
        +next() OptionRow
    }
    class Project {
        +input: BoxOperator
        +indices: Vec~usize~
        +next() OptionRow
    }
    class Sort {
        +input: BoxOperator
        +key: usize
        +desc: bool
        +sorter: ExternalSort
        +next() OptionRow
    }
    class Limit {
        +input: BoxOperator
        +remaining: u64
        +next() OptionRow
    }
    class Planner {
        +plan(stmt, catalog) BoxOperator
    }
    Operator <|.. SeqScan
    Operator <|.. PkScan
    Operator <|.. Filter
    Operator <|.. Project
    Operator <|.. Sort
    Operator <|.. Limit
    Planner --> Operator
```

- Every operator holds its input as `Box<dyn Operator>`. The chain pulls one row at a time.
- `Planner` picks `PkScan` when the `WHERE` names the primary key. Everything else gets `SeqScan`.
- `Sort` is the only operator that holds more than one row.

## 4. Transaction Layer

```mermaid
classDiagram
    class Database {
        +name: String
        +catalog: Catalog
        +lock: WriteLock
        +wal: Wal
        +begin(kind) Transaction
    }
    class Transaction {
        +kind: TxnKind
        +snapshot_frame: u64
        +start_frame: u64
        +guard: OptionWriteGuard
        +commit()
        +rollback()
        +check_writable()
    }
    class TxnKind {
        <<enumeration>>
        ReadOnly
        ReadWrite
    }
    class WriteLock {
        +inner: Mutex
        +timeout: Duration
        +acquire() WriteGuard
    }
    class Wal {
        +segments: Vec~File~
        +index: FrameIndex
        +next_lsn: u64
        +append_page(page_id, bytes) u64
        +append_commit()
        +sync()
        +truncate_to(frame)
        +read_page(page_id, max_frame) OptionBytes
        +size_bytes() u64
    }
    class FrameIndex {
        +map: Map~PageId_u64~
        +newest(page_id, max_frame) Optionu64
        +insert(page_id, frame)
    }
    class Checkpointer {
        +db: ArcDatabase
        +threshold: u64
        +run()
        +oldest_reader() u64
    }
    Database *-- WriteLock
    Database *-- Wal
    Transaction --> WriteLock : guard
    Wal *-- FrameIndex
    Checkpointer --> Wal
    Checkpointer --> BufferPool
```

- `begin(ReadWrite)` takes the lock. `begin(ReadOnly)` records `snapshot_frame` and takes nothing.
- `read_page(page_id, max_frame)` is what makes a snapshot work. A reader passes its own
  `snapshot_frame`, so it never sees a newer write.
- `rollback` calls `truncate_to(start_frame)`. Nothing reached the `.tbl` file, so there is no undo.
- `Checkpointer::oldest_reader` stops a checkpoint from passing a live snapshot.

## 5. Storage Layer

```mermaid
classDiagram
    class BufferPool {
        +frames: Vec~PoolFrame~
        +page_table: Map~PageId_usize~
        +clock_hand: usize
        +fetch(page_id) PageRef
        +fetch_for_write(page_id) PageMut
        +unpin(page_id, dirty)
        +evict() usize
        +flush_all()
    }
    class PoolFrame {
        +buf: Box~u8~
        +page_id: PageId
        +pin_count: u32
        +dirty: bool
        +ref_bit: bool
    }
    class PageId {
        +file: FileId
        +page_no: u32
    }
    class SlottedPage {
        +header: PageHeader
        +slot(i) Bytes
        +insert(bytes) u16
        +remove(i)
        +free_space() u16
    }
    class PageHeader {
        +lsn: u64
        +kind: u8
        +slot_count: u16
        +free_offset: u16
    }
    class BTree {
        +root: PageId
        +table: TableDef
        +get(key) OptionRow
        +insert(row)
        +delete(key)
        +cursor(range) Cursor
        +split(page)
        +merge(page)
    }
    class Cursor {
        +page: PageId
        +slot: u16
        +next() OptionRow
    }
    class OverflowChain {
        +head: PageId
        +read() Bytes
        +write(bytes) PageId
        +free()
    }
    class ExternalSort {
        +budget: usize
        +runs: Vec~RunFile~
        +add(row)
        +finish() Cursor
        +merge_pass()
    }
    class RunFile {
        +path: PathBuf
        +read_buf: Vec~u8~
        +head() OptionRow
        +advance()
    }
    BufferPool *-- PoolFrame
    PoolFrame --> PageId
    SlottedPage *-- PageHeader
    BTree --> BufferPool
    BTree --> Cursor
    BTree --> OverflowChain
    BTree --> SlottedPage
    ExternalSort --> RunFile
```

- One buffer pool for the whole server, and not one for each database. `[NFR10]` caps the
  memory of the server, and 100 databases with a pool each cannot hold inside it.
- A `FileId` is handed out when the pool first opens a `.tbl`, and never reaches the disk. So a
  `PageId` is unique across every database without any database needing a number of its own,
  which would need the server-level catalog that `design.md` rules out.
- `fetch` returns a pinned page. The caller must `unpin`. A pinned page is never evicted.
- `evict` uses the clock method. It sweeps frames, clears a set `ref_bit`, and takes the first
  frame with a clear bit and no pins.
- A page read goes to `Wal::read_page` first, then to the `.tbl` file. This is the snapshot rule.
- `OverflowChain` holds a value over 2 KB. The record in the page keeps a 12-byte pointer.

## 6. CLI

```mermaid
classDiagram
    class Cli {
        +args: Args
        +run()
    }
    class Connection {
        +stream: TcpStream
        +conn_id: u64
        +secret: String
        +startup(db)
        +query(sql) Stream
        +cancel()
    }
    class Repl {
        +editor: Editor
        +buffer: String
        +tx_state: String
        +run()
        +read_statement() String
    }
    class TableWriter {
        +widths: Vec~usize~
        +header(cols)
        +row(values)
        +footer(count)
    }
    Cli --> Connection
    Cli --> Repl
    Repl --> TableWriter
```

- `Repl::read_statement` collects lines until a `;`. `rustyline` gives history and editing.
- `Connection::cancel` opens a second socket and sends `Cancel` with the id and the secret.
- `TableWriter` prints rows as they arrive, so it measures widths from the first rows only.

## Errors

```mermaid
classDiagram
    class ErrorCode {
        <<enumeration>>
        SYNTAX_ERROR
        UNKNOWN_TABLE
        UNKNOWN_COLUMN
        TYPE_MISMATCH
        DUPLICATE_KEY
        NOT_NULL_VIOLATION
        STORAGE_FULL
        TXN_ABORTED
        READ_ONLY_TXN
        TXN_ALREADY_OPEN
        SCHEMA_CHANGE_IN_TXN
        LOCK_TIMEOUT
        CANCELLED
        TOO_MANY_CONNECTIONS
    }
    class DbError {
        +code: ErrorCode
        +message: String
        +position: Optionu32
    }
    DbError --> ErrorCode
```

Every fallible function returns `Result<T, DbError>`. The session turns a `DbError` into an
`Error` message and keeps the connection open.
