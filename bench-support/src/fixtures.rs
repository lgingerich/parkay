//! Parquet footer fixtures for tests, fuzz seeds, and `--files` benchmark inputs.
//! Synthetic generators and the benchmark corpus live in `corpus.rs`.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use parquet::file::metadata::{
    ParquetMetaDataOptions, ParquetMetaDataReader, ParquetStatisticsPolicy,
};

/// What a fixture's byte buffers hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contents {
    /// Each buffer is one Thrift-encoded footer (`FileMetaData`).
    Footer,
    /// Each buffer is a page index: Thrift-encoded `ColumnIndex` and `OffsetIndex`
    /// structs back to back.
    PageIndex,
}

/// A named set of Thrift buffers.
#[derive(Debug)]
pub struct Fixture {
    pub name: String,
    pub contents: Contents,
    pub footers: Vec<Vec<u8>>,
    /// For each buffer, the size of the file it came from. A footer's offsets fall
    /// within it.
    pub file_sizes: Vec<u64>,
}

impl Fixture {
    /// Total bytes across all footers.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.footers.iter().map(Vec::len).sum()
    }
}

/// Fixtures from every `.parquet` file in `dir`, sorted by name: for each file, its
/// footer and, if present and contiguous, its page index. Only those byte ranges are read.
///
/// Fixture names match [`crate::corpus`] conventions (`{stem}` and `{stem}_page_index`)
/// so Criterion directory names are consistent across corpus and external inputs.
///
/// Used for externally generated files, such as those from
/// <https://github.com/alamb/parquet_footer_parsing>.
///
/// # Errors
///
/// Returns an error if `dir` cannot be read or any `.parquet` file in it is unreadable,
/// truncated, or not an unencrypted Parquet file.
pub(crate) fn from_dir(dir: &Path) -> io::Result<Vec<Fixture>> {
    from_dir_inner(dir, true)
}

/// Footer fixtures only from every `.parquet` file in `dir`, sorted by name, skipping
/// page indexes.
///
/// # Errors
///
/// Returns an error if `dir` cannot be read or any `.parquet` file in it is unreadable,
/// truncated, or not an unencrypted Parquet file.
pub(crate) fn footers_from_dir(dir: &Path) -> io::Result<Vec<Fixture>> {
    from_dir_inner(dir, false)
}

/// Opens `path`, validates its `PAR1` trailer, and reads only the Thrift footer bytes.
fn read_parquet_footer(path: &Path) -> io::Result<(File, Vec<u8>, u64)> {
    let mut file = File::open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
    let file_len = file.metadata()?.len();
    let tail_offset = file_len.checked_sub(8).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: file too small ({file_len} bytes) to contain a Parquet footer",
                path.display()
            ),
        )
    })?;
    let mut tail = [0_u8; 8];
    file.seek(SeekFrom::Start(tail_offset))?;
    file.read_exact(&mut tail)?;
    if &tail[4..] != b"PAR1" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: not an unencrypted Parquet file", path.display()),
        ));
    }
    let footer_len = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
    let footer_start = tail_offset.checked_sub(footer_len).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: declared footer length ({footer_len} bytes) exceeds file size ({file_len} bytes)",
                path.display()
            ),
        )
    })?;
    let footer = read_range(&mut file, footer_start, tail_offset)?;
    Ok((file, footer, file_len))
}

fn from_dir_inner(dir: &Path, include_page_index: bool) -> io::Result<Vec<Fixture>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", dir.display())))?
    {
        let path = entry?.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "parquet") {
            paths.push(path);
        }
    }
    paths.sort();
    let mut fixtures = Vec::new();
    for path in paths {
        let stem = path
            .file_stem()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{}: missing file stem", path.display()),
                )
            })?
            .to_string_lossy()
            .into_owned();
        let (mut file, footer, file_len) = read_parquet_footer(&path)?;

        let index = if include_page_index {
            page_index_range(&footer)
        } else {
            None
        };
        fixtures.push(Fixture {
            name: stem.clone(),
            contents: Contents::Footer,
            footers: vec![footer],
            file_sizes: vec![file_len],
        });
        if let Some((start, end)) = index {
            fixtures.push(Fixture {
                name: format!("{stem}_page_index"),
                contents: Contents::PageIndex,
                footers: vec![read_range(&mut file, start, end)?],
                file_sizes: vec![file_len],
            });
        }
    }
    Ok(fixtures)
}

fn read_range(file: &mut File, start: u64, end: u64) -> io::Result<Vec<u8>> {
    let len = end.checked_sub(start).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid byte range {start}..{end}"),
        )
    })?;
    let len = usize::try_from(len).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("byte range length {len} exceeds platform usize: {e}"),
        )
    })?;
    let mut bytes = vec![0; len];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// The byte range holding every column index and offset index, found from the
