//! The lexer. Text in, tokens out, or an error at the character that broke.

use protocol::DbError;

use crate::sql::ast::CompareOp;
use crate::sql::syntax_error;
use crate::sql::token::{Keyword, Punct, Token, TokenKind};

/// Reads the tokens of one statement.
pub struct Lexer {
    /// The statement as characters. A position then counts characters, so a
    /// multi-byte character does not shift the count that an error reports.
    input: Vec<char>,
    pos: usize,
}

impl Lexer {
    pub fn new(input: &str) -> Lexer {
        Lexer {
            input: input.chars().collect(),
            pos: 0,
        }
    }

    /// The next token, or `End` once the input runs out.
    pub fn next_token(&mut self) -> Result<Token, DbError> {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pos += 1;
        }
        let position = self.position();
        let Some(c) = self.peek() else {
            return Ok(Token {
                kind: TokenKind::End,
                position,
            });
        };
        let kind = match c {
            '(' => self.single(TokenKind::Punct(Punct::LParen)),
            ')' => self.single(TokenKind::Punct(Punct::RParen)),
            ',' => self.single(TokenKind::Punct(Punct::Comma)),
            ';' => self.single(TokenKind::Punct(Punct::Semicolon)),
            '*' => self.single(TokenKind::Punct(Punct::Star)),
            '=' => self.single(TokenKind::Compare(CompareOp::Eq)),
            '<' => {
                self.pos += 1;
                self.compare_tail('=', CompareOp::Le, CompareOp::Lt)
            }
            '>' => {
                self.pos += 1;
                self.compare_tail('=', CompareOp::Ge, CompareOp::Gt)
            }
            '!' => {
                self.pos += 1;
                if !self.eat('=') {
                    return Err(syntax_error("an = must follow a !", position));
                }
                TokenKind::Compare(CompareOp::Ne)
            }
            '\'' => self.text(position)?,
            // A `-` can only be a sign, because arithmetic is out of scope.
            // So `a - 1` lexes as `a` and `-1`, and the parser refuses it.
            c if c.is_ascii_digit() || c == '.' || (c == '-' && self.starts_number(1)) => {
                self.number(position)?
            }
            c if c.is_alphabetic() || c == '_' => self.word(),
            c => {
                return Err(syntax_error(
                    format!("unexpected character {c:?}"),
                    position,
                ));
            }
        };
        Ok(Token { kind, position })
    }

    fn peek(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }

    /// The 1-based position of the character under the cursor.
    fn position(&self) -> u32 {
        self.pos as u32 + 1
    }

    fn single(&mut self, kind: TokenKind) -> TokenKind {
        self.pos += 1;
        kind
    }

    /// Takes `c` if it is next, and says whether it did.
    fn eat(&mut self, c: char) -> bool {
        let found = self.peek() == Some(c);
        if found {
            self.pos += 1;
        }
        found
    }

    /// The two-character comparison when `tail` follows, the one-character
    /// one when it does not.
    fn compare_tail(&mut self, tail: char, with: CompareOp, without: CompareOp) -> TokenKind {
        match self.eat(tail) {
            true => TokenKind::Compare(with),
            false => TokenKind::Compare(without),
        }
    }

    /// A single-quoted string. `''` inside it means one quote.
    fn text(&mut self, position: u32) -> Result<TokenKind, DbError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(syntax_error("a string has no closing quote", position)),
                Some('\'') => {
                    self.pos += 1;
                    if !self.eat('\'') {
                        return Ok(TokenKind::Text(out));
                    }
                    out.push('\'');
                }
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// Whether a digit or a point sits `ahead` characters on.
    fn starts_number(&self, ahead: usize) -> bool {
        self.input
            .get(self.pos + ahead)
            .is_some_and(|c| c.is_ascii_digit() || *c == '.')
    }

    /// A number, kept as text. The parser decides whether it is an integer or
    /// a decimal, because only it knows what the column holds.
    fn number(&mut self, position: u32) -> Result<TokenKind, DbError> {
        let start = self.pos;
        self.eat('-');
        self.digits();
        if self.eat('.') && !self.digits() {
            return Err(syntax_error("a digit must follow the point", position));
        }
        Ok(TokenKind::Number(
            self.input[start..self.pos].iter().collect(),
        ))
    }

    /// Consumes digits, and says whether it found one.
    fn digits(&mut self) -> bool {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        self.pos > start
    }

    /// A reserved word, or an identifier when no row of the table holds it.
    fn word(&mut self) -> TokenKind {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        let word: String = self.input[start..self.pos].iter().collect();
        match Keyword::parse(&word) {
            Some(keyword) => TokenKind::Keyword(keyword),
            None => TokenKind::Identifier(word),
        }
    }
}

