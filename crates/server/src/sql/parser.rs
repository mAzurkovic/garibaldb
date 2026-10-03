//! The parser. Tokens in, a statement out, by recursive descent.
//!
//! Precedence runs `OR`, `AND`, `NOT`, then a comparison, which is the order
//! SQL gives them. So `NOT a = 1` reads as `NOT (a = 1)`.

use protocol::{DataType, DbError, Decimal, Value};

use crate::sql::ast::{Assignment, ColumnSpec, CompareOp, Expr, OrderBy, Selection, Statement};
use crate::sql::lexer;
use crate::sql::syntax_error;
use crate::sql::token::{Keyword, Punct, Token, TokenKind};

/// Reads one statement. A trailing `;` is allowed, and text after it is not.
pub fn parse(sql: &str) -> Result<Statement, DbError> {
    Parser {
        tokens: lexer::tokens(sql)?,
        pos: 0,
    }
    .statement_and_end()
}

struct Parser {
    /// Always ends with `End`, so the cursor always has a token to read.
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn statement_and_end(&mut self) -> Result<Statement, DbError> {
        let statement = self.statement()?;
        self.eat_punct(Punct::Semicolon);
        match self.peek() {
            TokenKind::End => Ok(statement),
            _ => Err(self.unexpected("the end of the statement")),
        }
    }

    fn statement(&mut self) -> Result<Statement, DbError> {
        match self.peek_keyword() {
            Some(Keyword::Create) => {
                self.bump();
                self.create()
            }
            Some(Keyword::Drop) => {
                self.bump();
                self.drop_object()
            }
            Some(Keyword::Insert) => {
                self.bump();
                self.insert()
            }
            Some(Keyword::Select) => {
                self.bump();
                self.select()
            }
            Some(Keyword::Update) => {
                self.bump();
                self.update()
            }
            Some(Keyword::Delete) => {
                self.bump();
                self.delete()
            }
            Some(Keyword::Begin) => {
                self.bump();
                self.begin()
            }
            Some(Keyword::Commit) => {
                self.bump();
                Ok(Statement::Commit)
            }
            Some(Keyword::Rollback) => {
                self.bump();
                Ok(Statement::Rollback)
            }
            _ => Err(self.unexpected("a statement")),
        }
    }

    fn create(&mut self) -> Result<Statement, DbError> {
        if self.eat_keyword(Keyword::Database) {
            return Ok(Statement::CreateDatabase { name: self.name()? });
        }
        self.expect_keyword(Keyword::Table)?;
        let name = self.name()?;
        self.expect_punct(Punct::LParen)?;
        let mut columns = vec![self.column_spec()?];
        while self.eat_punct(Punct::Comma) {
            columns.push(self.column_spec()?);
        }
        self.expect_punct(Punct::RParen)?;
        Ok(Statement::CreateTable { name, columns })
    }

    /// A column, its type, and the constraints in either order.
    fn column_spec(&mut self) -> Result<ColumnSpec, DbError> {
        let name = self.name()?;
        let ty = self.data_type()?;
        let mut primary_key = false;
        let mut not_null = false;
        loop {
            if self.eat_keyword(Keyword::Primary) {
                self.expect_keyword(Keyword::Key)?;
                primary_key = true;
            } else if self.eat_keyword(Keyword::Not) {
                self.expect_keyword(Keyword::Null)?;
                not_null = true;
            } else {
                return Ok(ColumnSpec {
                    name,
                    ty,
                    primary_key,
                    not_null,
                });
            }
        }
    }

    /// A column type. `DECIMAL` carries a precision and a scale, and the type
    /// itself refuses a pair it cannot hold.
    fn data_type(&mut self) -> Result<DataType, DbError> {
        let position = self.position();
        let name = self.name()?;
        let text = match name.eq_ignore_ascii_case("decimal") {
            false => name,
            true => {
                self.expect_punct(Punct::LParen)?;
                let precision = self.number()?;
                self.expect_punct(Punct::Comma)?;
                let scale = self.number()?;
                self.expect_punct(Punct::RParen)?;
                format!("DECIMAL({precision},{scale})")
            }
        };
        text.parse()
            .map_err(|reason: String| syntax_error(reason, position))
    }

