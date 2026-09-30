//! The answer of a statement, as a table. See [FR74], [FR75], and [FR76].

use std::io::{self, Write};

use protocol::{ColumnDesc, DataType, DbError, Value};

/// How many rows fix the column widths.
///
/// [FR75] forbids a wait for the last row, so a width cannot depend on a row
/// that has not arrived. A wider value after these overflows its column.
const WIDTH_SAMPLE: usize = 50;

/// Writes rows as they arrive.
pub struct TableWriter<W> {
    out: W,
    names: Vec<String>,
    /// Which columns hold a number, which reads better against the right edge.
    right: Vec<bool>,
    widths: Vec<usize>,
    /// The rows that still wait for the widths.
    held: Vec<Vec<String>>,
    /// Whether the header reached the screen, which fixes the widths.
    open: bool,
    rows: u64,
}

impl<W: Write> TableWriter<W> {
    pub fn new(out: W) -> TableWriter<W> {
        TableWriter {
            out,
            names: Vec::new(),
            right: Vec::new(),
            widths: Vec::new(),
            held: Vec::new(),
            open: false,
            rows: 0,
        }
    }

    /// Takes the columns of the answer. Nothing prints yet, because the widths
    /// need rows. See [FR74].
    pub fn header(&mut self, cols: &[ColumnDesc]) {
        self.names = cols.iter().map(|c| c.name.clone()).collect();
        self.right = cols.iter().map(|c| is_number(c.ty)).collect();
        self.widths = self.names.iter().map(|n| width(n)).collect();
    }

    /// Takes one row. The first rows wait for their widths, and every row
    /// after them prints at once. See [FR75].
    pub fn row(&mut self, values: &[Value]) -> io::Result<()> {
        let cells: Vec<String> = values.iter().map(cell).collect();
        self.rows += 1;
        if self.widths.len() < cells.len() {
            self.widths.resize(cells.len(), 0);
            self.right.resize(cells.len(), false);
        }
        if self.open {
            return self.write_row(&cells);
        }
        for (i, c) in cells.iter().enumerate() {
            self.widths[i] = self.widths[i].max(width(c));
        }
        self.held.push(cells);
        if self.held.len() >= WIDTH_SAMPLE {
            self.open_table()?;
        }
        Ok(())
    }

    /// Ends the answer with its row count.
    pub fn footer(&mut self) -> io::Result<()> {
        if !self.open && !(self.names.is_empty() && self.held.is_empty()) {
            self.open_table()?;
        }
        let rows = self.rows;
        let word = if rows == 1 { "row" } else { "rows" };
        writeln!(self.out, "({rows} {word})")
    }

    /// Writes a failed statement. See [FR76].
    pub fn error(&mut self, error: &DbError) -> io::Result<()> {
        let DbError {
            code,
            message,
            position,
        } = error;
        match position {
            Some(at) => writeln!(self.out, "{code}: {message} at character {at}"),
            None => writeln!(self.out, "{code}: {message}"),
        }
    }

    /// Prints the header and every row that waited for it.
    fn open_table(&mut self) -> io::Result<()> {
        self.open = true;
        let left = vec![false; self.widths.len()];
        let header = line(&self.names, &self.widths, &left);
        let rule = self
            .widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<String>>()
            .join("+");
        writeln!(self.out, "{header}")?;
        writeln!(self.out, "{rule}")?;
        for cells in std::mem::take(&mut self.held) {
            self.write_row(&cells)?;
        }
        Ok(())
    }

    fn write_row(&mut self, cells: &[String]) -> io::Result<()> {
        let text = line(cells, &self.widths, &self.right);
        writeln!(self.out, "{text}")
    }
}

/// The text of one value. `Null` is the word, because an empty cell would read
/// as an empty string.
fn cell(value: &Value) -> String {
    match value {
        Value::Integer(i) => i.to_string(),
        Value::Text(t) => t.clone(),
        Value::Boolean(b) => b.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Null => "NULL".to_string(),
    }
}

fn is_number(ty: DataType) -> bool {
    matches!(ty, DataType::Integer | DataType::Decimal { .. })
}

fn width(text: &str) -> usize {
    text.chars().count()
}

