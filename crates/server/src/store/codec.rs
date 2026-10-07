//! The bytes of one row.
//!
//! A null bitmap of one bit for each column comes first, then every value
//! that is not null, in the order the columns hold. A value too large for a
//! record leaves a pointer to a chain of pages instead.

use protocol::{DataType, DbError, Decimal, ErrorCode, Value};

use crate::catalog::ColumnDef;
use crate::store::storage_error;

/// A value longer than this moves out of the record and into a chain.
pub const INLINE_LIMIT: usize = 2048;

/// Set in the length of a `TEXT` value when a chain holds it, which is the
/// only type that can outgrow a record.
const CHAIN_FLAG: u32 = 1 << 31;

/// Where the chain that holds one value starts, and how long the value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainPtr {
    pub head: u32,
    pub len: u64,
}

/// How a record holds one value.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Value(Value),
    /// The value lives in a chain, and the record keeps this pointer.
    Chain(ChainPtr),
}

/// Whether a value fits in a record, or needs a chain of its own.
pub fn fits_inline(value: &Value) -> bool {
    match value {
        Value::Text(text) => text.len() <= INLINE_LIMIT,
        _ => true,
    }
}

/// The bytes of a row. A value that needs a chain must already have one, so
/// the caller writes the chain first and passes its pointer.
pub fn encode_row(columns: &[ColumnDef], cells: &[Cell]) -> Result<Vec<u8>, DbError> {
    if cells.len() != columns.len() {
        return Err(storage_error(format!(
            "the row holds {} values for {} columns",
            cells.len(),
            columns.len()
        )));
    }
    let mut out = vec![0; bitmap_len(columns.len())];
    for (index, (column, cell)) in columns.iter().zip(cells).enumerate() {
        match cell {
            Cell::Value(Value::Null) => out[index / 8] |= 1 << (index % 8),
            Cell::Value(value) => encode_value(column, value, &mut out)?,
            Cell::Chain(ptr) => encode_chain(column, *ptr, &mut out)?,
        }
    }
    Ok(out)
}

/// The values of a row, read against the columns of its table.
pub fn decode_row(columns: &[ColumnDef], bytes: &[u8]) -> Result<Vec<Cell>, DbError> {
    let bitmap = bitmap_len(columns.len());
    if bytes.len() < bitmap {
        return Err(short());
    }
    let mut reader = Reader { bytes, at: bitmap };
    let mut cells = Vec::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        let null = bytes[index / 8] & (1 << (index % 8)) != 0;
        cells.push(match null {
            true => Cell::Value(Value::Null),
            false => reader.cell(column)?,
        });
    }
    Ok(cells)
}

fn bitmap_len(columns: usize) -> usize {
    columns.div_ceil(8)
}

fn short() -> DbError {
    storage_error("the record ends before its columns do")
}

fn mismatch(column: &ColumnDef) -> DbError {
    DbError {
        code: ErrorCode::TypeMismatch,
        message: format!("the column {} holds {}", column.name, column.ty),
        position: None,
    }
}

