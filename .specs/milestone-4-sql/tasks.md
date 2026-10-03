# Milestone 4, SQL front end — Tasks

Source: `docs/projectplan.md` M4. Types: `docs/internals.md` section 2.
Requirements: [FR31]-[FR35], [FR67], [FR70]. Grammar: section 4 of `docs/requirements.md`.

Crate `crates/server`, module `src/sql/`. The milestone ends with a server that understands
every statement of section 4 and runs none of them. Execution arrives in milestone 8.

Settled before this list. Do not reopen.
- [H1] lives in `crates/server/tests/h1/`, a module beside `server.rs`. Every suite that needs
  it is server-side, so `design.md` keeps its three crates.
- `SELECT *` parses. Section 4 of `requirements.md` gains the form, so the document stays the
  authority and the code does not become it.
- The AST holds no `eval`. `internals.md` hangs `eval` on `Expr`, and milestone 8 adds it with
  the executor that needs it. This milestone builds the tree and nothing more.
- A position is a 1-based character offset, not a byte offset. The CLI prints
  `at character {n}`, and a multi-byte character must not shift the count.
- A keyword is case-insensitive. An identifier keeps the case it was written in and compares
  case-sensitively. No quoted identifiers, because they are out of scope.
- A string literal uses single quotes, and `''` inside one means a single quote. No backslash
  escapes, because the grammar has no need of them.
- The parser never reads the catalog. An unknown table or an unknown column belongs to
  milestones 5 and 8, so a statement that parses still answers `UNKNOWN_TABLE`.
- No arithmetic in an expression, and no `JOIN`, subquery, aggregate, or `DISTINCT`. Section 5.1
  of `requirements.md` rules each one out. A parser that accepts one is a bug.

## 1. Tokens and the lexer [serial]

- [ ] 1.1 Add `src/sql/mod.rs` declaring `token`, `lexer`, `parser`, and `ast`, and declare
  `mod sql` in `main.rs`. `cargo check` passes with the four empty modules.
- [ ] 1.2 Add `Token` to `src/sql/token.rs`. Variants for a keyword, an identifier, a number, a
  string, an operator, a punctuation mark, and the end of the input. Each one carries its
  1-based character position.
- [ ] 1.3 Add `Keyword` to `src/sql/token.rs`, one variant for each word section 4 uses.
  `from_str` folds case, so `select`, `SELECT`, and `Select` all read as the same keyword.
- [ ] 1.4 Add `Lexer` to `src/sql/lexer.rs`. `next_token` skips whitespace and returns the next
  token, or a `DbError` with `SYNTAX_ERROR` and the position. Gives [FR67].
- [ ] 1.5 Lex the operators `=`, `!=`, `<`, `<=`, `>`, `>=`, and the marks `(`, `)`, `,`, `;`,
  and `*`. A `!` that no `=` follows is an error that names its own position.
- [ ] 1.6 Lex a number, a single-quoted string that reads `''` as one quote, and an identifier.
  An unterminated string is an error at its opening quote. A test proves a position counts
  characters and not bytes.

## 2. The AST [parallel, needs 1.1]

- [ ] 2.1 Add `Expr` to `src/sql/ast.rs`. Variants `Column`, `Literal`, `Compare`, `And`, `Or`,
  `Not`, and `IsNull`. A `Literal` holds a `protocol::Value`, so the parser reuses `Decimal`.
- [ ] 2.2 Add `CompareOp` to `src/sql/ast.rs` with the six comparisons. Gives [FR33].
- [ ] 2.3 Add `Statement` to `src/sql/ast.rs`, with the eleven variants of section 4.
- [ ] 2.4 Add the parts that those variants hold: `ColumnSpec` for a `CREATE TABLE` column,
  `Selection` for `*` or a named list, `Assignment` for one `SET`, and `OrderBy` with its
  direction. Gives [FR31] and [FR37].
- [ ] 2.5 Derive `Debug` and `PartialEq` on every type in `src/sql/ast.rs`, so one assertion
  compares a whole tree.
- [ ] 2.6 Add the `*` form to the `SELECT` row of section 4 in `docs/requirements.md`.

## 3. The parser [serial, needs 1 and 2]

