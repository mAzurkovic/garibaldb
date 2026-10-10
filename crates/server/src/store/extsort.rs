//! Sorting more rows than memory holds.
//!
//! Rows are held until they pass a budget, then sorted and written to `tmp/`
//! as a run. The runs merge a fan-in at a time into longer runs until one
//! merge is left, and that last one is read rather than written.
//!
//! A row carries its encoded form from the moment it arrives and its key
//! beside it, so it is encoded once, decoded once, and never decoded to be
//! compared.

use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as Atomic};

use protocol::{DbError, Value};

use crate::cancel::CancelHandle;
use crate::catalog::ColumnDef;
use crate::store::codec::{self, Cell};
use crate::store::row::Row;
use crate::store::storage_error;

/// How much of a run is read at a time. A merge holds one of these for each
/// run it reads, so this and the fan-in are the memory a merge takes.
const READ_BUFFER: usize = 64 * 1024;

/// How many runs a merge pass reads at a time.
pub const FAN_IN: usize = 160;

/// Tells the directory of one sort from another's, because two connections
/// sort under the same `tmp/` at the same time.
static NEXT_SORT: AtomicU64 = AtomicU64::new(0);

/// The directory of one sort, gone when the sort and the rows it handed out
/// are both done with it.
struct SortDir(PathBuf);

impl Drop for SortDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A row as it travels through a sort: the key it is ordered by, and the
/// bytes it was written as.
struct Held {
    key: Value,
    record: Vec<u8>,
}

/// Which of two keys comes first.
///
/// A null has no comparison, so it sorts after every value going up, which
/// puts it before them coming down. Two values of one column are of one type
/// and always compare, so the last arm is the shape of `compare` rather than
/// a case that happens.
fn order(left: &Value, right: &Value, descending: bool) -> Ordering {
    let ordering = match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (left, right) => left.compare(right).unwrap_or(Ordering::Equal),
    };
    match descending {
        true => ordering.reverse(),
        false => ordering,
    }
}

/// A run being written. Rows go out in the order they are pushed, so a merge
/// writes its result without holding it.
struct RunWriter {
    path: PathBuf,
    out: BufWriter<File>,
}

impl RunWriter {
    fn create(path: PathBuf) -> Result<RunWriter, DbError> {
        let file = File::create(&path).map_err(|e| failed(&path, e))?;
        Ok(RunWriter {
            path,
            out: BufWriter::new(file),
        })
    }

    fn push(&mut self, row: &Held) -> Result<(), DbError> {
        let len = u32::try_from(row.record.len())
            .map_err(|_| storage_error("a row too large to sort"))?;
        self.out
            .write_all(&len.to_le_bytes())
            .and_then(|()| self.out.write_all(&row.record))
            .map_err(|e| failed(&self.path, e))
    }

    /// Settles the run and closes it.
    fn finish(mut self) -> Result<RunFile, DbError> {
        self.out.flush().map_err(|e| failed(&self.path, e))?;
        Ok(RunFile { path: self.path })
    }
}

/// One run of rows in order, on disk and closed.
///
/// A run holds no file of its own, because a sort of a large table has
/// thousands of them and the open files of a process are few.
pub struct RunFile {
    path: PathBuf,
}

impl Drop for RunFile {
    /// A run goes as soon as nothing reads it, so a sort holds one copy of
    /// its rows on disk and not one for every pass.
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A run being read, a row at a time. One of these for each run of a merge,
/// so a merge holds the fan-in in open files and no more.
struct Reading {
    run: RunFile,
    reader: BufReader<File>,
    /// The row at the front, which a merge looks at without taking it.
    head: Option<Held>,
}

impl Reading {
    /// Opens a run and reads its first row.
    fn open(run: RunFile, columns: &[ColumnDef], key: usize) -> Result<Reading, DbError> {
        let file = File::open(&run.path).map_err(|e| failed(&run.path, e))?;
        let mut reading = Reading {
            reader: BufReader::with_capacity(READ_BUFFER, file),
            run,
            head: None,
        };
        reading.advance(columns, key)?;
        Ok(reading)
    }

    /// The row at the front of the run, or none at its end.
    fn head(&self) -> Option<&Held> {
        self.head.as_ref()
    }

