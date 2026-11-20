//! Parquet footer fixtures. Every fixture is a set of raw Thrift-encoded footers.
//!
//! Synthetic footers are written by arrow-rs with default properties, so they carry
//! column-chunk statistics like most real files.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use parquet::data_type::{ByteArray, ByteArrayType, Int64Type};
use parquet::file::metadata::ParquetMetaDataReader;
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::parser::parse_message_type;

/// What a fixture's byte buffers hold.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Contents {
    /// Each buffer is one Thrift-encoded footer (`FileMetaData`).
    Footer,
    /// Each buffer is a page index: Thrift-encoded `ColumnIndex` and `OffsetIndex`
    /// structs back to back.
    PageIndex,
}

/// A named set of Thrift buffers.
pub struct Fixture {
    pub name: String,
    pub contents: Contents,
    pub footers: Vec<Vec<u8>>,
}

impl Fixture {
    /// Total bytes across all footers.
    pub fn bytes(&self) -> usize {
        self.footers.iter().map(Vec::len).sum()
    }
}

/// Every fixture, smallest first. `testdata_dir` is parkay's `testdata` directory.
pub fn all(testdata_dir: &Path) -> Vec<Fixture> {
    let footer = |name: &str, footers| Fixture {
        name: name.into(),
        contents: Contents::Footer,
        footers,
    };
    vec![
        footer("parquet_testing", parquet_testing(testdata_dir)),
        // Many row groups of a narrow table: lots of small column chunks.
        footer("narrow_20x1000", vec![synthetic(20, 1000)]),
        // A wide table: about 10 MB of footer, beyond the CPU caches.
        footer("wide_1000x100", vec![synthetic(1000, 100)]),
        // A very wide schema with few row groups.
        footer("wide_10000x10", vec![synthetic(10_000, 10)]),
    ]
}

/// Fixtures from every `.parquet` file in `dir`, sorted by name: for each file, its
/// footer and, if present, its page index. Only those byte ranges are read.
///
/// Used for externally generated files, such as those from
/// <https://github.com/alamb/parquet_footer_parsing>.
pub fn from_dir(dir: &Path) -> Vec<Fixture> {
    let mut paths: Vec<_> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "parquet"))
        .collect();
    paths.sort();
    let mut fixtures = Vec::new();
    for path in paths {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let mut file = File::open(&path).unwrap();
        let file_len = file.metadata().unwrap().len();
        let mut tail = [0; 8];
        file.seek(SeekFrom::Start(file_len - 8)).unwrap();
        file.read_exact(&mut tail).unwrap();
        assert_eq!(
            &tail[4..],
            b"PAR1",
            "{}: not an unencrypted Parquet file",
            path.display()
        );
        let footer_len = u64::from(u32::from_le_bytes(tail[..4].try_into().unwrap()));
        let footer_start = file_len - 8 - footer_len;
        let footer = read_range(&mut file, footer_start, footer_start + footer_len);

        let index = page_index_range(&footer);
        fixtures.push(Fixture {
            name: format!("{stem}/metadata"),
            contents: Contents::Footer,
            footers: vec![footer],
        });
        if let Some((start, end)) = index {
            fixtures.push(Fixture {
                name: format!("{stem}/page_index"),
                contents: Contents::PageIndex,
                footers: vec![read_range(&mut file, start, end)],
            });
        }
    }
    fixtures
}

fn read_range(file: &mut File, start: u64, end: u64) -> Vec<u8> {
    let mut bytes = vec![0; usize::try_from(end - start).unwrap()];
    file.seek(SeekFrom::Start(start)).unwrap();
    file.read_exact(&mut bytes).unwrap();
    bytes
}

/// The byte range holding every column index and offset index, found from the
/// footer with arrow-rs. Panics if the indexes are not contiguous, since the
/// benchmark skips them as one run of structs.
fn page_index_range(footer: &[u8]) -> Option<(u64, u64)> {
    let metadata = ParquetMetaDataReader::decode_metadata(footer).unwrap();
    let mut ranges = Vec::new();
    for row_group in metadata.row_groups() {
        for column in row_group.columns() {
            for (offset, len) in [
                (column.column_index_offset(), column.column_index_length()),
                (column.offset_index_offset(), column.offset_index_length()),
            ] {
                if let (Some(offset), Some(len)) = (offset, len) {
                    let start = u64::try_from(offset).unwrap();
                    ranges.push((start, start + u64::try_from(len).unwrap()));
                }
            }
        }
    }
    ranges.sort_unstable();
    let (start, mut end) = *ranges.first()?;
    for &(next_start, next_end) in &ranges[1..] {
        assert_eq!(next_start, end, "page index is not contiguous");
        end = next_end;
    }
    Some((start, end))
}

/// Returns the Thrift-encoded footer of an unencrypted Parquet file.
pub fn footer(file: &[u8]) -> Option<&[u8]> {
    let (rest, magic) = file.split_last_chunk::<4>()?;
    if magic != b"PAR1" {
        return None;
    }
    let (rest, len) = rest.split_last_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    rest.get(rest.len().checked_sub(len)?..)
}

/// Footers of the unencrypted files in parquet-testing's `data` directory that
/// arrow-rs can decode, so every workload covers the same bytes. (A few test files
/// carry deliberately invalid metadata that arrow-rs rejects.)
pub fn parquet_testing(testdata_dir: &Path) -> Vec<Vec<u8>> {
    let dir = testdata_dir.join("parquet-testing/data");
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e} (run `git submodule update --init`)", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "parquet"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|path| footer(&fs::read(path).unwrap()).map(<[u8]>::to_vec))
        .filter(|footer| ParquetMetaDataReader::decode_metadata(footer).is_ok())
        .collect()
}

/// The footer of a file with `columns` columns (alternating `int64` and string) and
/// `row_groups` row groups of four rows each.
pub fn synthetic(columns: usize, row_groups: usize) -> Vec<u8> {
    let fields: String = (0..columns)
        .map(|c| {
            if c % 2 == 0 {
                format!("required int64 c{c};")
            } else {
                format!("required binary c{c} (STRING);")
            }
        })
        .collect();
    let schema = Arc::new(parse_message_type(&format!("message m {{ {fields} }}")).unwrap());
    let props = Arc::new(WriterProperties::builder().build());
    let ints: Vec<i64> = (0..4).map(|i| i * 1_000_003).collect();
    let strings: Vec<ByteArray> = ["alpha", "bravo", "charlie", "delta"]
        .into_iter()
        .map(ByteArray::from)
        .collect();

    let mut file = Vec::new();
    let mut writer = SerializedFileWriter::new(&mut file, schema, props).unwrap();
    for _ in 0..row_groups {
        let mut row_group = writer.next_row_group().unwrap();
        let mut c = 0;
        while let Some(mut column) = row_group.next_column().unwrap() {
            if c % 2 == 0 {
                column
                    .typed::<Int64Type>()
                    .write_batch(&ints, None, None)
                    .unwrap();
            } else {
                column
                    .typed::<ByteArrayType>()
                    .write_batch(&strings, None, None)
                    .unwrap();
            }
            column.close().unwrap();
            c += 1;
        }
        row_group.close().unwrap();
    }
    writer.close().unwrap();
    footer(&file).unwrap().to_vec()
}
