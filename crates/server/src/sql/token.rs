//! The tokens of a statement, and the words the grammar reserves.

use std::fmt;

use crate::sql::ast::CompareOp;

/// A word the grammar reserves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    And,
    Asc,
    Begin,
    By,
    Commit,
    Create,
    Database,
    Delete,
    Desc,
    Drop,
    False,
    From,
    Insert,
    Into,
    Is,
    Key,
    Limit,
    Not,
    Null,
    Only,
    Or,
    Order,
    Primary,
    Read,
    Rollback,
    Select,
    Set,
    Table,
    True,
    Update,
    Values,
    Where,
}

/// Every reserved word with the text that spells it, in the order of the
/// enum, so [`Keyword::as_str`] is an index. A test holds the order.
const KEYWORDS: &[(&str, Keyword)] = &[
    ("AND", Keyword::And),
    ("ASC", Keyword::Asc),
    ("BEGIN", Keyword::Begin),
    ("BY", Keyword::By),
    ("COMMIT", Keyword::Commit),
    ("CREATE", Keyword::Create),
    ("DATABASE", Keyword::Database),
    ("DELETE", Keyword::Delete),
    ("DESC", Keyword::Desc),
    ("DROP", Keyword::Drop),
    ("FALSE", Keyword::False),
    ("FROM", Keyword::From),
    ("INSERT", Keyword::Insert),
    ("INTO", Keyword::Into),
    ("IS", Keyword::Is),
    ("KEY", Keyword::Key),
    ("LIMIT", Keyword::Limit),
    ("NOT", Keyword::Not),
    ("NULL", Keyword::Null),
    ("ONLY", Keyword::Only),
    ("OR", Keyword::Or),
    ("ORDER", Keyword::Order),
    ("PRIMARY", Keyword::Primary),
    ("READ", Keyword::Read),
    ("ROLLBACK", Keyword::Rollback),
    ("SELECT", Keyword::Select),
    ("SET", Keyword::Set),
    ("TABLE", Keyword::Table),
    ("TRUE", Keyword::True),
    ("UPDATE", Keyword::Update),
    ("VALUES", Keyword::Values),
    ("WHERE", Keyword::Where),
];

impl Keyword {
    /// Reads a word. A keyword is case-insensitive, so `select` and `SELECT`
    /// are one keyword. A word that no row holds is an identifier, and
    /// returns none.
    pub fn parse(word: &str) -> Option<Keyword> {
        KEYWORDS
            .iter()
            .find(|(text, _)| text.eq_ignore_ascii_case(word))
            .map(|(_, keyword)| *keyword)
    }

    pub fn as_str(self) -> &'static str {
        KEYWORDS[self as usize].0
    }
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A mark that separates the parts of a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punct {
    LParen,
    RParen,
    Comma,
    Semicolon,
    Star,
}

impl Punct {
    pub fn as_str(self) -> &'static str {
        match self {
            Punct::LParen => "(",
            Punct::RParen => ")",
            Punct::Comma => ",",
            Punct::Semicolon => ";",
            Punct::Star => "*",
        }
    }
}

impl fmt::Display for Punct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one token holds.
///
/// A number keeps its text, because the parser decides whether it is an
/// `INTEGER` or a `DECIMAL`. A string arrives with its quotes gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Keyword(Keyword),
    Identifier(String),
    Number(String),
    Text(String),
    Compare(CompareOp),
    Punct(Punct),
    End,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Keyword(keyword) => write!(f, "{keyword}"),
            TokenKind::Identifier(name) => write!(f, "{name}"),
            TokenKind::Number(text) => write!(f, "{text}"),
            TokenKind::Text(_) => f.write_str("a string"),
            TokenKind::Compare(op) => write!(f, "{op}"),
            TokenKind::Punct(punct) => write!(f, "{punct}"),
            TokenKind::End => f.write_str("the end of the statement"),
        }
    }
}

/// One token and where it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// The 1-based character offset of the first character of the token.
    /// Characters, not bytes, so a multi-byte character counts once.
    pub position: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyword_table_follows_the_enum_order() {
        for (index, (text, keyword)) in KEYWORDS.iter().enumerate() {
            assert_eq!(*keyword as usize, index, "{text} sits in the wrong row");
            assert_eq!(keyword.as_str(), *text);
            assert_eq!(keyword.to_string(), *text);
        }
    }

    #[test]
    fn a_keyword_reads_in_any_case() {
        for (text, keyword) in KEYWORDS {
            assert_eq!(Keyword::parse(text), Some(*keyword));
            assert_eq!(Keyword::parse(&text.to_lowercase()), Some(*keyword));
        }
        assert_eq!(Keyword::parse("Select"), Some(Keyword::Select));
    }

    #[test]
    fn a_word_that_is_not_reserved_is_not_a_keyword() {
        for word in ["shop", "selected", "", "sel", "order_by"] {
            assert_eq!(Keyword::parse(word), None, "{word}");
        }
    }

    #[test]
    fn every_mark_shows_itself() {
        let cases = [
            (Punct::LParen, "("),
            (Punct::RParen, ")"),
            (Punct::Comma, ","),
            (Punct::Semicolon, ";"),
            (Punct::Star, "*"),
        ];
        for (punct, text) in cases {
            assert_eq!(punct.as_str(), text);
            assert_eq!(punct.to_string(), text);
        }
    }

    #[test]
    fn a_token_names_itself_in_an_error() {
        let cases = [
            (TokenKind::Keyword(Keyword::From), "FROM"),
            (TokenKind::Identifier("shop".to_string()), "shop"),
            (TokenKind::Number("12.5".to_string()), "12.5"),
            (TokenKind::Text("x".to_string()), "a string"),
            (TokenKind::Compare(CompareOp::Ne), "!="),
            (TokenKind::Punct(Punct::Comma), ","),
            (TokenKind::End, "the end of the statement"),
        ];
        for (kind, shown) in cases {
            assert_eq!(kind.to_string(), shown);
        }
    }
}