fn encode_value(column: &ColumnDef, value: &Value, out: &mut Vec<u8>) -> Result<(), DbError> {
    match (column.ty, value) {
        (DataType::Integer, Value::Integer(n)) => out.extend_from_slice(&n.to_le_bytes()),
        (DataType::Boolean, Value::Boolean(b)) => out.push(u8::from(*b)),
        (DataType::Decimal { .. }, Value::Decimal(d)) => {
            out.push(u8::from(d.units < 0));
            out.push(d.scale);
            out.extend_from_slice(&d.units.unsigned_abs().to_le_bytes());
        }
        (DataType::Text, Value::Text(text)) => {
            if !fits_inline(value) {
                return Err(storage_error(format!(
                    "the value of {} needs a chain",
                    column.name
                )));
            }
            out.extend_from_slice(&(text.len() as u32).to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        _ => return Err(mismatch(column)),
    }
    Ok(())
}

fn encode_chain(column: &ColumnDef, ptr: ChainPtr, out: &mut Vec<u8>) -> Result<(), DbError> {
    if column.ty != DataType::Text {
        return Err(mismatch(column));
    }
    out.extend_from_slice(&CHAIN_FLAG.to_le_bytes());
    out.extend_from_slice(&ptr.head.to_le_bytes());
    out.extend_from_slice(&ptr.len.to_le_bytes());
    Ok(())
}

/// Walks a record. Every read checks that the bytes are there, so a record
/// that was cut short is an error and never a panic.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Result<&[u8], DbError> {
        let end = self.at.checked_add(len).ok_or_else(short)?;
        let taken = self.bytes.get(self.at..end).ok_or_else(short)?;
        self.at = end;
        Ok(taken)
    }

    fn u32(&mut self) -> Result<u32, DbError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64, DbError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn cell(&mut self, column: &ColumnDef) -> Result<Cell, DbError> {
        let value = match column.ty {
            DataType::Integer => {
                let b = self.take(8)?;
                Value::Integer(i64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ]))
            }
            DataType::Boolean => Value::Boolean(self.take(1)?[0] != 0),
            DataType::Decimal { .. } => {
                let negative = self.take(1)?[0] != 0;
                let scale = self.take(1)?[0];
                let magnitude = u128::from_le_bytes(
                    self.take(16)?.try_into().expect("sixteen bytes were taken"),
                );
                let units = magnitude as i128;
                Value::Decimal(Decimal {
                    units: match negative {
                        true => -units,
                        false => units,
                    },
                    scale,
                })
            }
            DataType::Text => {
                let len = self.u32()?;
                if len & CHAIN_FLAG != 0 {
                    return Ok(Cell::Chain(ChainPtr {
                        head: self.u32()?,
                        len: self.u64()?,
                    }));
                }
                let bytes = self.take(len as usize)?.to_vec();
                Value::Text(
                    String::from_utf8(bytes)
                        .map_err(|_| storage_error("the record holds text that is not UTF-8"))?,
                )
            }
        };
        Ok(Cell::Value(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, ty: DataType) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            ty,
            not_null: false,
        }
    }

    fn row(values: Vec<Value>) -> Vec<Cell> {
        values.into_iter().map(Cell::Value).collect()
    }

    /// Every column type, in one table.
    fn every_type() -> Vec<ColumnDef> {
        vec![
            column("i", DataType::Integer),
            column("t", DataType::Text),
            column("b", DataType::Boolean),
            column("d", DataType::Decimal { p: 20, s: 4 }),
        ]
    }

    #[test]
    fn every_type_reads_back_from_its_bytes() {
        let columns = every_type();
        let cells = row(vec![
            Value::Integer(i64::MIN),
            Value::Text("hé".to_string()),
            Value::Boolean(true),
            Value::Decimal(Decimal {
                units: 1220,
                scale: 2,
            }),
        ]);
        let bytes = encode_row(&columns, &cells).unwrap();
        assert_eq!(decode_row(&columns, &bytes).unwrap(), cells);
    }

    #[test]
    fn a_decimal_keeps_its_sign_its_scale_and_every_digit() {
        let columns = vec![column("d", DataType::Decimal { p: 38, s: 38 })];
        for units in [0, 1, -1, i128::MAX, i128::MIN + 1] {
            for scale in [0, 2, 38] {
                let cells = row(vec![Value::Decimal(Decimal { units, scale })]);
                let bytes = encode_row(&columns, &cells).unwrap();
                assert_eq!(
                    decode_row(&columns, &bytes).unwrap(),
                    cells,
                    "{units} {scale}"
                );
            }
        }
    }

    #[test]
    fn a_null_reads_back_as_a_null_and_takes_no_room() {
        let columns = every_type();
        let nulls = row(vec![Value::Null; 4]);
        let bytes = encode_row(&columns, &nulls).unwrap();
        assert_eq!(bytes.len(), 1, "only the bitmap");
        assert_eq!(decode_row(&columns, &bytes).unwrap(), nulls);
    }

    #[test]
    fn a_null_among_values_reads_back_in_its_place() {
        let columns = every_type();
        let cells = vec![
            Cell::Value(Value::Integer(1)),
            Cell::Value(Value::Null),
            Cell::Value(Value::Boolean(false)),
            Cell::Value(Value::Null),
        ];
        let bytes = encode_row(&columns, &cells).unwrap();
        assert_eq!(decode_row(&columns, &bytes).unwrap(), cells);
    }

    #[test]
    fn a_row_of_a_hundred_columns_reads_back() {
        let columns: Vec<ColumnDef> = (0..100)
            .map(|n| column(&format!("c{n}"), DataType::Integer))
            .collect();
        let cells = row((0..100).map(Value::Integer).collect());
        let bytes = encode_row(&columns, &cells).unwrap();
        assert_eq!(
            bytes.len(),
            13 + 100 * 8,
            "a bitmap of 13 bytes and the values"
        );
        assert_eq!(decode_row(&columns, &bytes).unwrap(), cells);
    }

    #[test]
    fn a_value_at_the_threshold_stays_in_the_record() {
        let columns = vec![column("t", DataType::Text)];
        let text = "x".repeat(INLINE_LIMIT);
        assert!(fits_inline(&Value::Text(text.clone())));
        let cells = row(vec![Value::Text(text)]);
        let bytes = encode_row(&columns, &cells).unwrap();
        assert_eq!(decode_row(&columns, &bytes).unwrap(), cells);
    }

    #[test]
    fn a_value_over_the_threshold_needs_a_chain() {
        let columns = vec![column("t", DataType::Text)];
        let text = "x".repeat(INLINE_LIMIT + 1);
        assert!(!fits_inline(&Value::Text(text.clone())));
        let e = encode_row(&columns, &row(vec![Value::Text(text)])).unwrap_err();
        assert!(e.message.contains("needs a chain"), "{e}");
        // Only text can outgrow a record.
        assert!(fits_inline(&Value::Integer(i64::MAX)));
    }

    #[test]
    fn a_chain_pointer_reads_back_in_place_of_the_value() {
        let columns = every_type();
        let ptr = ChainPtr {
            head: 42,
            len: 1024 * 1024,
        };
        let cells = vec![
            Cell::Value(Value::Integer(1)),
            Cell::Chain(ptr),
            Cell::Value(Value::Boolean(true)),
            Cell::Value(Value::Null),
        ];
        let bytes = encode_row(&columns, &cells).unwrap();
        assert_eq!(decode_row(&columns, &bytes).unwrap(), cells);
    }

    #[test]
    fn only_text_holds_a_chain() {
        let columns = vec![column("i", DataType::Integer)];
        let cells = vec![Cell::Chain(ChainPtr { head: 1, len: 1 })];
        assert_eq!(
            encode_row(&columns, &cells).unwrap_err().code,
            ErrorCode::TypeMismatch
        );
    }

    #[test]
    fn a_value_of_the_wrong_type_is_refused() {
        let cases = [
            (DataType::Integer, Value::Text("x".to_string())),
            (DataType::Text, Value::Integer(1)),
            (DataType::Boolean, Value::Integer(1)),
            (DataType::Decimal { p: 4, s: 2 }, Value::Boolean(true)),
        ];
        for (ty, value) in cases {
            let columns = vec![column("c", ty)];
            let e = encode_row(&columns, &row(vec![value])).unwrap_err();
            assert_eq!(e.code, ErrorCode::TypeMismatch, "{ty}");
        }
    }

    #[test]
    fn a_row_with_the_wrong_count_of_values_is_refused() {
        let columns = every_type();
        let e = encode_row(&columns, &row(vec![Value::Integer(1)])).unwrap_err();
        assert!(e.message.contains("for 4 columns"), "{e}");
    }

    #[test]
    fn bytes_that_end_too_soon_are_an_error_and_never_a_panic() {
        let columns = every_type();
        let cells = row(vec![
            Value::Integer(1),
            Value::Text("abc".to_string()),
            Value::Boolean(true),
            Value::Decimal(Decimal { units: 5, scale: 1 }),
        ]);
        let bytes = encode_row(&columns, &cells).unwrap();
        for cut in 0..bytes.len() {
            let e = decode_row(&columns, &bytes[..cut]).unwrap_err();
            assert!(e.message.contains("ends before"), "cut at {cut} gave {e}");
        }
    }

    #[test]
    fn a_length_that_overruns_the_record_is_an_error() {
        let columns = vec![column("t", DataType::Text)];
        // A bitmap, then a length that claims more text than is there.
        let mut bytes = vec![0];
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let e = decode_row(&columns, &bytes).unwrap_err();
        assert!(e.message.contains("ends before"), "{e}");
    }

    #[test]
    fn text_that_is_not_utf8_is_an_error() {
        let columns = vec![column("t", DataType::Text)];
        let mut bytes = vec![0];
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xff, 0xfe]);
        let e = decode_row(&columns, &bytes).unwrap_err();
        assert!(e.message.contains("not UTF-8"), "{e}");
    }

    #[test]
    fn a_row_of_no_columns_reads_back() {
        assert_eq!(encode_row(&[], &[]).unwrap(), Vec::<u8>::new());
        assert_eq!(decode_row(&[], &[]).unwrap(), Vec::<Cell>::new());
    }
}