    /// Takes the row at the front and reads the one behind it.
    fn take(&mut self, columns: &[ColumnDef], key: usize) -> Result<Option<Held>, DbError> {
        let head = self.head.take();
        self.advance(columns, key)?;
        Ok(head)
    }

    fn advance(&mut self, columns: &[ColumnDef], key: usize) -> Result<(), DbError> {
        let mut len = [0; 4];
        match self.reader.read_exact(&mut len) {
            Ok(()) => {}
            // The end of the run, which is not a short read.
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.head = None;
                return Ok(());
            }
            Err(e) => return Err(failed(&self.run.path, e)),
        }
        let mut record = vec![0; u32::from_le_bytes(len) as usize];
        self.reader
            .read_exact(&mut record)
            .map_err(|e| failed(&self.run.path, e))?;
        self.head = Some(Held {
            key: decode(columns, &record)?.swap_remove(key),
            record,
        });
        Ok(())
    }
}

/// A merge of several runs into one stream of rows in order.
struct Merge {
    runs: Vec<Reading>,
    columns: Vec<ColumnDef>,
    key: usize,
    descending: bool,
}

impl Merge {
    /// Opens the runs of one merge, which is where their files are held.
    fn open(
        runs: Vec<RunFile>,
        columns: Vec<ColumnDef>,
        key: usize,
        descending: bool,
    ) -> Result<Merge, DbError> {
        Ok(Merge {
            runs: runs
                .into_iter()
                .map(|run| Reading::open(run, &columns, key))
                .collect::<Result<_, DbError>>()?,
            columns,
            key,
            descending,
        })
    }

    /// The next row of the runs.
    ///
    /// The front of every run is looked at, so a row costs the fan-in in
    /// comparisons. A heap of the fronts would make it the logarithm of the
    /// fan-in, which is the change to make if the fan-in grows.
    fn next(&mut self) -> Result<Option<Held>, DbError> {
        let mut first: Option<usize> = None;
        for (index, run) in self.runs.iter().enumerate() {
            let Some(head) = run.head() else { continue };
            let better = match first {
                None => true,
                Some(best) => {
                    let held = &self.runs[best]
                        .head()
                        .expect("the best run holds a row")
                        .key;
                    order(&head.key, held, self.descending) == Ordering::Less
                }
            };
            if better {
                first = Some(index);
            }
        }
        match first {
            None => Ok(None),
            Some(index) => self.runs[index].take(&self.columns, self.key),
        }
    }
}

/// Rows in order, out of memory or out of a merge of runs.
pub struct Sorted {
    rows: Source,
    /// Held so the directory outlives the runs the rows come from.
    _dir: Arc<SortDir>,
}

enum Source {
    Held(std::vec::IntoIter<Held>),
    Merge(Merge),
}

impl Sorted {
    /// The next row, or none at the end.
    pub fn next(&mut self, columns: &[ColumnDef]) -> Result<Option<Row>, DbError> {
        let held = match &mut self.rows {
            Source::Held(rows) => rows.next(),
            Source::Merge(merge) => merge.next()?,
        };
        match held {
            None => Ok(None),
            Some(held) => Ok(Some(decode(columns, &held.record)?)),
        }
    }
}

/// A sort of rows that spills to disk once it holds enough of them.
pub struct ExternalSort {
    dir: Arc<SortDir>,
    columns: Vec<ColumnDef>,
    key: usize,
    descending: bool,
    /// The bytes of records held before a run is written.
    budget: u64,
    fan_in: usize,
    held: Vec<Held>,
    bytes: u64,
    runs: Vec<RunFile>,
    /// Names the next run. A run is never written twice.
    written: usize,
}

impl ExternalSort {
    /// Opens a sort with a directory of its own under `tmp`.
    pub fn new(
        tmp: &Path,
        columns: Vec<ColumnDef>,
        key: usize,
        descending: bool,
        budget: u64,
        fan_in: usize,
    ) -> Result<ExternalSort, DbError> {
        let dir = tmp.join(format!(
            "{}-{}",
            std::process::id(),
            NEXT_SORT.fetch_add(1, Atomic::Relaxed)
        ));
        fs::create_dir_all(&dir).map_err(|e| failed(&dir, e))?;
        Ok(ExternalSort {
            dir: Arc::new(SortDir(dir)),
            columns,
            key,
            descending,
            budget,
            fan_in,
            held: Vec::new(),
            bytes: 0,
            runs: Vec::new(),
            written: 0,
        })
    }