/// Every token of a statement, ending with `End`.
pub fn tokens(sql: &str) -> Result<Vec<Token>, DbError> {
    let mut lexer = Lexer::new(sql);
    let mut tokens = Vec::new();
    loop {
        let token = lexer.next_token()?;
        let end = token.kind == TokenKind::End;
        tokens.push(token);
        if end {
            return Ok(tokens);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::ErrorCode;

    fn kinds(sql: &str) -> Vec<TokenKind> {
        tokens(sql).unwrap().into_iter().map(|t| t.kind).collect()
    }

    fn fail(sql: &str) -> DbError {
        tokens(sql).unwrap_err()
    }

    fn identifier(name: &str) -> TokenKind {
        TokenKind::Identifier(name.to_string())
    }

    #[test]
    fn a_statement_reads_as_its_tokens_in_order() {
        assert_eq!(
            kinds("SELECT a FROM t WHERE b != 'x';"),
            vec![
                TokenKind::Keyword(Keyword::Select),
                identifier("a"),
                TokenKind::Keyword(Keyword::From),
                identifier("t"),
                TokenKind::Keyword(Keyword::Where),
                identifier("b"),
                TokenKind::Compare(CompareOp::Ne),
                TokenKind::Text("x".to_string()),
                TokenKind::Punct(Punct::Semicolon),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn every_comparison_and_mark_reads() {
        assert_eq!(
            kinds("= != < <= > >= ( ) , ; *"),
            vec![
                TokenKind::Compare(CompareOp::Eq),
                TokenKind::Compare(CompareOp::Ne),
                TokenKind::Compare(CompareOp::Lt),
                TokenKind::Compare(CompareOp::Le),
                TokenKind::Compare(CompareOp::Gt),
                TokenKind::Compare(CompareOp::Ge),
                TokenKind::Punct(Punct::LParen),
                TokenKind::Punct(Punct::RParen),
                TokenKind::Punct(Punct::Comma),
                TokenKind::Punct(Punct::Semicolon),
                TokenKind::Punct(Punct::Star),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn a_comparison_without_its_second_character_still_reads() {
        assert_eq!(
            kinds("a<1"),
            vec![
                identifier("a"),
                TokenKind::Compare(CompareOp::Lt),
                TokenKind::Number("1".to_string()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn a_number_reads_with_and_without_a_point() {
        assert_eq!(
            kinds("1 12.50 .5 -7 -1.5 -.5"),
            vec![
                TokenKind::Number("1".to_string()),
                TokenKind::Number("12.50".to_string()),
                TokenKind::Number(".5".to_string()),
                TokenKind::Number("-7".to_string()),
                TokenKind::Number("-1.5".to_string()),
                TokenKind::Number("-.5".to_string()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn an_identifier_holds_an_underscore_and_a_digit() {
        assert_eq!(
            kinds("order_total x1 _a"),
            vec![
                identifier("order_total"),
                identifier("x1"),
                identifier("_a"),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn a_string_reads_a_doubled_quote_as_one_quote() {
        assert_eq!(
            kinds("'it''s' '' 'a b'"),
            vec![
                TokenKind::Text("it's".to_string()),
                TokenKind::Text(String::new()),
                TokenKind::Text("a b".to_string()),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn whitespace_of_every_kind_separates_tokens() {
        assert_eq!(
            kinds("SELECT\t*\nFROM\r\n  t"),
            vec![
                TokenKind::Keyword(Keyword::Select),
                TokenKind::Punct(Punct::Star),
                TokenKind::Keyword(Keyword::From),
                identifier("t"),
                TokenKind::End,
            ]
        );
    }

    #[test]
    fn a_position_counts_characters_and_not_bytes() {
        // Four of these characters are two bytes each in UTF-8, so a count of
        // bytes would put the `!` at 15 instead of 11.
        let read = tokens("'ünïcödé' a").unwrap();
        assert_eq!(read[0].kind, TokenKind::Text("ünïcödé".to_string()));
        assert_eq!(read[0].position, 1);
        assert_eq!(read[1].position, 11, "the token after the string");
        assert_eq!(fail("'ünïcödé' !").position, Some(11));
    }

    #[test]
    fn the_end_sits_after_the_last_character() {
        let read = tokens("a").unwrap();
        assert_eq!(read[1].kind, TokenKind::End);
        assert_eq!(read[1].position, 2);
        assert_eq!(tokens("").unwrap()[0].position, 1);
    }

    #[test]
    fn an_unterminated_string_names_its_opening_quote() {
        let e = fail("SELECT 'x FROM t");
        assert_eq!(e.code, ErrorCode::SyntaxError);
        assert_eq!(e.position, Some(8));
        assert_eq!(e.message, "a string has no closing quote");
    }

    #[test]
    fn a_bang_without_an_equals_is_an_error() {
        let e = fail("a ! 1");
        assert_eq!(e.position, Some(3));
        assert_eq!(e.message, "an = must follow a !");
    }

    #[test]
    fn a_point_without_a_digit_is_an_error() {
        for (sql, position) in [("12.", 1), (".", 1), ("a = .", 5)] {
            let e = fail(sql);
            assert_eq!(e.position, Some(position), "for {sql}");
            assert_eq!(e.message, "a digit must follow the point");
        }
    }

    #[test]
    fn a_character_the_grammar_has_no_use_for_is_an_error() {
        // A `-` that no number follows is one of them, because the grammar
        // has no subtraction.
        for (sql, position) in [("a @ b", 3), ("#", 1), ("a + b", 3), ("a - b", 3), ("-", 1)] {
            let e = fail(sql);
            assert_eq!(e.code, ErrorCode::SyntaxError);
            assert_eq!(e.position, Some(position), "for {sql}");
            assert!(e.message.starts_with("unexpected character"), "{e}");
        }
    }
}