- [ ] 3.1 Add `Parser` to `src/sql/parser.rs`. It holds the tokens and a position. `parse` reads
  one statement and then the end of the input, so trailing text is an error.
- [ ] 3.2 Add `parse_expr`, by precedence: `OR`, then `AND`, then `NOT`, then a comparison, then
  `IS NULL` and `IS NOT NULL`, then a literal, a column, or a parenthesised expression. Gives
  [FR33], [FR34], and [FR35].
- [ ] 3.3 Parse the DDL: `CREATE DATABASE`, `DROP DATABASE`, `DROP TABLE`, and `CREATE TABLE`
  with its column list. A column type reads through `protocol::DataType::from_str`, which
  already refuses a bad precision.
- [ ] 3.4 Parse `SELECT`, with `*` or a column list, `FROM`, and the optional `WHERE`,
  `ORDER BY` with `ASC` or `DESC`, and `LIMIT`. Parse `DELETE FROM` with its optional `WHERE`.
  Gives [FR31], [FR32], [FR37], and [FR40].
- [ ] 3.5 Parse `INSERT INTO` with more than one row of values, `UPDATE` with its `SET` list,
  and `BEGIN`, `BEGIN READ ONLY`, `COMMIT`, and `ROLLBACK`.
- [ ] 3.6 Test the parser. Every form of section 4 parses to the tree the test names, a
  statement from section 5.1 is refused, and a malformed statement gives `SYNTAX_ERROR` at the
  character the test names.

## 4. The server speaks SQL [serial, needs 3]

- [ ] 4.1 Parse the `sql` of a `Query` in `src/net/session.rs`. A parse failure answers
  `Error(SYNTAX_ERROR)` with its position, and the connection stays open. Gives [FR67],
  [FR70], and the position that [FR69] needs to be readable.
- [ ] 4.2 A statement that parses answers `Error(UNKNOWN_TABLE)`, because nothing executes it
  until milestone 8. A `Ready` follows either answer.
- [ ] 4.3 Update the `session.rs` unit tests that send any text and expect `UNKNOWN_TABLE`, so
  each one sends a statement that parses.
- [ ] 4.4 Extend `crates/server/tests/server.rs`. Over a real socket, a malformed statement
  answers `SYNTAX_ERROR` with a position, a statement of section 4 answers `UNKNOWN_TABLE`, and
  a `Ready` follows both.

## 5. The statement runner [serial, needs 4]

- [ ] 5.1 Add `crates/server/tests/h1/mod.rs` and move the `Server`, `Conn`, and `wait_log`
  helpers out of `server.rs` into it. `server.rs` declares `mod h1` and uses them, and its six
  tests still pass unchanged.
- [ ] 5.2 Add the case-file reader to that module. It reads `statement ok`, `statement error
  CODE`, and `query` with its `----` separator and the rows that follow, which are the three
  forms that section 2.1 of `testplan.md` shows.
- [ ] 5.3 Add the runner. It starts one server for each file, sends each statement in order, and
  compares the answer. A mismatch names the file, the line, the statement, the expected result,
  and the result that arrived, then fails the test. Gives [TR6].
- [ ] 5.4 Add `crates/server/tests/statements.rs`. It walks every `.test` file in
  `crates/server/tests/cases/` and runs it, so a later milestone adds a file and no test.
- [ ] 5.5 Add `crates/server/tests/cases/syntax.test`. It holds every statement form of section
  4 as `statement error UNKNOWN_TABLE`, and a set of malformed statements as
  `statement error SYNTAX_ERROR`. Every case passes.

## Dependencies

- 1.1 blocks everything.
- 2 runs beside 1, because the AST holds no token.
- 3 needs 1 and 2.
- 4 needs 3.
- 5 needs 4. Task 5.1 only moves code, so it can land as soon as 4.4 is in.

## Done when

- Every statement form of section 4 parses, and a test names the tree it parses to.
- A malformed statement answers `SYNTAX_ERROR` with the character that broke it, over a real
  socket, and the connection stays open.
- A statement that parses answers `UNKNOWN_TABLE`, because nothing runs it yet.
- [H1] runs a file of cases and reports each failure with its line.
- `cargo test --workspace` passes. Clippy is clean with `-D warnings`.