    /// Takes one row, spilling a run when the held rows pass the budget.
    pub fn add(&mut self, row: &Row) -> Result<(), DbError> {
        let cells: Vec<Cell> = row.iter().cloned().map(Cell::Value).collect();
        let record = codec::encode_row(&self.columns, &cells)?;
        self.bytes += record.len() as u64;
        self.held.push(Held {
            key: row[self.key].clone(),
            record,
        });
        match self.bytes >= self.budget {
            true => self.spill(),
            false => Ok(()),
        }
    }

    /// Settles the sort and hands back its rows in order.
    pub fn finish(mut self, cancel: &CancelHandle) -> Result<Sorted, DbError> {
        if self.runs.is_empty() {
            let mut held = std::mem::take(&mut self.held);
            held.sort_by(|left, right| order(&left.key, &right.key, self.descending));
            return Ok(Sorted {
                rows: Source::Held(held.into_iter()),
                _dir: Arc::clone(&self.dir),
            });
        }
        self.spill()?;
        // Down to one merge, which is read rather than written.
        while self.runs.len() > self.fan_in {
            self.pass(cancel)?;
        }
        Ok(Sorted {
            rows: Source::Merge(Merge::open(
                std::mem::take(&mut self.runs),
                self.columns.clone(),
                self.key,
                self.descending,
            )?),
            _dir: Arc::clone(&self.dir),
        })
    }

    /// Writes the held rows to a run of their own.
    fn spill(&mut self) -> Result<(), DbError> {
        if self.held.is_empty() {
            return Ok(());
        }
        let mut held = std::mem::take(&mut self.held);
        self.bytes = 0;
        held.sort_by(|left, right| order(&left.key, &right.key, self.descending));
        let mut run = RunWriter::create(self.path())?;
        for row in &held {
            run.push(row)?;
        }
        self.runs.push(run.finish()?);
        Ok(())
    }

    /// Merges the runs a fan-in at a time into longer ones.
    fn pass(&mut self, cancel: &CancelHandle) -> Result<(), DbError> {
        let mut left = std::mem::take(&mut self.runs);
        let mut merged = Vec::new();
        while !left.is_empty() {
            let take = left.len().min(self.fan_in);
            let mut merge = Merge::open(
                left.drain(..take).collect(),
                self.columns.clone(),
                self.key,
                self.descending,
            )?;
            let mut run = RunWriter::create(self.path())?;
            // Straight out of the merge and into the run, so a pass holds no
            // more rows than the fronts of the runs it reads.
            while let Some(row) = merge.next()? {
                cancel.check()?;
                run.push(&row)?;
            }
            merged.push(run.finish()?);
        }
        self.runs = merged;
        Ok(())
    }

    fn path(&mut self) -> PathBuf {
        let path = self.dir.0.join(format!("{}.run", self.written));
        self.written += 1;
        path
    }
}

/// The values of a record. Nothing in a run lives in a chain, because a run
/// has no page to outgrow.
fn decode(columns: &[ColumnDef], record: &[u8]) -> Result<Row, DbError> {
    codec::decode_row(columns, record)?
        .into_iter()
        .map(|cell| match cell {
            Cell::Value(value) => Ok(value),
            Cell::Chain(_) => Err(storage_error("a run holds no chain")),
        })
        .collect()
}