/// footer with arrow-rs (skipping statistics). Returns `None` if the file has no
/// page indexes, any index length is non-positive, or the indexes are not stored as
/// one non-empty contiguous run (for example, when written per row group or separated
/// by bloom filters).
pub(crate) fn page_index_range(footer: &[u8]) -> Option<(u64, u64)> {
    let options = ParquetMetaDataOptions::new()
        .with_column_stats_policy(ParquetStatisticsPolicy::SkipAll)
        .with_encoding_stats_policy(ParquetStatisticsPolicy::SkipAll)
        .with_size_stats_policy(ParquetStatisticsPolicy::SkipAll);
    let metadata =
        ParquetMetaDataReader::decode_metadata_with_options(footer, Some(&options)).ok()?;
    let mut ranges = Vec::new();
    for row_group in metadata.row_groups() {
        for column in row_group.columns() {
            for (offset, len) in [
                (column.column_index_offset(), column.column_index_length()),
                (column.offset_index_offset(), column.offset_index_length()),
            ] {
                if let (Some(offset), Some(len)) = (offset, len) {
                    if len <= 0 {
                        return None;
                    }
                    let start = u64::try_from(offset).ok()?;
                    let end = start.checked_add(u64::try_from(len).ok()?)?;
                    ranges.push((start, end));
                }
            }
        }
    }
    ranges.sort_unstable();
    let &(start, mut end) = ranges.first()?;
    for &(next_start, next_end) in &ranges[1..] {
        if next_start != end {
            return None;
        }
        end = next_end;
    }
    (end > start).then_some((start, end))
}

/// Returns the Thrift-encoded footer of an unencrypted Parquet file.
#[must_use]
pub fn footer(file: &[u8]) -> Option<&[u8]> {
    let (rest, magic) = file.split_last_chunk::<4>()?;
    if magic != b"PAR1" {
        return None;
    }
    let (rest, len) = rest.split_last_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    rest.get(rest.len().checked_sub(len)?..)
}

/// Wraps a raw Thrift footer as the tail of an unencrypted Parquet file:
/// `[footer | u32 LE length | b"PAR1"]`.
///
/// # Panics
///
/// Panics if `footer.len()` exceeds `u32::MAX`.
#[must_use]
pub fn footer_tail(footer: &[u8]) -> Vec<u8> {
    let len = u32::try_from(footer.len())
        .unwrap_or_else(|_| panic!("footer length {} exceeds u32::MAX", footer.len()));
    let mut tail = Vec::with_capacity(footer.len() + 8);
    tail.extend_from_slice(footer);
    tail.extend_from_slice(&len.to_le_bytes());
    tail.extend_from_slice(b"PAR1");
    tail
}

