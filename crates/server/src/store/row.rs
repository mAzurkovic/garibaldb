//! One row, between its values and the bytes a page holds.
//!
//! A value too large for a record lives in a chain of its own pages, so
//! storing a row writes those chains first and reading one reads them back.

use protocol::{DbError, Value};

use crate::catalog::ColumnDef;
use crate::store::btree::MAX_RECORD;
use crate::store::codec::{self, Cell, ChainPtr};
use crate::store::page::FileId;
use crate::store::pool::BufferPool;
use crate::store::{overflow, storage_error};

/// The values of one row, in the order its columns hold.
pub type Row = Vec<Value>;

/// Encodes a row, moving every value too large for a record into a chain.
///
/// A row that still does not fit a page is refused, and the chains it wrote
/// along the way are freed, so a refused row leaves nothing behind.
pub fn store(
    pool: &BufferPool,
    file: FileId,
    columns: &[ColumnDef],
    values: &[Value],
) -> Result<Vec<u8>, DbError> {
    let mut cells = Vec::with_capacity(values.len());
    let mut written = Vec::new();
    for value in values {
        match value {
            Value::Text(text) if text.len() > codec::INLINE_LIMIT => {
                let ptr = overflow::write(pool, file, text.as_bytes())?;
                written.push(ptr);
                cells.push(Cell::Chain(ptr));
            }
            inline => cells.push(Cell::Value(inline.clone())),
        }
    }
    let encoded = codec::encode_row(columns, &cells).and_then(|row| match row.len() > MAX_RECORD {
        true => Err(storage_error(format!(
            "a row of {} bytes does not fit a page",
            row.len()
        ))),
        false => Ok(row),
    });
    match encoded {
        Ok(row) => Ok(row),
        Err(e) => {
            abandon(pool, file, &written);
            Err(e)
        }
    }
}

/// Reads a row back, following the chain of every value that lives in one.
pub fn load(
    pool: &BufferPool,
    file: FileId,
    columns: &[ColumnDef],
    bytes: &[u8],
) -> Result<Row, DbError> {
    codec::decode_row(columns, bytes)?
        .into_iter()
        .map(|cell| match cell {
            Cell::Value(value) => Ok(value),
            Cell::Chain(ptr) => {
                let bytes = overflow::read(pool, file, ptr)?;
                String::from_utf8(bytes)
                    .map(Value::Text)
                    .map_err(|_| storage_error("a chain holds text that is not UTF-8"))
            }
        })
        .collect()
}

/// Frees the chain of every value of a row that is going.
pub fn free(
    pool: &BufferPool,
    file: FileId,
    columns: &[ColumnDef],
    bytes: &[u8],
) -> Result<(), DbError> {
    for cell in codec::decode_row(columns, bytes)? {
        if let Cell::Chain(ptr) = cell {
            overflow::free(pool, file, ptr)?;
        }
    }
    Ok(())
}

/// Gives back the chains of a row that will not be stored after all. The
/// failure that got here is the one worth reporting, so a page that will not
/// come back is left to the sweep at the next start.
fn abandon(pool: &BufferPool, file: FileId, written: &[ChainPtr]) {
    for ptr in written {
        let _ = overflow::free(pool, file, *ptr);
    }
}

#[cfg(test)]
mod tests {
    use protocol::{DataType, Decimal};

    use super::*;
    use std::sync::Arc;

    use crate::catalog::testing::{self, Dir};
    use crate::store::page::PAGE_SIZE;
    use crate::wal::writer::Wal;

    fn column(name: &str, ty: DataType) -> ColumnDef {
        ColumnDef {
            name: name.to_string(),
            ty,
            not_null: false,
        }
    }

    fn fixture(label: &str) -> (Dir, BufferPool, Arc<Wal>, FileId) {
        testing::table(label, 16)
    }

    /// How many pages the file holds, which says whether a free gave any back.
    fn pages(pool: &BufferPool, file: FileId) -> u32 {
        let mut count = 1;
        while pool
            .fetch(crate::store::page::PageId::new(file, count))
            .is_ok()
        {
            count += 1;
        }
        count
    }

    #[test]
    fn every_type_reads_back_from_its_bytes() {
        let (_dir, pool, _wal, file) = fixture("types");
        let columns = vec![
            column("i", DataType::Integer),
            column("t", DataType::Text),
            column("b", DataType::Boolean),
            column("d", DataType::Decimal { p: 10, s: 2 }),
            column("n", DataType::Text),
        ];
        let values = vec![
            Value::Integer(-9),
            Value::Text("hé".to_string()),
            Value::Boolean(true),
            Value::Decimal(Decimal {
                units: 1220,
                scale: 2,
            }),
            Value::Null,
        ];
        let bytes = store(&pool, file, &columns, &values).unwrap();
        assert_eq!(load(&pool, file, &columns, &bytes).unwrap(), values);
    }

