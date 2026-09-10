# Milestone 1, Protocol — Tasks

Source: `docs/projectplan.md` M1. Types: `docs/internals.md` sections 1 and 2.
Requirements: [FR14]-[FR21], [FR67]-[FR69].

Crate `crates/protocol`. A library. No file I/O, no socket, no engine code.

## 1. Workspace and crate skeleton [serial]

- [x] 1.1 Add the root `Cargo.toml`. Workspace with member `crates/protocol`. Edition 2024. `cargo check` passes.
- [x] 1.2 Add `crates/protocol/Cargo.toml`. Depends on `serde` with `derive`, and `serde_json`. Nothing else.
- [x] 1.3 Add `crates/protocol/src/lib.rs`. Declares `mod value`, `mod error`, `mod message`. Re-exports each public type.

## 2. Values and types [serial, needs 1]

- [x] 2.1 Add `DataType` to `src/value.rs`. Variants `Integer`, `Text`, `Boolean`, `Decimal { p: u8, s: u8 }`. Serde as a string.
- [x] 2.2 Add `Decimal` to `src/value.rs`. Fields `units: i128` and `scale: u8`. `from_str` and `to_string` keep every digit.
- [x] 2.3 Add `Decimal::fits(p, s)`. Returns false when the value needs more than `p` digits, or more than `s` after the point. Gives [FR20].
- [x] 2.4 Add `Decimal::compare`. Aligns scales before it compares, so `12.20` equals `12.2`. Gives [FR39].
- [x] 2.5 Add serde for `Decimal`. Writes a JSON string, never a JSON number. Reads a string. Gives [FR19].
- [x] 2.6 Add `Value` to `src/value.rs`. Variants `Integer(i64)`, `Text(String)`, `Boolean(bool)`, `Decimal(Decimal)`, `Null`. Add `type_of` and `compare`.

## 3. Errors [parallel, needs 1]

- [x] 3.1 Add `ErrorCode` to `src/error.rs`. Ten variants: `SYNTAX_ERROR`, `UNKNOWN_TABLE`, `UNKNOWN_COLUMN`, `TYPE_MISMATCH`, `DUPLICATE_KEY`, `NOT_NULL_VIOLATION`, `STORAGE_FULL`, `TXN_ABORTED`, `SCHEMA_CHANGE_IN_TXN`, `LOCK_TIMEOUT`.
- [x] 3.2 Serde `ErrorCode` as its screaming-snake name. The name is the wire contract and does not change. Gives [FR68].
- [x] 3.3 Add `DbError` to `src/error.rs`. Fields `code: ErrorCode`, `message: String`, `position: Option<u32>`. Implements `std::error::Error`. Gives [FR69].

## 4. Messages [serial, needs 2 and 3]

- [x] 4.1 Add `ClientMsg` to `src/message.rs`. Variants `Startup { version: u16, database: String }`, `Query { sql: String }`, `Cancel { conn_id: u64, secret: String }`, `Close`.
- [x] 4.2 Add `ColumnDesc` and `ServerMsg` to `src/message.rs`. Variants `Ready`, `RowDesc`, `DataRow`, `Complete`, `Error`.
- [x] 4.3 Tag both enums with `#[serde(tag = "type", rename_all = "lowercase")]`. A message carries a `type` field.
- [x] 4.4 Add `to_line` and `from_line` to both enums. `to_line` returns a `String` with no inner newline. `from_line` parses one line.
- [x] 4.5 Add `PROTOCOL_VERSION: u16 = 1`. `Startup` carries it, and the server refuses a version it does not know.

## 5. Tests [serial, needs 4]

- [x] 5.1 Round-trip test for each `ClientMsg` and `ServerMsg` variant. `from_line(to_line(m))` equals `m`.
- [x] 5.2 Decimal test. `1002.2` and a 38-digit value survive a JSON round trip unchanged. Gives [FR19].
- [x] 5.3 Comparison test. `Value::compare` with a `Null` on either side returns `None`. Gives [FR36].
- [x] 5.4 Decimal order test. `12.20` compares equal to `12.2`. `2.10` is less than `10.2`.
- [x] 5.5 Bad input test. An unknown `type`, a missing field, and a JSON number for a decimal each return an error. No panic.

## Dependencies

- 1 blocks 2, 3, 4, 5.
- 2 and 3 run at the same time.
- 4 needs 2 and 3.
- 5 needs 4.

## Done when

- `cargo test -p protocol` passes.
- The crate has no `std::fs` and no `std::net`.
- A `Decimal` keeps 38 digits through a JSON round trip.