    fn drop_object(&mut self) -> Result<Statement, DbError> {
        if self.eat_keyword(Keyword::Database) {
            return Ok(Statement::DropDatabase { name: self.name()? });
        }
        self.expect_keyword(Keyword::Table)?;
        Ok(Statement::DropTable { name: self.name()? })
    }

    fn insert(&mut self) -> Result<Statement, DbError> {
        self.expect_keyword(Keyword::Into)?;
        let table = self.name()?;
        self.expect_punct(Punct::LParen)?;
        let columns = self.name_list()?;
        self.expect_punct(Punct::RParen)?;
        self.expect_keyword(Keyword::Values)?;
        let mut rows = vec![self.value_row()?];
        while self.eat_punct(Punct::Comma) {
            rows.push(self.value_row()?);
        }
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn value_row(&mut self) -> Result<Vec<Value>, DbError> {
        self.expect_punct(Punct::LParen)?;
        let mut row = vec![self.literal()?];
        while self.eat_punct(Punct::Comma) {
            row.push(self.literal()?);
        }
        self.expect_punct(Punct::RParen)?;
        Ok(row)
    }

    fn select(&mut self) -> Result<Statement, DbError> {
        let selection = match self.eat_punct(Punct::Star) {
            true => Selection::All,
            false => Selection::Columns(self.name_list()?),
        };
        self.expect_keyword(Keyword::From)?;
        let table = self.name()?;
        let filter = self.filter()?;
        let order_by = self.order_by()?;
        let limit = match self.eat_keyword(Keyword::Limit) {
            true => Some(self.row_count()?),
            false => None,
        };
        Ok(Statement::Select {
            table,
            selection,
            filter,
            order_by,
            limit,
        })
    }

    fn order_by(&mut self) -> Result<Option<OrderBy>, DbError> {
        if !self.eat_keyword(Keyword::Order) {
            return Ok(None);
        }
        self.expect_keyword(Keyword::By)?;
        let column = self.name()?;
        let descending = self.eat_keyword(Keyword::Desc);
        if !descending {
            // Ascending is the order the tree already holds, so the word only
            // has to be allowed.
            let _ = self.eat_keyword(Keyword::Asc);
        }
        Ok(Some(OrderBy { column, descending }))
    }

    fn update(&mut self) -> Result<Statement, DbError> {
        let table = self.name()?;
        self.expect_keyword(Keyword::Set)?;
        let mut assignments = vec![self.assignment()?];
        while self.eat_punct(Punct::Comma) {
            assignments.push(self.assignment()?);
        }
        let filter = self.filter()?;
        Ok(Statement::Update {
            table,
            assignments,
            filter,
        })
    }

    fn assignment(&mut self) -> Result<Assignment, DbError> {
        let column = self.name()?;
        self.expect_compare(CompareOp::Eq)?;
        Ok(Assignment {
            column,
            value: self.literal()?,
        })
    }

    fn delete(&mut self) -> Result<Statement, DbError> {
        self.expect_keyword(Keyword::From)?;
        let table = self.name()?;
        let filter = self.filter()?;
        Ok(Statement::Delete { table, filter })
    }

    fn begin(&mut self) -> Result<Statement, DbError> {
        if !self.eat_keyword(Keyword::Read) {
            return Ok(Statement::Begin { read_only: false });
        }
        self.expect_keyword(Keyword::Only)?;
        Ok(Statement::Begin { read_only: true })
    }

    fn filter(&mut self) -> Result<Option<Expr>, DbError> {
        match self.eat_keyword(Keyword::Where) {
            true => Ok(Some(self.or_expr()?)),
            false => Ok(None),
        }
    }

    fn or_expr(&mut self) -> Result<Expr, DbError> {
        let mut left = self.and_expr()?;
        while self.eat_keyword(Keyword::Or) {
            left = Expr::Or(Box::new(left), Box::new(self.and_expr()?));
        }
        Ok(left)
    }

    fn and_expr(&mut self) -> Result<Expr, DbError> {
        let mut left = self.not_expr()?;
        while self.eat_keyword(Keyword::And) {
            left = Expr::And(Box::new(left), Box::new(self.not_expr()?));
        }
        Ok(left)
    }

    fn not_expr(&mut self) -> Result<Expr, DbError> {
        match self.eat_keyword(Keyword::Not) {
            true => Ok(Expr::Not(Box::new(self.not_expr()?))),
            false => self.predicate(),
        }
    }

    /// An operand, then a comparison or an `IS NULL` if one follows.
    fn predicate(&mut self) -> Result<Expr, DbError> {
        let operand = self.operand()?;
        if let Some(op) = self.peek_compare() {
            self.bump();
            return Ok(Expr::Compare {
                left: Box::new(operand),
                op,
                right: Box::new(self.operand()?),
            });
        }
        if !self.eat_keyword(Keyword::Is) {
            return Ok(operand);
        }
        let negated = self.eat_keyword(Keyword::Not);
        self.expect_keyword(Keyword::Null)?;
        Ok(Expr::IsNull {
            operand: Box::new(operand),
            negated,
        })
    }

    fn operand(&mut self) -> Result<Expr, DbError> {
        if self.eat_punct(Punct::LParen) {
            let inner = self.or_expr()?;
            self.expect_punct(Punct::RParen)?;
            return Ok(inner);
        }
        match self.peek() {
            TokenKind::Identifier(_) => Ok(Expr::Column(self.name()?)),
            _ => Ok(Expr::Literal(self.literal()?)),
        }
    }

    fn literal(&mut self) -> Result<Value, DbError> {
        let position = self.position();
        let value = match self.peek().clone() {
            TokenKind::Number(text) => number_value(&text, position)?,
            TokenKind::Text(text) => Value::Text(text),
            TokenKind::Keyword(Keyword::True) => Value::Boolean(true),
            TokenKind::Keyword(Keyword::False) => Value::Boolean(false),
            TokenKind::Keyword(Keyword::Null) => Value::Null,
            _ => return Err(self.unexpected("a value")),
        };
        self.bump();
        Ok(value)
    }

    fn name_list(&mut self) -> Result<Vec<String>, DbError> {
        let mut names = vec![self.name()?];
        while self.eat_punct(Punct::Comma) {
            names.push(self.name()?);
        }
        Ok(names)
    }

    fn name(&mut self) -> Result<String, DbError> {
        let TokenKind::Identifier(name) = self.peek().clone() else {
            return Err(self.unexpected("a name"));
        };
        self.bump();
        Ok(name)
    }

    /// The text of a number token, for a place that holds no expression.
    fn number(&mut self) -> Result<String, DbError> {
        let TokenKind::Number(text) = self.peek().clone() else {
            return Err(self.unexpected("a number"));
        };
        self.bump();
        Ok(text)
    }

    fn row_count(&mut self) -> Result<u64, DbError> {
        let position = self.position();
        let text = self.number()?;
        text.parse()
            .map_err(|_| syntax_error(format!("{text} is not a row count"), position))
    }

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn position(&self) -> u32 {
        self.tokens[self.pos].position
    }

    /// Moves to the next token, and stays on `End` once it reaches it.
    fn bump(&mut self) {
        self.pos = (self.pos + 1).min(self.tokens.len() - 1);
    }

    fn peek_keyword(&self) -> Option<Keyword> {
        match self.peek() {
            TokenKind::Keyword(keyword) => Some(*keyword),
            _ => None,
        }
    }

    fn peek_compare(&self) -> Option<CompareOp> {
        match self.peek() {
            TokenKind::Compare(op) => Some(*op),
            _ => None,
        }
    }

    fn eat_keyword(&mut self, want: Keyword) -> bool {
        let found = self.peek_keyword() == Some(want);
        if found {
            self.bump();
        }
        found
    }

    fn expect_keyword(&mut self, want: Keyword) -> Result<(), DbError> {
        match self.eat_keyword(want) {
            true => Ok(()),
            false => Err(self.unexpected(want.as_str())),
        }
    }

    fn eat_punct(&mut self, want: Punct) -> bool {
        let found = self.peek() == &TokenKind::Punct(want);
        if found {
            self.bump();
        }
        found
    }

    fn expect_punct(&mut self, want: Punct) -> Result<(), DbError> {
        match self.eat_punct(want) {
            true => Ok(()),
            false => Err(self.unexpected(want.as_str())),
        }
    }

    fn expect_compare(&mut self, want: CompareOp) -> Result<(), DbError> {
        if self.peek_compare() != Some(want) {
            return Err(self.unexpected(want.as_str()));
        }
        self.bump();
        Ok(())
    }

    fn unexpected(&self, expected: &str) -> DbError {
        syntax_error(
            format!("expected {expected}, found {}", self.peek()),
            self.position(),
        )
    }
}

/// A number literal. The text decides the type, because a point means the
/// value is a decimal and every digit of it has to survive.
fn number_value(text: &str, position: u32) -> Result<Value, DbError> {
    if text.contains('.') {
        return text
            .parse::<Decimal>()
            .map(Value::Decimal)
            .map_err(|reason| syntax_error(reason, position));
    }
    text.parse()
        .map(Value::Integer)
        .map_err(|_| syntax_error(format!("{text} does not fit an integer"), position))
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ErrorCode;

    fn parsed(sql: &str) -> Statement {
        parse(sql).unwrap_or_else(|e| panic!("{sql:?} must parse, got {e}"))
    }

    fn fail(sql: &str) -> DbError {
        parse(sql).unwrap_err()
    }

    fn name(text: &str) -> String {
        text.to_string()
    }

    fn col(text: &str) -> Expr {
        Expr::Column(name(text))
    }

    fn int(n: i64) -> Expr {
        Expr::Literal(Value::Integer(n))
    }

    fn cmp(left: Expr, op: CompareOp, right: Expr) -> Expr {
        Expr::Compare {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }

    /// The condition of a `WHERE`, which keeps an expression test short.
    fn filter_of(condition: &str) -> Expr {
        let Statement::Select {
            filter: Some(filter),
            ..
        } = parsed(&format!("SELECT * FROM t WHERE {condition}"))
        else {
            panic!("expected a select that holds a filter");
        };
        filter
    }

    #[test]
    fn a_database_is_created_and_dropped() {
        assert_eq!(
            parsed("CREATE DATABASE shop"),
            Statement::CreateDatabase { name: name("shop") }
        );
        assert_eq!(
            parsed("DROP DATABASE shop"),
            Statement::DropDatabase { name: name("shop") }
        );
    }

    #[test]
    fn a_table_is_created_with_its_columns_and_constraints() {
        assert_eq!(
            parsed(
                "CREATE TABLE item (id INTEGER PRIMARY KEY, label TEXT NOT NULL, \
                 sold BOOLEAN, price DECIMAL(10, 2))"
            ),
            Statement::CreateTable {
                name: name("item"),
                columns: vec![
                    ColumnSpec {
                        name: name("id"),
                        ty: DataType::Integer,
                        primary_key: true,
                        not_null: false,
                    },
                    ColumnSpec {
                        name: name("label"),
                        ty: DataType::Text,
                        primary_key: false,
                        not_null: true,
                    },
                    ColumnSpec {
                        name: name("sold"),
                        ty: DataType::Boolean,
                        primary_key: false,
                        not_null: false,
                    },
                    ColumnSpec {
                        name: name("price"),
                        ty: DataType::Decimal { p: 10, s: 2 },
                        primary_key: false,
                        not_null: false,
                    },
                ],
            }
        );
    }

    #[test]
    fn the_constraints_of_a_column_read_in_either_order() {
        let both = "CREATE TABLE t (a INTEGER PRIMARY KEY NOT NULL)";
        let swapped = "CREATE TABLE t (a INTEGER NOT NULL PRIMARY KEY)";
        assert_eq!(parsed(both), parsed(swapped));
        let Statement::CreateTable { columns, .. } = parsed(both) else {
            panic!("expected a create table");
        };
        assert!(columns[0].primary_key && columns[0].not_null);
    }

    #[test]
    fn a_table_is_dropped() {
        assert_eq!(
            parsed("DROP TABLE item"),
            Statement::DropTable { name: name("item") }
        );
    }

    #[test]
    fn an_insert_carries_more_than_one_row() {
        assert_eq!(
            parsed("INSERT INTO t (a, b) VALUES (1, 'x'), (2, NULL)"),
            Statement::Insert {
                table: name("t"),
                columns: vec![name("a"), name("b")],
                rows: vec![
                    vec![Value::Integer(1), Value::Text(name("x"))],
                    vec![Value::Integer(2), Value::Null],
                ],
            }
        );
    }

    #[test]
    fn a_select_takes_every_column_or_the_ones_it_names() {
        let Statement::Select { selection, .. } = parsed("SELECT * FROM t") else {
            panic!("expected a select");
        };
        assert_eq!(selection, Selection::All);
        let Statement::Select { selection, .. } = parsed("SELECT a, b FROM t") else {
            panic!("expected a select");
        };
        assert_eq!(selection, Selection::Columns(vec![name("a"), name("b")]));
    }

    #[test]
    fn a_select_carries_every_clause() {
        assert_eq!(
            parsed("SELECT a FROM t WHERE a > 1 ORDER BY b DESC LIMIT 10"),
            Statement::Select {
                table: name("t"),
                selection: Selection::Columns(vec![name("a")]),
                filter: Some(cmp(col("a"), CompareOp::Gt, int(1))),
                order_by: Some(OrderBy {
                    column: name("b"),
                    descending: true,
                }),
                limit: Some(10),
            }
        );
    }

    #[test]
    fn a_sort_is_ascending_unless_it_says_otherwise() {
        for sql in [
            "SELECT * FROM t ORDER BY a",
            "SELECT * FROM t ORDER BY a ASC",
        ] {
            let Statement::Select { order_by, .. } = parsed(sql) else {
                panic!("expected a select");
            };
            assert_eq!(
                order_by,
                Some(OrderBy {
                    column: name("a"),
                    descending: false,
                })
            );
        }
    }

    #[test]
    fn a_select_with_no_clause_holds_none_of_them() {
        assert_eq!(
            parsed("SELECT * FROM t"),
            Statement::Select {
                table: name("t"),
                selection: Selection::All,
                filter: None,
                order_by: None,
                limit: None,
            }
        );
    }

    #[test]
    fn an_update_sets_more_than_one_column() {
        assert_eq!(
            parsed("UPDATE t SET a = 1, b = 'x' WHERE c = true"),
            Statement::Update {
                table: name("t"),
                assignments: vec![
                    Assignment {
                        column: name("a"),
                        value: Value::Integer(1),
                    },
                    Assignment {
                        column: name("b"),
                        value: Value::Text(name("x")),
                    },
                ],
                filter: Some(cmp(
                    col("c"),
                    CompareOp::Eq,
                    Expr::Literal(Value::Boolean(true))
                )),
            }
        );
    }

    #[test]
    fn a_delete_takes_an_optional_condition() {
        assert_eq!(
            parsed("DELETE FROM t"),
            Statement::Delete {
                table: name("t"),
                filter: None,
            }
        );
        assert_eq!(
            parsed("DELETE FROM t WHERE a = 1"),
            Statement::Delete {
                table: name("t"),
                filter: Some(cmp(col("a"), CompareOp::Eq, int(1))),
            }
        );
    }

    #[test]
    fn a_transaction_opens_read_write_or_read_only_and_ends() {
        assert_eq!(parsed("BEGIN"), Statement::Begin { read_only: false });
        assert_eq!(
            parsed("BEGIN READ ONLY"),
            Statement::Begin { read_only: true }
        );
        assert_eq!(parsed("COMMIT"), Statement::Commit);
        assert_eq!(parsed("ROLLBACK"), Statement::Rollback);
    }

    #[test]
    fn a_keyword_reads_in_any_case() {
        assert_eq!(parsed("select * from t"), parsed("SELECT * FROM t"));
        assert_eq!(parsed("SeLeCt * FrOm t"), parsed("SELECT * FROM t"));
        // An identifier keeps its case, so these are two different tables.
        assert_ne!(parsed("SELECT * FROM t"), parsed("SELECT * FROM T"));
    }

    #[test]
    fn a_trailing_semicolon_is_allowed_and_optional() {
        assert_eq!(parsed("COMMIT;"), parsed("COMMIT"));
        assert_eq!(parsed("SELECT * FROM t ;"), parsed("SELECT * FROM t"));
    }

    #[test]
    fn every_comparison_parses() {
        let cases = [
            ("=", CompareOp::Eq),
            ("!=", CompareOp::Ne),
            ("<", CompareOp::Lt),
            ("<=", CompareOp::Le),
            (">", CompareOp::Gt),
            (">=", CompareOp::Ge),
        ];
        for (text, op) in cases {
            assert_eq!(filter_of(&format!("a {text} 1")), cmp(col("a"), op, int(1)));
        }
    }

    #[test]
    fn and_binds_tighter_than_or() {
        assert_eq!(
            filter_of("a = 1 OR b = 2 AND c = 3"),
            Expr::Or(
                Box::new(cmp(col("a"), CompareOp::Eq, int(1))),
                Box::new(Expr::And(
                    Box::new(cmp(col("b"), CompareOp::Eq, int(2))),
                    Box::new(cmp(col("c"), CompareOp::Eq, int(3))),
                )),
            )
        );
    }

    #[test]
    fn parentheses_override_the_precedence() {
        assert_eq!(
            filter_of("(a = 1 OR b = 2) AND c = 3"),
            Expr::And(
                Box::new(Expr::Or(
                    Box::new(cmp(col("a"), CompareOp::Eq, int(1))),
                    Box::new(cmp(col("b"), CompareOp::Eq, int(2))),
                )),
                Box::new(cmp(col("c"), CompareOp::Eq, int(3))),
            )
        );
    }

    #[test]
    fn a_chain_of_one_operator_reads_left_to_right() {
        assert_eq!(
            filter_of("a = 1 AND b = 2 AND c = 3"),
            Expr::And(
                Box::new(Expr::And(
                    Box::new(cmp(col("a"), CompareOp::Eq, int(1))),
                    Box::new(cmp(col("b"), CompareOp::Eq, int(2))),
                )),
                Box::new(cmp(col("c"), CompareOp::Eq, int(3))),
            )
        );
        assert_eq!(
            filter_of("a = 1 OR b = 2 OR c = 3"),
            Expr::Or(
                Box::new(Expr::Or(
                    Box::new(cmp(col("a"), CompareOp::Eq, int(1))),
                    Box::new(cmp(col("b"), CompareOp::Eq, int(2))),
                )),
                Box::new(cmp(col("c"), CompareOp::Eq, int(3))),
            )
        );
    }

    #[test]
    fn not_covers_the_comparison_that_follows_it() {
        assert_eq!(
            filter_of("NOT a = 1"),
            Expr::Not(Box::new(cmp(col("a"), CompareOp::Eq, int(1))))
        );
        assert_eq!(
            filter_of("NOT NOT a = 1"),
            Expr::Not(Box::new(Expr::Not(Box::new(cmp(
                col("a"),
                CompareOp::Eq,
                int(1)
            )))))
        );
    }

    #[test]
    fn a_null_test_reads_both_ways() {
        assert_eq!(
            filter_of("a IS NULL"),
            Expr::IsNull {
                operand: Box::new(col("a")),
                negated: false,
            }
        );
        assert_eq!(
            filter_of("a IS NOT NULL"),
            Expr::IsNull {
                operand: Box::new(col("a")),
                negated: true,
            }
        );
    }

    #[test]
    fn a_condition_may_be_a_column_or_a_literal_on_its_own() {
        assert_eq!(filter_of("a"), col("a"));
        assert_eq!(filter_of("true"), Expr::Literal(Value::Boolean(true)));
    }

    #[test]
    fn every_literal_form_parses() {
        assert_eq!(filter_of("a = 7"), cmp(col("a"), CompareOp::Eq, int(7)));
        assert_eq!(filter_of("a = -7"), cmp(col("a"), CompareOp::Eq, int(-7)));
        assert_eq!(
            filter_of("a = 'it''s'"),
            cmp(
                col("a"),
                CompareOp::Eq,
                Expr::Literal(Value::Text(name("it's")))
            )
        );
        assert_eq!(
            filter_of("a = false"),
            cmp(
                col("a"),
                CompareOp::Eq,
                Expr::Literal(Value::Boolean(false))
            )
        );
        assert_eq!(
            filter_of("a = NULL"),
            cmp(col("a"), CompareOp::Eq, Expr::Literal(Value::Null))
        );
    }

    #[test]
    fn a_decimal_literal_keeps_every_digit() {
        assert_eq!(
            filter_of("a = 12.20"),
            cmp(
                col("a"),
                CompareOp::Eq,
                Expr::Literal(Value::Decimal(Decimal {
                    units: 1220,
                    scale: 2,
                }))
            )
        );
    }

    #[test]
    fn a_number_too_large_for_an_integer_is_an_error() {
        let e = fail("INSERT INTO t (a) VALUES (99999999999999999999)");
        assert_eq!(e.code, ErrorCode::SyntaxError);
        assert_eq!(e.position, Some(27));
        assert!(e.message.contains("does not fit an integer"), "{e}");
    }

    #[test]
    fn a_bad_precision_reports_the_type_that_holds_it() {
        for sql in [
            "CREATE TABLE t (a DECIMAL(39, 2))",
            "CREATE TABLE t (a DECIMAL(2, 3))",
            "CREATE TABLE t (a DECIMAL(0, 0))",
        ] {
            let e = fail(sql);
            assert_eq!(e.code, ErrorCode::SyntaxError);
            assert_eq!(e.position, Some(19), "for {sql}");
        }
    }

    #[test]
    fn a_type_the_system_has_no_column_for_is_an_error() {
        let e = fail("CREATE TABLE t (a REAL)");
        assert_eq!(e.position, Some(19));
        assert!(e.message.contains("unknown data type"), "{e}");
    }

    #[test]
    fn a_limit_that_is_not_a_count_is_an_error() {
        let e = fail("SELECT * FROM t LIMIT 1.5");
        assert_eq!(e.position, Some(23));
        assert!(e.message.contains("is not a row count"), "{e}");
    }

    #[test]
    fn text_after_the_statement_is_an_error() {
        let e = fail("SELECT * FROM t; SELECT * FROM u");
        assert_eq!(e.position, Some(18));
        assert!(e.message.starts_with("expected the end"), "{e}");
    }

    #[test]
    fn an_empty_statement_is_an_error() {
        for sql in ["", "   ", ";"] {
            let e = fail(sql);
            assert_eq!(e.code, ErrorCode::SyntaxError);
            assert!(e.message.contains("a statement"), "{sql:?}: {e}");
        }
    }

    #[test]
    fn a_missing_part_names_what_it_expected_and_where() {
        let cases = [
            ("SELECT FROM t", 8, "a name"),
            ("SELECT a t", 10, "FROM"),
            ("SELECT a FROM", 14, "a name"),
            ("CREATE shop", 8, "TABLE"),
            ("CREATE TABLE t a INTEGER)", 16, "("),
            ("CREATE TABLE t (a INTEGER", 26, ")"),
            ("INSERT t (a) VALUES (1)", 8, "INTO"),
            ("INSERT INTO t (a) (1)", 19, "VALUES"),
            ("UPDATE t a = 1", 10, "SET"),
            ("UPDATE t SET a 1", 16, "="),
            ("DELETE t", 8, "FROM"),
            ("SELECT * FROM t WHERE a IS 1", 28, "NULL"),
            ("SELECT * FROM t ORDER a", 23, "BY"),
            ("SELECT * FROM t LIMIT", 22, "a number"),
            ("BEGIN READ", 11, "ONLY"),
            ("SELECT * FROM t WHERE (a = 1", 29, ")"),
            ("SELECT * FROM t WHERE a =", 26, "a value"),
            ("DROP shop", 6, "TABLE"),
        ];
        for (sql, position, expected) in cases {
            let e = fail(sql);
            assert_eq!(e.code, ErrorCode::SyntaxError, "for {sql}");
            assert_eq!(e.position, Some(position), "for {sql}: {e}");
            assert!(
                e.message.contains(expected),
                "for {sql}: wanted {expected}, got {e}"
            );
        }
    }

    #[test]
    fn a_feature_that_is_out_of_scope_is_refused() {
        for sql in [
            "SELECT a FROM t JOIN u ON t.a = u.a",
            "SELECT COUNT(a) FROM t",
            "SELECT DISTINCT a FROM t",
            "SELECT a FROM t GROUP BY a",
            "SELECT a FROM (SELECT a FROM u)",
            "ALTER TABLE t ADD COLUMN b INTEGER",
            "CREATE INDEX i ON t (a)",
            "DROP TABLE IF EXISTS t",
            "SELECT a + 1 FROM t",
            "SELECT a FROM t WHERE a = ?",
        ] {
            let e = fail(sql);
            assert_eq!(e.code, ErrorCode::SyntaxError, "{sql} must be refused");
        }
    }
}