fn failed(path: &Path, e: std::io::Error) -> DbError {
    storage_error(format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use protocol::DataType;

    use super::*;
    use crate::catalog::testing::Dir;

    /// A table of a key column and a label, so a row is a few dozen bytes.
    fn columns(ty: DataType) -> Vec<ColumnDef> {
        vec![
            ColumnDef {
                name: "key".to_string(),
                ty,
                not_null: false,
            },
            ColumnDef {
                name: "label".to_string(),
                ty: DataType::Text,
                not_null: false,
            },
        ]
    }

    fn row(key: Value) -> Row {
        vec![key, Value::Text("a label of some length".to_string())]
    }

    /// Sorts rows and hands back the keys in the order they come out.
    fn sorted(
        dir: &Dir,
        ty: DataType,
        keys: Vec<Value>,
        descending: bool,
        budget: u64,
        fan_in: usize,
    ) -> Vec<Value> {
        let columns = columns(ty);
        let mut sort =
            ExternalSort::new(&dir.0, columns.clone(), 0, descending, budget, fan_in).unwrap();
        for key in keys {
            sort.add(&row(key)).unwrap();
        }
        let mut rows = sort.finish(&CancelHandle::new()).unwrap();
        let mut out = Vec::new();
        while let Some(row) = rows.next(&columns).unwrap() {
            out.push(row[0].clone());
        }
        out
    }

    fn ints(keys: &[i64]) -> Vec<Value> {
        keys.iter().copied().map(Value::Integer).collect()
    }

    /// How many files and directories sit under the sort root.
    fn files(dir: &Dir) -> usize {
        fs::read_dir(&dir.0).map(|read| read.count()).unwrap_or(0)
    }

    #[test]
    fn rows_that_fit_the_budget_are_sorted_in_memory() {
        let dir = Dir::new("sort-in-memory");
        let keys = sorted(&dir, DataType::Integer, ints(&[3, 1, 2]), false, 1 << 20, 2);
        assert_eq!(keys, ints(&[1, 2, 3]));
        assert_eq!(files(&dir), 0, "nothing was written and the directory went");
    }

    #[test]
    fn rows_past_the_budget_spill_and_come_back_in_order() {
        let dir = Dir::new("sort-spill");
        let keys: Vec<i64> = (0..200).map(|n| (n * 7919) % 200).collect();
        let mut want = keys.clone();
        want.sort_unstable();

        // A budget of a few rows, so the sort writes many runs and merges
        // them two at a time, which takes several passes.
        let got = sorted(&dir, DataType::Integer, ints(&keys), false, 128, 2);

        assert_eq!(got, ints(&want));
        assert_eq!(files(&dir), 0, "every run went with the sort");
    }

    #[test]
    fn a_descending_sort_is_the_ascending_one_reversed() {
        let dir = Dir::new("sort-descending");
        let keys: Vec<i64> = (0..60).map(|n| (n * 37) % 60).collect();
        let mut want = keys.clone();
        want.sort_unstable();
        want.reverse();

        assert_eq!(
            sorted(&dir, DataType::Integer, ints(&keys), true, 128, 2),
            ints(&want)
        );
    }

    #[test]
    fn a_merge_of_one_run_reads_it_straight_back() {
        let dir = Dir::new("sort-one-run");
        // A budget small enough for one spill and no more.
        let keys = sorted(&dir, DataType::Integer, ints(&[2, 1]), false, 1, 2);
        assert_eq!(keys, ints(&[1, 2]));
    }

    #[test]
    fn every_type_sorts_by_its_own_order() {
        let dir = Dir::new("sort-types");
        let cases = [
            (
                DataType::Text,
                vec!["be", "a", "hé", "B"],
                vec!["B", "a", "be", "hé"],
            ),
            (DataType::Text, vec!["b", "a"], vec!["a", "b"]),
        ];
        for (ty, given, want) in cases {
            let keys = given
                .iter()
                .map(|text| Value::Text(text.to_string()))
                .collect();
            let want: Vec<Value> = want
                .iter()
                .map(|text| Value::Text(text.to_string()))
                .collect();
            // Text by the bytes of its UTF-8, so a capital sorts before a
            // lower case letter and an accent after both.
            assert_eq!(sorted(&dir, ty, keys, false, 128, 2), want);
        }

        let truths = vec![Value::Boolean(true), Value::Boolean(false)];
        assert_eq!(
            sorted(&dir, DataType::Boolean, truths, false, 128, 2),
            vec![Value::Boolean(false), Value::Boolean(true)]
        );

        // A decimal by its value, so the scale makes no difference.
        let ty = DataType::Decimal { p: 10, s: 2 };
        let given = vec![
            Value::Decimal("12.20".parse().unwrap()),
            Value::Decimal("2.5".parse().unwrap()),
            Value::Decimal("12.2".parse().unwrap()),
        ];
        let got = sorted(&dir, ty, given, false, 128, 2);
        assert_eq!(got[0], Value::Decimal("2.5".parse().unwrap()));
        assert_eq!(
            got[1].compare(&got[2]),
            Some(Ordering::Equal),
            "12.20 and 12.2 are one value"
        );
    }

    #[test]
    fn a_null_sorts_last_going_up_and_first_coming_down() {
        let dir = Dir::new("sort-nulls");
        // Two nulls as well, which are equal to one another.
        let given = || {
            vec![
                Value::Integer(2),
                Value::Null,
                Value::Integer(1),
                Value::Null,
            ]
        };
        assert_eq!(
            sorted(&dir, DataType::Integer, given(), false, 1 << 20, 2),
            vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Null,
                Value::Null
            ]
        );
        assert_eq!(
            sorted(&dir, DataType::Integer, given(), true, 1 << 20, 2),
            vec![
                Value::Null,
                Value::Null,
                Value::Integer(2),
                Value::Integer(1)
            ]
        );
        // And through the runs, where the key is read back off the disk.
        assert_eq!(
            sorted(&dir, DataType::Integer, given(), false, 1, 2),
            vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Null,
                Value::Null
            ]
        );
    }

    #[test]
    fn a_sort_of_many_runs_holds_the_fan_in_in_open_files() {
        let dir = Dir::new("sort-many-runs");
        let columns = columns(DataType::Integer);
        // A run for every row, and far more runs than the fan-in, so the
        // merge opens a few of them at a time and never all of them.
        let mut sort = ExternalSort::new(&dir.0, columns.clone(), 0, false, 1, 4).unwrap();
        for key in (0..400).rev() {
            sort.add(&row(Value::Integer(key))).unwrap();
        }

        let mut rows = sort.finish(&CancelHandle::new()).unwrap();
        let mut keys = Vec::new();
        while let Some(row) = rows.next(&columns).unwrap() {
            keys.push(row[0].clone());
        }
        assert_eq!(keys, ints(&(0..400).collect::<Vec<i64>>()));
    }

    #[test]
    fn a_merge_stops_when_the_statement_is_cancelled() {
        let dir = Dir::new("sort-cancelled");
        let columns = columns(DataType::Integer);
        let mut sort = ExternalSort::new(&dir.0, columns, 0, false, 128, 2).unwrap();
        for key in 0..100 {
            sort.add(&row(Value::Integer(key))).unwrap();
        }

        // More runs than the fan-in, so the merge happens inside `finish`,
        // which is the one long loop below the operators.
        let cancel = CancelHandle::new();
        cancel.stop();
        let e = sort.finish(&cancel).err().unwrap();

        assert_eq!(e.code, protocol::ErrorCode::Cancelled);
        assert_eq!(files(&dir), 0, "the runs went with it");
    }

    #[test]
    fn a_sort_that_is_dropped_part_way_leaves_nothing_behind() {
        let dir = Dir::new("sort-dropped");
        let columns = columns(DataType::Integer);
        let mut sort = ExternalSort::new(&dir.0, columns, 0, false, 128, 2).unwrap();
        for key in 0..100 {
            sort.add(&row(Value::Integer(key))).unwrap();
        }
        assert!(files(&dir) > 0, "it spilled while it ran");

        drop(sort);

        assert_eq!(files(&dir), 0);
    }

    #[test]
    fn a_sort_that_cannot_make_its_directory_is_an_error() {
        let dir = Dir::new("sort-no-room");
        // A file where the sort wants its directory.
        let path = dir.0.join("tmp");
        fs::write(&path, b"not a directory").expect("the file writes");

        let e = ExternalSort::new(&path, columns(DataType::Integer), 0, false, 128, 2)
            .err()
            .unwrap();
        assert_eq!(e.code, protocol::ErrorCode::StorageFull);
        assert!(e.message.contains("tmp"), "{e}");
    }

    #[test]
    fn a_sort_of_nothing_gives_nothing() {
        let dir = Dir::new("sort-empty");
        assert!(sorted(&dir, DataType::Integer, Vec::new(), false, 128, 2).is_empty());
    }
}
