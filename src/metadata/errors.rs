//! Errors from opening a file and reading its metadata.
//!
//! They are layered. [`FooterError`] names what is wrong inside the footer, by byte
//! offset within the footer, and is small because the decoders that return it run once
//! per column chunk. [`OpenError`] and [`ExtractError`] are what the public functions
//! return: they add where the footer is in the file, or which column chunk failed.

use std::fmt;

use thiserror::Error;

use crate::thrift::ThriftError;

/// The footer structure an error concerns, named as in `parquet.thrift`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Structure {
    /// The top-level `FileMetaData`.
    FileMetaData,
    /// One `SchemaElement` of the schema.
    SchemaElement,
    /// A `LogicalType` annotation, or one of the structures inside it.
    LogicalType,
    /// One `RowGroup`.
    RowGroup,
    /// One `ColumnChunk`.
    ColumnChunk,
    /// The `ColumnMetaData` of a column chunk.
    ColumnMetaData,
    /// The `Statistics` of a column chunk.
    Statistics,
    /// One `ColumnOrder`.
    ColumnOrder,
    /// One `KeyValue` pair.
    KeyValue,
}

impl fmt::Display for Structure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::FileMetaData => "FileMetaData",
            Self::SchemaElement => "SchemaElement",
            Self::LogicalType => "LogicalType",
            Self::RowGroup => "RowGroup",
            Self::ColumnChunk => "ColumnChunk",
            Self::ColumnMetaData => "ColumnMetaData",
            Self::Statistics => "Statistics",
            Self::ColumnOrder => "ColumnOrder",
            Self::KeyValue => "KeyValue",
        })
    }
}

/// Something a file uses that Parkay does not support yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// Encrypted columns, signalled by the footer or by a column chunk.
    Encryption,
    /// A column chunk whose data is in another file.
    ExternalFile,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Encryption => "encryption",
            Self::ExternalFile => "a column chunk stored in another file",
        })
    }
}

/// Why a schema's list of elements does not form one tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaProblem {
    /// The schema has no elements, so it has no root.
    Empty,
    /// An element follows the end of the root's subtree.
    ExtraRoot,
    /// The list ends before a group has all the children it declares.
    ChildrenMissing,
    /// A field other than the root has no repetition.
    RepetitionMissing,
    /// A fixed-length byte array column has no length.
    LengthMissing,
}

impl fmt::Display for SchemaProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "it has no elements",
            Self::ExtraRoot => "an element follows the end of the root's subtree",
            Self::ChildrenMissing => "it ends before a group has all its children",
            Self::RepetitionMissing => "a field has no repetition",
            Self::LengthMissing => "a fixed-length column has no length",
        })
    }
}

/// What is wrong inside a footer.
///
/// Every offset is a byte offset within the footer. Add the footer's position in the
/// file, which [`OpenError::Footer`] and
/// [`Metadata::footer_offset`](super::Metadata::footer_offset) give, for a position in
/// the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FooterError {
    /// The footer is not well-formed Thrift.
    #[error(transparent)]
    Thrift(#[from] ThriftError),
    /// A required field is absent, or is present with a wire type it cannot have.
    #[error("{structure} has no field {field}, which is required")]
    MissingField {
        /// The structure that lacks the field.
        structure: Structure,
        /// The field's id in `parquet.thrift`.
        field: u8,
    },
    /// A field id appears twice in one structure.
    #[error("{structure} repeats field {field} at byte {offset}")]
    DuplicateField {
        /// The structure holding the field.
        structure: Structure,
        /// The field's id in `parquet.thrift`.
        field: u8,
        /// Offset of the second occurrence's value.
        offset: u32,
    },
    /// A field holds a value the format does not allow, such as a negative count or an
    /// enumeration value that does not exist.
    #[error("{structure} field {field} holds an invalid value at byte {offset}")]
    InvalidValue {
        /// The structure holding the field.
        structure: Structure,
        /// The field's id in `parquet.thrift`.
        field: u8,
        /// Offset of the value.
        offset: u32,
    },
    /// A string is not valid UTF-8.
    #[error("{structure} field {field} is not valid UTF-8 at byte {offset}")]
    InvalidUtf8 {
        /// The structure holding the field.
        structure: Structure,
        /// The field's id in `parquet.thrift`.
        field: u8,
        /// Offset of the string's length prefix.
        offset: u32,
    },
    /// The file uses something Parkay does not support.
    #[error("{feature} is not supported (byte {offset})")]
    Unsupported {
        /// What is not supported.
        feature: Unsupported,
        /// Offset of the field that signals it.
        offset: u32,
    },
    /// The schema nests deeper than [`Limits::schema_depth_max`](crate::Limits).
    #[error("the schema nests deeper than the limit of {limit} levels at element {element}")]
    SchemaTooDeep {
        /// Index of the first element past the limit.
        element: u32,
        /// The limit that was exceeded.
        limit: u8,
    },
    /// The schema's elements do not form one tree.
    #[error("the schema is not a tree: {problem} (element {element})")]
    Schema {
        /// What is wrong.
        problem: SchemaProblem,
        /// Index of the element where it was found.
        element: u32,
    },
    /// A row group does not have one column chunk per leaf column of the schema.
    #[error("row group {row_group} has {found} column chunks, not {expected}")]
    ColumnCount {
        /// Index of the row group.
        row_group: u32,
        /// Column chunks the row group lists.
        found: u32,
        /// Column chunks it must list: the schema's leaf columns, or what the first row
        /// group lists.
        expected: u32,
    },
    /// The footer does not list one column order per leaf column of the schema.
    #[error("the footer lists {found} column orders for {expected} columns")]
    ColumnOrderCount {
        /// Column orders the footer lists.
        found: u32,
        /// Leaf columns in the schema.
        expected: u32,
    },
}

