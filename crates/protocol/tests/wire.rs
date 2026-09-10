//! The wire contract, seen from outside the crate.
//!
//! These tests link `protocol` as a consumer does, so they use only the names
//! that `lib.rs` re-exports. This proves that the CLI needs no private path.

use std::cmp::Ordering;

use protocol::{
    ClientMsg, ColumnDesc, DataType, Decimal, ErrorCode, PROTOCOL_VERSION, ServerMsg, TxState,
    Value,
};

fn dec(s: &str) -> Decimal {
    s.parse().unwrap()
}

/// A 38-digit value. The boundary of `[FR18]`.
fn max_digits() -> String {
    "1".repeat(38)
}

fn client_msgs() -> Vec<ClientMsg> {
    vec![
        ClientMsg::Startup {
            version: PROTOCOL_VERSION,
            database: "shop".to_string(),
        },
        ClientMsg::Query {
            sql: "SELECT * FROM item".to_string(),
        },
        ClientMsg::Cancel {
            conn_id: 7,
            secret: "s3cret".to_string(),
        },
        ClientMsg::Close,
    ]
}

fn server_msgs() -> Vec<ServerMsg> {
    vec![
        ServerMsg::Ready {
            conn_id: 7,
            secret: "s3cret".to_string(),
            tx: TxState::Open,
        },
        ServerMsg::RowDesc {
            cols: vec![
                ColumnDesc {
                    name: "id".to_string(),
                    ty: DataType::Integer,
                },
                ColumnDesc {
                    name: "name".to_string(),
                    ty: DataType::Text,
                },
                ColumnDesc {
                    name: "sold".to_string(),
                    ty: DataType::Boolean,
                },
                ColumnDesc {
                    name: "price".to_string(),
                    ty: DataType::Decimal { p: 10, s: 2 },
                },
            ],
        },
        // One of every `Value` variant, and a decimal with a trailing zero.
        ServerMsg::DataRow {
            values: vec![
                Value::Integer(-1),
                Value::Text("hé\t\"x\"".to_string()),
                Value::Boolean(true),
                Value::Decimal(dec("12.20")),
                Value::Null,
            ],
        },
        ServerMsg::Complete {
            kind: "SELECT".to_string(),
            rows: 2,
        },
        ServerMsg::Error {
            code: ErrorCode::SyntaxError,
            message: "unexpected token".to_string(),
            position: Some(12),
        },
    ]
}

#[test]
fn every_client_message_reads_back_from_its_line() {
    for msg in client_msgs() {
        let line = msg.to_line();
        assert_eq!(ClientMsg::from_line(&line).unwrap(), msg, "{line}");
    }
}

#[test]
fn every_server_message_reads_back_from_its_line() {
    for msg in server_msgs() {
        let line = msg.to_line();
        assert_eq!(ServerMsg::from_line(&line).unwrap(), msg, "{line}");
    }
}

#[test]
fn a_statement_exchange_reads_back_in_order() {
    let query = ClientMsg::Query {
        sql: "SELECT price FROM item".to_string(),
    };
    let replies = vec![
        ServerMsg::RowDesc {
            cols: vec![ColumnDesc {
                name: "price".to_string(),
                ty: DataType::Decimal { p: 10, s: 2 },
            }],
        },
        ServerMsg::DataRow {
            values: vec![Value::Decimal(dec("12.20"))],
        },
        ServerMsg::DataRow {
            values: vec![Value::Null],
        },
        ServerMsg::Complete {
            kind: "SELECT".to_string(),
            rows: 2,
        },
        ServerMsg::Ready {
            conn_id: 7,
            secret: "s3cret".to_string(),
            tx: TxState::None,
        },
    ];

    let mut stream = query.to_line();
    for msg in &replies {
        stream.push('\n');
        stream.push_str(&msg.to_line());
    }

    let mut lines = stream.lines();
    assert_eq!(ClientMsg::from_line(lines.next().unwrap()).unwrap(), query);
    let read: Vec<ServerMsg> = lines.map(|l| ServerMsg::from_line(l).unwrap()).collect();
    assert_eq!(read, replies);
}

#[test]
fn a_decimal_keeps_its_digits_through_json() {
    // The digits, not only the numeric value. `[FR19]`
    for s in ["1002.2", "12.20", "-0.50", &max_digits()] {
        let d = dec(s);
        let line = ServerMsg::DataRow {
            values: vec![Value::Decimal(d)],
        }
        .to_line();
        // The wire form is a JSON string that holds the same digits.
        assert!(line.contains(&format!(r#""Decimal":"{s}""#)), "{line}");
        // `PartialEq` compares the stored digits, so `12.20` never becomes `12.2`.
        match ServerMsg::from_line(&line).unwrap() {
            ServerMsg::DataRow { values } => assert_eq!(values, vec![Value::Decimal(d)], "{line}"),
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn more_than_38_digits_fails_to_read() {
    // 39 digits is over the boundary of `[FR18]`.
    assert!("1".repeat(39).parse::<Decimal>().is_err());
    assert!(max_digits().parse::<Decimal>().is_ok());
}

#[test]
fn a_comparison_with_null_gives_none() {
    // `[FR36]`
    for other in [
        Value::Integer(1),
        Value::Text("a".to_string()),
        Value::Boolean(false),
        Value::Decimal(dec("1.0")),
        Value::Null,
    ] {
        assert_eq!(Value::Null.compare(&other), None, "{other:?}");
        assert_eq!(other.compare(&Value::Null), None, "{other:?}");
    }
}

#[test]
fn a_decimal_compares_by_its_value_not_its_digits() {
    // `[FR39]`
    let cases = [
        ("12.20", "12.2", Ordering::Equal),
        ("2.10", "10.2", Ordering::Less),
        ("10.2", "2.10", Ordering::Greater),
    ];
    for (a, b, want) in cases {
        assert_eq!(dec(a).compare(&dec(b)), want, "{a} against {b}");
        assert_eq!(
            Value::Decimal(dec(a)).compare(&Value::Decimal(dec(b))),
            Some(want),
            "{a} against {b}"
        );
    }
    // The stored digits still differ, so `==` says no.
    assert_ne!(dec("12.20"), dec("12.2"));
}

#[test]
fn an_unknown_type_gives_an_error() {
    assert!(ClientMsg::from_line(r#"{"type":"hello"}"#).is_err());
    assert!(ServerMsg::from_line(r#"{"type":"hello"}"#).is_err());
}

#[test]
fn a_missing_field_gives_an_error() {
    assert!(ClientMsg::from_line(r#"{"type":"startup","version":1}"#).is_err());
    assert!(ServerMsg::from_line(r#"{"type":"datarow"}"#).is_err());
}

#[test]
fn a_json_number_for_a_decimal_gives_an_error() {
    // A JSON number is a float and would lose digits. `[FR19]`
    let line = r#"{"type":"datarow","values":[{"Decimal":1002.2}]}"#;
    assert!(ServerMsg::from_line(line).is_err());
}