/// Footers of the unencrypted files in parquet-testing's `data` directory that
/// arrow-rs can decode, so every workload covers the same bytes. (A few test files
/// carry deliberately invalid metadata that arrow-rs rejects.)
///
/// # Panics
///
/// Panics if `testdata_dir/parquet-testing/data` cannot be read.
#[must_use]
pub fn parquet_testing(testdata_dir: &Path) -> Vec<Vec<u8>> {
    let dir = testdata_dir.join("parquet-testing/data");
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e} (run `git submodule update --init`)", dir.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
                .path()
        })
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "parquet"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|path| {
            let (_, footer, _) = read_parquet_footer(path).ok()?;
            ParquetMetaDataReader::decode_metadata(&footer)
                .is_ok()
                .then_some(footer)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use parquet::file::metadata::ParquetMetaDataWriter;

    use super::*;
    use crate::{TempDir, corpus};

    #[test]
    fn footer_tail_round_trip() {
        for raw in [
            b"".as_slice(),
            b"short footer".as_slice(),
            footer(&corpus::uniform_file(2, 1)).unwrap(),
        ] {
            let tail = footer_tail(raw);
            assert_eq!(tail.len(), raw.len() + 8);
            assert_eq!(&tail[tail.len() - 4..], b"PAR1");
            assert_eq!(footer(&tail), Some(raw));
        }
    }

    #[test]
    fn from_dir_uses_corpus_compatible_names_and_skips_parquet_dirs() {
        let dir = TempDir::new("names");
        let parquet_bytes = corpus::uniform_file(2, 1);
        fs::write(dir.0.join("sample.parquet"), &parquet_bytes).unwrap();
        // Non-parquet files and Spark-style `*.parquet/` directories should be ignored.
        fs::write(dir.0.join("readme.txt"), b"ignore me").unwrap();
        fs::create_dir(dir.0.join("dataset.parquet")).unwrap();

        let all = from_dir(&dir.0).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "sample");
        assert_eq!(all[0].contents, Contents::Footer);
        assert_eq!(all[0].footers[0], footer(&parquet_bytes).unwrap());
        assert_eq!(all[0].file_sizes, vec![parquet_bytes.len() as u64]);

        assert_eq!(all[1].name, "sample_page_index");
        assert_eq!(all[1].contents, Contents::PageIndex);
        assert_ne!(all[1].footers[0], b"");

        let footers_only = footers_from_dir(&dir.0).unwrap();
        assert_eq!(footers_only.len(), 1);
        assert_eq!(footers_only[0].name, "sample");
        assert_eq!(footers_only[0].contents, Contents::Footer);
    }

    #[test]
    fn from_dir_errors_on_tiny_file() {
        for (label, bytes) in [("empty", b"".as_slice()), ("short", b"PAR1".as_slice())] {
            let dir = TempDir::new(label);
            fs::write(dir.0.join("tiny.parquet"), bytes).unwrap();
            let err = from_dir(&dir.0).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
            assert!(
                err.to_string().contains("too small"),
                "unexpected error message: {err}"
            );
        }
    }

    #[test]
    fn from_dir_errors_on_oversized_footer_length() {
        let dir = TempDir::new("oversized");
        let mut bytes = vec![0_u8; 4];
        bytes.extend_from_slice(&255_u32.to_le_bytes());
        bytes.extend_from_slice(b"PAR1");
        fs::write(dir.0.join("bad_len.parquet"), &bytes).unwrap();

        let err = from_dir(&dir.0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("exceeds file size"),
            "unexpected error message: {err}"
        );
    }

    #[test]
    fn from_dir_errors_on_bad_magic() {
        let dir = TempDir::new("bad_magic");
        fs::write(dir.0.join("bad.parquet"), b"0123456789AB").unwrap();

        let err = from_dir(&dir.0).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("not an unencrypted Parquet file"),
            "unexpected error message: {err}"
        );
    }

    fn rewrite_page_index_offsets(
        col_offset: i64,
        col_len: i32,
        off_offset: i64,
        off_len: i32,
    ) -> Vec<u8> {
        let base_file = corpus::uniform_file(1, 1);
        let base_footer = footer(&base_file).unwrap();
        let meta = ParquetMetaDataReader::decode_metadata(base_footer).unwrap();
        let mut builder = meta.into_builder();
        let row_groups = builder.take_row_groups();
        let rewritten_rgs = row_groups
            .into_iter()
            .map(|rg| {
                let mut rg_builder = rg.into_builder();
                let cols = rg_builder.take_columns();
                let rewritten_cols = cols
                    .into_iter()
                    .map(|col| {
                        col.into_builder()
                            .set_column_index_offset(Some(col_offset))
                            .set_column_index_length(Some(col_len))
                            .set_offset_index_offset(Some(off_offset))
                            .set_offset_index_length(Some(off_len))
                            .build()
                            .unwrap()
                    })
                    .collect();
                rg_builder
                    .set_column_metadata(rewritten_cols)
                    .build()
                    .unwrap()
            })
            .collect();
        let updated = builder.set_row_groups(rewritten_rgs).build();
        let mut out = Vec::new();
        ParquetMetaDataWriter::new(&mut out, &updated)
            .finish()
            .unwrap();
        out
    }

    #[test]
    fn page_index_range_rejects_non_contiguous_and_empty_ranges() {
        let non_contiguous_file = rewrite_page_index_offsets(100, 10, 200, 10);
        let raw_footer = footer(&non_contiguous_file).unwrap();
        assert_eq!(page_index_range(raw_footer), None);

        // Zero-length index ranges are also rejected.
        let zero_len_file = rewrite_page_index_offsets(100, 0, 100, 0);
        assert_eq!(page_index_range(footer(&zero_len_file).unwrap()), None);

        // Also verify from_dir omits the page_index fixture cleanly without panicking.
        let dir = TempDir::new("non_contiguous");
        fs::write(dir.0.join("split.parquet"), &non_contiguous_file).unwrap();
        let fixtures = from_dir(&dir.0).unwrap();
        assert_eq!(fixtures.len(), 1);
        assert_eq!(fixtures[0].name, "split");
        assert_eq!(fixtures[0].contents, Contents::Footer);

        // Sanity check: contiguous ranges still return Some.
        let contiguous_file = rewrite_page_index_offsets(100, 10, 110, 15);
        let contiguous_footer = footer(&contiguous_file).unwrap();
        assert_eq!(page_index_range(contiguous_footer), Some((100, 125)));
    }
}