/// One line of the table. A cell wider than its column overflows it, which
/// keeps the row readable when a late value is the widest.
fn line(cells: &[String], widths: &[usize], right: &[bool]) -> String {
    let mut out = String::new();
    for (i, text) in cells.iter().enumerate() {
        if i > 0 {
            out.push('|');
        }
        let pad = " ".repeat(widths[i].saturating_sub(width(text)));
        out.push(' ');
        if right[i] {
            out.push_str(&pad);
            out.push_str(text);
        } else {
            out.push_str(text);
            out.push_str(&pad);
        }
        out.push(' ');
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{Decimal, ErrorCode};

    fn col(name: &str, ty: DataType) -> ColumnDesc {
        ColumnDesc {
            name: name.to_string(),
            ty,
        }
    }

    /// Renders an answer and returns what reached the screen.
    fn render(cols: &[ColumnDesc], rows: &[Vec<Value>]) -> String {
        let mut out = Vec::new();
        let mut table = TableWriter::new(&mut out);
        table.header(cols);
        for row in rows {
            table.row(row).unwrap();
        }
        table.footer().unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_table_aligns_its_columns_and_counts_its_rows() {
        let cols = [col("id", DataType::Integer), col("name", DataType::Text)];
        let rows = [
            vec![Value::Integer(1), Value::Text("ada".to_string())],
            vec![Value::Integer(100), Value::Text("bo".to_string())],
        ];
        assert_eq!(
            render(&cols, &rows),
            " id  | name\n-----+------\n   1 | ada\n 100 | bo\n(2 rows)\n"
        );
    }

    #[test]
    fn one_row_reads_as_a_row_and_not_as_rows() {
        let cols = [col("a", DataType::Integer)];
        let rows = [vec![Value::Integer(7)]];
        assert!(render(&cols, &rows).ends_with("(1 row)\n"));
    }

    #[test]
    fn an_answer_with_no_row_still_shows_its_columns() {
        let cols = [col("a", DataType::Integer)];
        assert_eq!(render(&cols, &[]), " a\n---\n(0 rows)\n");
    }

    #[test]
    fn a_statement_with_no_columns_shows_only_the_count() {
        let mut out = Vec::new();
        let mut table = TableWriter::new(&mut out);
        table.footer().unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "(0 rows)\n");
    }

    #[test]
    fn a_null_shows_the_word_and_every_type_shows_its_text() {
        let cols = [
            col("i", DataType::Integer),
            col("t", DataType::Text),
            col("b", DataType::Boolean),
            col("d", DataType::Decimal { p: 6, s: 2 }),
            col("n", DataType::Text),
        ];
        let rows = [vec![
            Value::Integer(-3),
            Value::Text("hé".to_string()),
            Value::Boolean(true),
            Value::Decimal(Decimal {
                units: 1220,
                scale: 2,
            }),
            Value::Null,
        ]];
        let shown = render(&cols, &rows);
        let row = shown.lines().nth(2).unwrap();
        assert_eq!(row, " -3 | hé | true | 12.20 | NULL");
    }

    #[test]
    fn a_value_wider_than_its_header_widens_the_column() {
        let cols = [col("a", DataType::Text)];
        let rows = [vec![Value::Text("wide value".to_string())]];
        assert_eq!(
            render(&cols, &rows),
            " a\n------------\n wide value\n(1 row)\n"
        );
    }

    #[test]
    fn nothing_prints_while_the_rows_still_fix_the_widths() {
        let cols = [col("a", DataType::Text)];
        let mut out = Vec::new();
        let mut table = TableWriter::new(&mut out);
        table.header(&cols);
        for i in 0..WIDTH_SAMPLE - 1 {
            table.row(&[Value::Text(format!("r{i}"))]).unwrap();
        }
        drop(table);
        assert!(out.is_empty(), "the widths are not fixed yet");
    }

    #[test]
    fn the_row_that_fixes_the_widths_brings_every_held_row_with_it() {
        let cols = [col("a", DataType::Text)];
        let mut out = Vec::new();
        let mut table = TableWriter::new(&mut out);
        table.header(&cols);
        for i in 0..WIDTH_SAMPLE {
            table.row(&[Value::Text(format!("r{i}"))]).unwrap();
        }
        drop(table);
        let shown = String::from_utf8(out).unwrap();
        assert_eq!(shown.lines().count(), WIDTH_SAMPLE + 2);
        assert!(shown.ends_with(" r49\n"), "{shown}");
    }

    #[test]
    fn a_value_wider_than_the_fixed_width_overflows_its_column() {
        let cols = [col("a", DataType::Text), col("b", DataType::Text)];
        let mut out = Vec::new();
        let mut table = TableWriter::new(&mut out);
        table.header(&cols);
        for _ in 0..WIDTH_SAMPLE {
            table
                .row(&[Value::Text("x".to_string()), Value::Text("y".to_string())])
                .unwrap();
        }
        table
            .row(&[
                Value::Text("a much wider value".to_string()),
                Value::Text("y".to_string()),
            ])
            .unwrap();
        drop(table);
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.ends_with(" a much wider value | y\n"), "{shown}");
    }

    #[test]
    fn a_row_with_more_values_than_columns_still_prints() {
        let cols = [col("a", DataType::Integer)];
        let rows = [vec![Value::Integer(1), Value::Text("extra".to_string())]];
        assert_eq!(
            render(&cols, &rows),
            " a\n---+-------\n 1 | extra\n(1 row)\n"
        );
    }

    #[test]
    fn an_error_shows_its_code_and_its_message() {
        let mut out = Vec::new();
        let mut table = TableWriter::new(&mut out);
        table
            .error(&DbError {
                code: ErrorCode::UnknownTable,
                message: "the server holds no table".to_string(),
                position: None,
            })
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "UNKNOWN_TABLE: the server holds no table\n"
        );
    }

    #[test]
    fn an_error_with_a_position_names_the_character() {
        let mut out = Vec::new();
        let mut table = TableWriter::new(&mut out);
        table
            .error(&DbError {
                code: ErrorCode::SyntaxError,
                message: "unexpected token".to_string(),
                position: Some(7),
            })
            .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "SYNTAX_ERROR: unexpected token at character 7\n"
        );
    }

    /// A screen that has gone away. Every write fails.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_broken_screen_reports_its_error_and_panics_nothing() {
        let cols = [col("a", DataType::Text)];

        let mut table = TableWriter::new(Broken);
        assert!(
            table
                .error(&DbError {
                    code: ErrorCode::UnknownTable,
                    message: "gone".to_string(),
                    position: Some(1),
                })
                .is_err()
        );

        let mut table = TableWriter::new(Broken);
        table.header(&cols);
        assert!(table.footer().is_err(), "the header reaches the screen");

        // Held rows write nothing, so the row that fixes the widths is the
        // first one that can fail. Every row after it writes on its own.
        let mut table = TableWriter::new(Broken);
        table.header(&cols);
        for i in 0..WIDTH_SAMPLE - 1 {
            table.row(&[Value::Text(format!("r{i}"))]).unwrap();
        }
        assert!(table.row(&[Value::Text("last".to_string())]).is_err());
        assert!(table.row(&[Value::Text("after".to_string())]).is_err());
    }
}