// The decoders that return this run once per column chunk. Keep it from growing
// unnoticed, as the Thrift error is kept.
const _: () = assert!(size_of::<FooterError>() == 16);
const _: () = assert!(size_of::<Result<(), FooterError>>() == 16);

/// Why a file could not be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The file is smaller than the eight bytes every Parquet file ends with.
    #[error("a file of {file_size} bytes is too small to be a Parquet file")]
    FileTooSmall {
        /// The file's size as the caller gave it.
        file_size: u64,
    },
    /// The caller supplied more bytes than it says the file has. This is what a file
    /// replaced between two requests looks like.
    #[error("the tail holds {tail_size} bytes, more than the file's {file_size}")]
    TailLongerThanFile {
        /// Bytes in the supplied tail.
        tail_size: u64,
        /// The file's size as the caller gave it.
        file_size: u64,
    },
    /// The file does not end with the Parquet magic bytes.
    #[error("the file does not end with the Parquet magic bytes; it ends with {found:?}")]
    NotParquet {
        /// The file's last four bytes.
        found: [u8; 4],
    },
    /// The footer itself is encrypted, which Parkay does not support yet.
    #[error("the footer is encrypted, which is not supported")]
    EncryptedFooter,
    /// The footer is larger than [`Limits::footer_size_max`](crate::Limits).
    #[error("the footer is {footer_size} bytes, more than the limit of {limit}")]
    FooterTooLarge {
        /// The footer's size as the file states it.
        footer_size: u32,
        /// The limit that was exceeded.
        limit: u32,
    },
    /// The footer's stated size does not fit in the file.
    #[error("a footer of {footer_size} bytes does not fit in a file of {file_size} bytes")]
    FooterLongerThanFile {
        /// The footer's size as the file states it.
        footer_size: u32,
        /// The file's size as the caller gave it.
        file_size: u64,
    },
    /// The footer's contents are wrong.
    #[error("in the footer, which starts at byte {footer_offset} of the file: {source}")]
    Footer {
        /// Where the footer starts in the file.
        footer_offset: u64,
        /// What is wrong, by byte offset within the footer.
        #[source]
        source: FooterError,
    },
}

/// Why column-chunk facts could not be extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ExtractError {
    /// The request names row groups the file does not have.
    #[error("row groups {start}..{end} are not all among the file's {count}")]
    RowGroups {
        /// First row group requested.
        start: u32,
        /// One past the last row group requested.
        end: u32,
        /// Row groups in the file.
        count: u32,
    },
    /// The request names a column the file does not have.
    #[error("column {column} is not among the file's {count}")]
    Column {
        /// The column requested.
        column: u32,
        /// Leaf columns in the file.
        count: u32,
    },
    /// One column chunk's metadata is wrong. Chunks that were not requested are not
    /// looked at, so they cannot cause this.
    #[error("row group {row_group}, column {column}: {source}")]
    ColumnChunk {
        /// Index of the row group in the file.
        row_group: u32,
        /// Index of the leaf column in the file.
        column: u32,
        /// What is wrong, by byte offset within the footer.
        #[source]
        source: FooterError,
    },
}
