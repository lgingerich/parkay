//! Opening a file: from the bytes at its end to its [`Metadata`].

use super::errors::OpenError;
use super::file::Metadata;
use super::schema::Schema;
use super::wire::file_metadata;
use crate::limits::Limits;

/// The bytes every Parquet file ends with: the footer's length, then the magic.
const TRAILER_SIZE: u64 = 8;
/// The magic of a file whose footer is not encrypted.
const MAGIC: [u8; 4] = *b"PAR1";
/// The magic of a file whose footer is encrypted.
const MAGIC_ENCRYPTED_FOOTER: [u8; 4] = *b"PARE";

/// What [`open`] found.
#[derive(Debug)]
pub enum Opened {
    /// The tail held the whole footer.
    File {
        /// The file's metadata.
        metadata: Metadata,
        /// The file's schema. `None` when the footer began with the bytes of the schema
        /// passed to [`open`], which is then this file's schema too.
        schema: Option<Schema>,
    },
    /// The tail does not hold the whole footer. Fetch the bytes of the file from
    /// `start` up to, but not including, `end`, and call [`open`] again with them in
    /// front of the tail. Nothing is kept from this call.
    MissingBytes {
        /// Offset in the file of the first byte needed.
        start: u64,
        /// Offset in the file of the first byte already supplied.
        end: u64,
    },
}

/// Opens a file from the bytes at its end.
///
/// `tail` is any number of bytes from the end of the file, and `file_size` is the
/// size of the whole file, which places the tail and bounds every offset the footer
/// holds. Opening performs no I/O and keeps nothing between calls. It copies the footer
/// out of `tail`, so the caller can reuse the buffer.
///
/// `schema_shared` is the schema of an earlier file from the same dataset. If this
/// file's footer begins with the bytes that schema was decoded from, the schema is not
/// decoded again.
///
/// How long a tail to read is the caller's guess: a footer's length is known only from
/// the last eight bytes of its file. Nothing here fixes that guess.
// TODO(scanner): the layer that performs I/O must take the length of this first read as
// a setting, not a constant, with 512 KiB as its default, which is what DataFusion
// uses. A second round trip for a footer the first read missed costs far more than
// reading some hundreds of kilobytes that turn out not to be needed. That layer reads
// every file's tail into one buffer that it reuses: opening copies the footer out, so
// nothing here holds on to the buffer.
///
/// Opening reads what belongs to the file as a whole and records where each column
/// chunk's metadata begins. It does not decode the column chunks, so a file can open
/// and a column chunk still fail when [`Metadata::extract`] decodes it.
///
/// # Errors
///
/// Returns an error if the tail and size cannot belong to a Parquet file, if the footer
/// exceeds a limit, is encrypted, or is malformed outside its column chunks.
pub fn open(
    tail: &[u8],
    file_size: u64,
    limits: &Limits,
    schema_shared: Option<&Schema>,
) -> Result<Opened, OpenError> {
    let tail_size = size_u64(tail.len());
    if tail_size > file_size {
        return Err(OpenError::TailLongerThanFile {
            tail_size,
            file_size,
        });
    }
    if file_size < TRAILER_SIZE {
        return Err(OpenError::FileTooSmall { file_size });
    }
    let Some((before_trailer, trailer)) = tail.split_last_chunk::<8>() else {
        return Ok(Opened::MissingBytes {
            start: file_size - TRAILER_SIZE,
            end: file_size - tail_size,
        });
    };
    let footer_size = trailer_check(*trailer, file_size, *limits)?;
    let footer_and_trailer_size = u64::from(footer_size) + TRAILER_SIZE;
    let footer_offset = file_size - footer_and_trailer_size;
    let Some(footer_start) = before_trailer.len().checked_sub(footer_size as usize) else {
        return Ok(Opened::MissingBytes {
            start: footer_offset,
            end: file_size - tail_size,
        });
    };
    let footer = &before_trailer[footer_start..];
    match file_metadata::decode(footer, *limits, schema_shared) {
        Ok(mut decoded) => Ok(Opened::File {
            schema: decoded.schema.take(),
            metadata: Metadata::new(footer, decoded, file_size, footer_offset),
        }),
        Err(source) => Err(OpenError::Footer {
            footer_offset,
            source,
        }),
    }
}

/// Checks the last eight bytes of a file and returns the footer's size.
fn trailer_check(trailer: [u8; 8], file_size: u64, limits: Limits) -> Result<u32, OpenError> {
    let (size, magic) = trailer.split_at(4);
    if magic == MAGIC_ENCRYPTED_FOOTER {
        return Err(OpenError::EncryptedFooter);
    }
    if magic != MAGIC {
        return Err(OpenError::NotParquet {
            found: [magic[0], magic[1], magic[2], magic[3]],
        });
    }
    let footer_size = u32::from_le_bytes([size[0], size[1], size[2], size[3]]);
    if footer_size > limits.footer_size_max() {
        return Err(OpenError::FooterTooLarge {
            footer_size,
            limit: limits.footer_size_max(),
        });
    }
    if u64::from(footer_size) + TRAILER_SIZE > file_size {
        return Err(OpenError::FooterLongerThanFile {
            footer_size,
            file_size,
        });
    }
    Ok(footer_size)
}

/// A length in memory as a size in a file. A `usize` is at most 64 bits on every target
/// Rust supports.
fn size_u64(size: usize) -> u64 {
    u64::try_from(size).unwrap_or(u64::MAX)
}
