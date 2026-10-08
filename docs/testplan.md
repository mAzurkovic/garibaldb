# GaribalDB — Test Plan

Version 1. Date 2026-09-09.

This document says how the project proves that each requirement in `requirements.md` holds.
It tests the server from the outside only. It does not read internal state.
A test that reads internal state proves the design, not the requirement.

> The key words "MUST", "MUST NOT", "SHOULD", "SHOULD NOT", and "MAY" in this document
> are to be interpreted as described in
> [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119#section-1) and
> [RFC 8174](https://www.rfc-editor.org/rfc/rfc8174#section-2).

## 1. Rules

- [TR1] Each requirement MUST map to at least one test. A requirement that binds the operator, not the system, is the one exception.
- [TR2] Each test MUST name the requirements that it proves.
- [TR3] A test MUST use the network protocol or the CLI. A test MUST NOT read a data file.
- [TR4] A test MUST give the same result each time it runs.
- [TR5] A test MUST start from an empty server.
- [TR6] The build MUST fail if a test for a MUST requirement fails.
- [TR7] The build MUST report, but MUST NOT fail, if a test for a SHOULD requirement fails.

## 2. Test Harnesses

| Id | Harness | Purpose |
|---|---|---|
| [H1] | Statement runner | Sends SQL. Compares the result to an expected result. |
| [H2] | Concurrency runner | Runs many clients. Controls the order of their statements. |
| [H3] | Crash runner | Stops the server at a chosen moment. Starts it again. |
| [H4] | Load runner | Sends many statements. Measures latency and throughput. |
| [H5] | Data generator | Writes large tables. |
| [H6] | Resource monitor | Measures the memory that the server uses. |

### 2.1 Statement runner [H1]

Each test is a text file. A file holds statements and the expected result of each one.
The runner sends each statement and compares the result. This is the same idea as
`sqllogictest`, which SQLite uses.

```
statement ok
CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT NOT NULL)

statement error DUPLICATE_KEY
INSERT INTO t (a, b) VALUES (1, 'x'), (1, 'y')

query
SELECT a, b FROM t ORDER BY a
----
(0 rows)
```

### 2.2 Concurrency runner [H2]

The runner opens many connections. A test script says which client sends which statement,
and in which order. The runner blocks a client until the script permits it to continue.
This makes an interleaving that repeats.

### 2.3 Crash runner [H3]

Two failure types. They prove different requirements.

| Type | Method | Proves |
|---|---|---|
| Process crash | `kill -9` on the server | [FR56] Data survives a restart |
| Power loss | Block device fault injection | [FR57] Data survives a power failure |

`kill -9` does not prove [FR57]. The operating system still holds the written data,
and it writes that data to the disk after the process stops. To prove [FR57] a test MUST
discard every write that the server did not force to the disk.

As built, [T10] to [T13] hold in two forms. A `kill -9` of the server proves [FR56] on any
machine. A test that shortens or corrupts a log file proves that recovery copes with a tail
that never committed, a frame cut in half, and a checksum that disagrees. The block device
form below is not built, so [FR57] against a real disk is not yet proven, and a release has to
say so.

Recommended method: the Linux `dm-log-writes` device. It records each write and each
flush. A test then rebuilds the disk as it was at any earlier moment, and starts the
server on that disk. `dm-flakey` is a simpler option that drops unflushed writes.

### 2.4 Load runner [H4]

Sends a fixed mix of statements from a set count of clients for a set time.
Reports throughput and the P50, P95, and P99 latency.

### 2.5 Data generator [H5]

Writes rows until a table reaches a target size or a target row count.
Used for the capacity tests and the scan tests. These tests run once a week, not on each build.

### 2.6 Resource monitor [H6]

Runs the server inside a memory limit. Reports the peak memory that the server used.
A test fails if the operating system stops the server for using too much memory.

## 3. Test Suites

| Suite | Requirements | Harness | Runs |
|---|---|---|---|
| [S1] Database lifecycle | FR1–FR5 | H1 | Each build |
| [S2] Table lifecycle | FR6–FR13 | H1 | Each build |
| [S3] Data types | FR14–FR21 | H1 | Each build |
| [S4] Row operations | FR22–FR30 | H1 | Each build |
| [S5] Queries | FR31–FR43 | H1 | Each build |
| [S6] Transactions, one client | FR44–FR50, FR53, FR55 | H1 | Each build |
| [S7] Isolation | FR51, FR52, FR54 | H2 | Each build |
| [S8] Durability | FR56–FR60 | H3 | Each build |
| [S9] Connections | FR61–FR66 | H2 | Each build |
| [S10] Errors | FR67–FR70 | H1 | Each build |
| [S11] CLI | FR71–FR80 | H1 | Each build |
| [S12] Logging | FR81–FR83 | H1 | Each build |
| [S13] Capacity | NFR1–NFR9 | H5 | Each week |
| [S14] Memory | NFR10–NFR13 | H6 | Each week |
| [S15] Performance | NFR14–NFR17 | H4 | Each week |
| [S16] Availability | NFR18–NFR19 | H3, H5 | Each week |
| [S17] Security | NFR20 | H1 | Each build |

[NFR21] binds the operator, not the system. No test covers it. See [TR1].

## 4. The Hard Properties

The suites above are simple, except these five. Each one needs a stated method.

### 4.1 Atomicity — [FR49]

- [T1] Start a transaction. Change 100 rows. Roll back. Read every row. No row changed.
- [T2] Start a transaction. Change 100 rows. Break the connection. Read every row. No row changed.
- [T3] Start a transaction. Change 100 rows. Stop the server. Start it again. No row changed.
- [T4] Start a transaction. Change 100 rows. Add a row with a duplicate primary key.
  The statement fails. Roll back. No row changed.

### 4.2 Isolation — [FR51]

Use the known anomaly tests. Each test names an anomaly that a serializable system MUST prevent.
The `Hermitage` test set is a public source for these.

| Anomaly | Test |
|---|---|
| Dirty write | Two clients change the same row. Both commit. One change survives whole. |
| Dirty read | Client A changes a row. Client B reads it before A commits. B sees the old value. |
| Non-repeatable read | Client A reads a row twice. Client B changes it between the reads. A sees one value. |
| Phantom read | Client A counts rows twice. Client B adds a matching row between the counts. A sees one count. |
| Lost update | Two clients read a value, add 1, and write it. The value grows by 2, or one client aborts. |
| Write skew | Two clients each read a set and change a different row. The result matches some serial order. |

- [T5] Each anomaly above MUST NOT occur.
- [T6] Run 1000 random transactions from 20 clients. Record every statement and every result.
  A serial order of those transactions MUST exist that gives the same results.
- [T7] When the system aborts a transaction, it MUST return the retry code. A client that
  retries MUST succeed in the end.

### 4.3 Consistency — [FR50]

- [T8] Start a transaction. Add a row. Delete the row that holds the same primary key.
  Add the row again. Commit. The commit MUST succeed.
- [T9] Start a transaction. Break a rule of a table. The statement MUST fail.
  Commit. The commit MUST leave no row that breaks a rule.

### 4.4 Durability — [FR57], [FR58]

- [T10] Commit 1000 transactions. Cut the power at a random moment. Start the server.
  Every transaction that reported success MUST be present.
- [T11] Repeat [T10] 100 times. Cut the power at a different moment each time.
- [T12] A transaction that did not report success MUST be present whole, or absent whole.
  It MUST NOT be present in part.
- [T13] Cut the power during recovery. Start the server again. Recovery MUST complete.

### 4.5 Capacity — [NFR1], [NFR2], [NFR11]

- [T14] Write 100 GB into one table. Read one row by primary key. The read MUST succeed.
- [T15] Write 1 billion rows into one table. Count them. The count MUST be correct.
- [T16] Measure the server memory at 1 GB of data and at 100 GB of data.
  The two numbers MUST be close. Memory MUST NOT grow with the data.
- [T17] Write past a limit in section 3.1 of `requirements.md`.
  The write MUST fail with the storage full code. A read after it MUST succeed.

## 5. Compliance

- The build is **conditionally compliant** when every test for a MUST requirement passes.
- The build is **unconditionally compliant** when every test passes.
- A release MUST be at least conditionally compliant.
- A release MUST list each SHOULD requirement that it does not meet.

## 6. Out of Scope

Load tests against other databases · tests of the wire format · tests that read a data file ·
security tests · tests of more than one server
