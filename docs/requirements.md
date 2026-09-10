# GaribalDB — Requirements

Version 1. Date 2026-09-09.

GaribalDB is a relational database server. Clients connect over a network and send SQL.
This document states what the system must do. It does not state how to build it.
Design decisions belong in `design.md`. Acceptance tests belong in `testplan.md`.

> The key words "MUST", "MUST NOT", "SHOULD", "SHOULD NOT", and "MAY" in this document
> are to be interpreted as described in
> [RFC 2119](https://www.rfc-editor.org/rfc/rfc2119#section-1) and
> [RFC 8174](https://www.rfc-editor.org/rfc/rfc8174#section-2).

## 1. User Stories

- As an application developer, I want to store rows in tables, so my program keeps data after it stops.
- As an application developer, I want transactions, so a group of changes applies fully or not at all.
- As an application developer, I want a remote server, so more than one program can share the same data.
- As an operator, I want automatic recovery, so the database returns after a power failure without my help.

## 2. Functional Requirements

### 2.1 Database lifecycle

- [FR1] A user MUST be able to create a database.
- [FR2] A user MUST be able to delete a database.
- [FR3] The system MUST refuse to create a database that exists.
- [FR4] The system MUST refuse to delete a database while a client is connected to it.
- [FR5] A client MUST see only the tables in the database it connected to.

### 2.2 Table lifecycle

- [FR6] A user MUST be able to create a table.
- [FR7] A user MUST give a name and a data type for each column.
- [FR8] A user MUST be able to make one column the primary key.
- [FR9] A user MUST be able to mark a column `NOT NULL`.
- [FR10] A user MUST be able to delete a table.
- [FR11] The system MUST delete all rows of a table when it deletes the table.
- [FR12] The system MUST refuse to create a table that exists.
- [FR13] The system MUST refuse an operation on a table that does not exist.

### 2.3 Data types

- [FR14] The system MUST support `INTEGER`. The range is a signed 64-bit number.
- [FR15] The system MUST support `TEXT`. The encoding is UTF-8.
- [FR16] The system MUST support `BOOLEAN`.
- [FR17] The system MUST support `DECIMAL(p, s)`. `p` is the count of digits. `s` is the count of digits after the point.
- [FR18] `p` MUST NOT be more than 38. `s` MUST NOT be more than `p`.
- [FR19] The system MUST store a decimal value exactly. A value that a user writes MUST come back unchanged.
- [FR20] The system MUST refuse a decimal value with more digits than the column permits.
- [FR21] The system MUST store an empty value in a column that permits one.

### 2.4 Row operations

- [FR22] A user MUST be able to add one row to a table.
- [FR23] A user MUST be able to add more than one row with one statement.
- [FR24] A user MUST be able to read rows from a table.
- [FR25] A user MUST be able to change rows in a table.
- [FR26] A user MUST be able to delete rows from a table.
- [FR27] A change or a delete MUST apply to every row that matches the condition.
- [FR28] The system MUST refuse a row with a duplicate primary key. (**Consistency**)
- [FR29] The system MUST refuse a row with no value in a `NOT NULL` column. (**Consistency**)
- [FR30] The system MUST refuse a value of the wrong data type. (**Consistency**)

### 2.5 Queries

- [FR31] A user MUST be able to choose which columns the result contains.
- [FR32] A user MUST be able to filter rows with a condition.
- [FR33] A condition MUST support these comparisons: `=`, `!=`, `<`, `<=`, `>`, `>=`.
- [FR34] A condition MUST support `AND`, `OR`, and `NOT`.
- [FR35] A condition MUST support `IS NULL` and `IS NOT NULL`.
- [FR36] A comparison with an empty value MUST be neither true nor false. The row MUST NOT appear in the result.
- [FR37] A user MUST be able to sort the result by a column.
- [FR38] The system MUST sort `TEXT` by the byte order of the UTF-8 encoding.
- [FR39] The system MUST sort and compare `DECIMAL` by numeric value. `12.20` and `12.2` MUST compare equal.
- [FR40] A user MUST be able to limit the count of rows in the result.
- [FR41] The system MUST return every row that matches the condition.
- [FR42] The system MUST return a result that is larger than the memory of the server.
- [FR43] The system MUST return the name of each column with the result.

### 2.6 Transactions

- [FR44] A user MUST be able to start a transaction.
- [FR45] A user MUST declare a transaction read-only or read-write when it starts.
- [FR46] The system MUST refuse a change from a read-only transaction.
- [FR47] A user MUST be able to commit a transaction.
- [FR48] A user MUST be able to roll back a transaction. (**Atomicity**)
- [FR49] The system MUST apply all changes of a transaction, or none of them. (**Atomicity**)
- [FR50] The system MUST NOT commit a transaction that leaves a row that breaks a rule of its table. The rules are the primary key, `NOT NULL`, and the data type. (**Consistency**)
- [FR51] The result of transactions that run at the same time MUST equal the result of the same transactions run one after the other, in some order. (**Isolation**)
- [FR52] A transaction MUST NOT change data in more than one database.
- [FR53] A connection MUST NOT have more than one open transaction.
- [FR54] The system MUST abort a transaction that it cannot complete. The system MUST return a code that tells the client to try again. (**Isolation**)
- [FR55] The system MUST refuse a statement that creates or deletes a database or a table while a transaction is open.

### 2.7 Durability and recovery

- [FR56] The system MUST keep committed data after a restart. (**Durability**)
- [FR57] The system MUST keep committed data after a power failure. (**Durability**)
- [FR58] The system MUST discard the changes of a transaction that did not commit. (**Atomicity**)
- [FR59] The system MUST recover the data at startup. An operator MUST NOT need to act. (**Durability**)
- [FR60] The system MUST accept connections only after recovery is complete. (**Durability**)

### 2.8 Client connections

- [FR61] A client MUST be able to connect to the server over a network.
- [FR62] A client MUST give one database name when it connects.
- [FR63] The system MUST accept connections from more than one client at the same time.
- [FR64] The system MUST give each connection its own session.
- [FR65] The system MUST roll back an open transaction when its connection closes.
- [FR66] The system MUST let a client stop a statement that is running.

### 2.9 Errors

- [FR67] The system MUST return an error for a statement it cannot run.
- [FR68] Each error MUST have a stable code and a message.
- [FR69] A failed statement MUST NOT change data.
- [FR70] The system MUST return a distinct code for each of these failures:
  - bad syntax
  - unknown table
  - unknown column
  - wrong data type
  - duplicate primary key
  - missing value in a `NOT NULL` column
  - storage full
  - transaction aborted
  - schema change while a transaction is open
  - too many connections

### 2.10 CLI client

- [FR71] The CLI MUST connect to a server by host, port, and database name.
- [FR72] The CLI MUST accept a statement typed over more than one line.
- [FR73] A `;` MUST end a statement.
- [FR74] The CLI MUST show a result as a table with column names.
- [FR75] The CLI MUST show rows as they arrive. The CLI MUST NOT wait for the last row.
- [FR76] The CLI MUST show the error code and the message when a statement fails.
- [FR77] The CLI MUST run one statement given on the command line, then stop.
- [FR78] The CLI MUST stop with a status code that shows success or failure.
- [FR79] The CLI MUST let the user stop a slow query. The connection MUST stay open.
- [FR80] The CLI SHOULD show the user when a transaction is open.

### 2.11 Server logging

- [FR81] The server SHOULD write a log entry for each new connection.
- [FR82] The server SHOULD write a log entry for each error.
- [FR83] The server SHOULD write a log entry when recovery starts and when it completes.

## 3. Non-Functional Requirements

### 3.1 Capacity

- [NFR1] One table MUST hold 100 GB of data.
- [NFR2] One table MUST hold 1 billion rows.
- [NFR3] One database MUST hold 1000 tables.
- [NFR4] One server MUST hold 100 databases.
- [NFR5] One table MUST hold 100 columns.
- [NFR6] One row MUST hold 1 MB of data.
- [NFR7] One value MUST hold 1 MB of data.
- [NFR8] The system MUST refuse a write that passes a limit. The system MUST return the storage full code.
- [NFR9] The system MUST keep serving reads after it refuses a write.

### 3.2 Memory

- [NFR10] The server MUST run inside a memory limit that the operator sets. The default limit is 1 GB.
- [NFR11] The memory that the server uses MUST NOT depend on the size of the data.
- [NFR12] The server MUST hold every capacity in section 3.1 inside the memory limit.
- [NFR13] The memory that the server uses for one connection MUST NOT exceed 10 MB.

### 3.3 Performance

- [NFR14] The system MUST accept 100 client connections at the same time.
- [NFR15] The system SHOULD read one row by primary key in under 10 ms at P99.
- [NFR16] The system SHOULD commit 1000 write transactions per second, counted across all clients.
- [NFR17] The system SHOULD read 1 million rows per second in a full table scan.

### 3.4 Availability

- [NFR18] The system MUST NOT lose a transaction that it reported as committed. (**Durability**)
- [NFR19] The system SHOULD complete recovery after a crash in under 60 seconds.

### 3.5 Security

- [NFR20] Version 1 MUST NOT require a client to prove who it is.
- [NFR21] The operator MUST run the server on a trusted network.

**Accepted risk:** any client that reaches the server port has full access to all data.

## 4. SQL Statements in Scope

| Statement | Form |
|---|---|
| `CREATE DATABASE` | `CREATE DATABASE name` |
| `DROP DATABASE` | `DROP DATABASE name` |
| `CREATE TABLE` | `CREATE TABLE t (col type [PRIMARY KEY] [NOT NULL], ...)` |
| `DROP TABLE` | `DROP TABLE t` |
| `INSERT` | `INSERT INTO t (cols) VALUES (...), (...)` |
| `SELECT` | `SELECT cols FROM t [WHERE cond] [ORDER BY col [ASC\|DESC]] [LIMIT n]` |
| `UPDATE` | `UPDATE t SET col = val, ... [WHERE cond]` |
| `DELETE` | `DELETE FROM t [WHERE cond]` |
| `BEGIN` | `BEGIN [READ ONLY]` |
| `COMMIT` | `COMMIT` |
| `ROLLBACK` | `ROLLBACK` |

Column types: `INTEGER`, `TEXT`, `BOOLEAN`, `DECIMAL(p, s)`.

## 5. Out of Scope

### 5.1 SQL features

`JOIN` · aggregate functions · `GROUP BY` · subqueries · `DISTINCT` · `ALTER TABLE` ·
`CREATE INDEX` · views · triggers · stored procedures · foreign keys ·
`UNIQUE` other than the primary key · scalar functions · arithmetic in an expression ·
`IF EXISTS` · `IF NOT EXISTS` · prepared statements and parameter binding ·
a query across two databases · binary floating-point types (`REAL`, `DOUBLE`) ·
date and time types

### 5.2 System features

authentication · encrypted network traffic · user roles and permissions · replication ·
backup and restore · schema migration · reading statements from a file in the CLI ·
more than one transaction on one connection