    #[test]
    fn a_value_of_a_megabyte_goes_to_a_chain_and_comes_back() {
        let (_dir, pool, _wal, file) = fixture("megabyte");
        let columns = vec![column("t", DataType::Text)];
        let text = "x".repeat(1024 * 1024);
        let values = vec![Value::Text(text.clone())];

        let bytes = store(&pool, file, &columns, &values).unwrap();
        assert!(bytes.len() < PAGE_SIZE, "the record keeps only a pointer");
        assert_eq!(load(&pool, file, &columns, &bytes).unwrap(), values);
    }

    #[test]
    fn a_value_at_the_inline_limit_stays_in_the_record() {
        let (_dir, pool, _wal, file) = fixture("inline");
        let columns = vec![column("t", DataType::Text)];
        let values = vec![Value::Text("x".repeat(codec::INLINE_LIMIT))];
        let before = pages(&pool, file);
        let bytes = store(&pool, file, &columns, &values).unwrap();
        assert_eq!(pages(&pool, file), before, "no chain page was taken");
        assert_eq!(load(&pool, file, &columns, &bytes).unwrap(), values);
    }

    #[test]
    fn a_row_of_a_hundred_columns_reads_back() {
        let (_dir, pool, _wal, file) = fixture("hundred");
        let columns: Vec<ColumnDef> = (0..100)
            .map(|n| column(&format!("c{n}"), DataType::Integer))
            .collect();
        let values: Vec<Value> = (0..100).map(Value::Integer).collect();
        let bytes = store(&pool, file, &columns, &values).unwrap();
        assert_eq!(load(&pool, file, &columns, &bytes).unwrap(), values);
    }

    #[test]
    fn a_row_too_wide_for_a_page_is_refused_and_leaves_no_chain() {
        let (_dir, pool, _wal, file) = fixture("too-wide");
        // One value over the inline limit, which takes a chain, and four at
        // the limit, which stay in the record and together outgrow a page.
        let columns: Vec<ColumnDef> = (0..5)
            .map(|n| column(&format!("t{n}"), DataType::Text))
            .collect();
        let wide = "x".repeat(codec::INLINE_LIMIT + 1);
        let mut values = vec![Value::Text(wide.clone())];
        for _ in 0..4 {
            values.push(Value::Text("x".repeat(codec::INLINE_LIMIT)));
        }

        let before = pages(&pool, file);
        let e = store(&pool, file, &columns, &values).err().unwrap();
        assert!(e.message.contains("does not fit a page"), "{e}");

        // The chains it wrote came back, so the next row takes those pages.
        let one = vec![column("t", DataType::Text)];
        store(&pool, file, &one, &[Value::Text(wide)]).unwrap();
        assert!(
            pages(&pool, file) <= before + 1,
            "the refused row left pages behind"
        );
    }

    #[test]
    fn a_value_of_the_wrong_type_is_refused_and_leaves_no_chain() {
        let (_dir, pool, _wal, file) = fixture("wrong-type");
        let columns = vec![column("t", DataType::Text), column("i", DataType::Integer)];
        let before = pages(&pool, file);
        let values = vec![
            Value::Text("x".repeat(codec::INLINE_LIMIT + 1)),
            Value::Text("not an integer".to_string()),
        ];
        assert!(store(&pool, file, &columns, &values).is_err());

        let one = vec![column("t", DataType::Text)];
        store(
            &pool,
            file,
            &one,
            &[Value::Text("x".repeat(codec::INLINE_LIMIT + 1))],
        )
        .unwrap();
        assert!(
            pages(&pool, file) <= before + 1,
            "the refused row left pages behind"
        );
    }

    #[test]
    fn freeing_a_row_gives_the_pages_of_its_chains_back() {
        let (_dir, pool, _wal, file) = fixture("free");
        let columns = vec![column("t", DataType::Text)];
        let values = vec![Value::Text("x".repeat(PAGE_SIZE * 3))];
        let bytes = store(&pool, file, &columns, &values).unwrap();
        let grown = pages(&pool, file);

        free(&pool, file, &columns, &bytes).unwrap();
        store(&pool, file, &columns, &values).unwrap();
        assert!(
            pages(&pool, file) <= grown,
            "the file grew instead of reusing the freed pages"
        );
    }

    #[test]
    fn freeing_a_row_that_holds_no_chain_frees_nothing() {
        let (_dir, pool, _wal, file) = fixture("free-none");
        let columns = vec![column("i", DataType::Integer)];
        let bytes = store(&pool, file, &columns, &[Value::Integer(1)]).unwrap();
        let before = pages(&pool, file);
        free(&pool, file, &columns, &bytes).unwrap();
        assert_eq!(pages(&pool, file), before);
    }

    #[test]
    fn a_chain_that_holds_text_which_is_not_utf8_is_an_error() {
        let (_dir, pool, _wal, file) = fixture("not-utf8");
        let columns = vec![column("t", DataType::Text)];
        // A chain written by hand, holding bytes no string could hold.
        let ptr = overflow::write(&pool, file, &[0xff; 4096]).unwrap();
        let bytes = codec::encode_row(&columns, &[Cell::Chain(ptr)]).unwrap();
        let e = load(&pool, file, &columns, &bytes).err().unwrap();
        assert!(e.message.contains("not UTF-8"), "{e}");
    }
}
