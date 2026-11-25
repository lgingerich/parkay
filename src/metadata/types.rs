//! The plain definitions of the Parquet format that the metadata is read into: the
//! physical and logical types of a column, how a field repeats, how a chunk is
//! compressed and encoded, and how its statistics are ordered.
//!
//! Each is named as in `format/parquet.thrift` and holds the codes that file gives it.
//! Beside them are the values the decoders in `wire/` fill in: a chunk's location and
//! statistics, and a key-value pair. Nothing here reads bytes, and nothing here uses
//! another module of `metadata`, so the decoders depend only downward.

/// How the values of a leaf column are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhysicalType {
    /// One bit per value.
    Boolean,
    /// A 32-bit signed integer.
    Int32,
    /// A 64-bit signed integer.
    Int64,
    /// A 96-bit value, used only for legacy timestamps.
    Int96,
    /// A 32-bit IEEE float.
    Float,
    /// A 64-bit IEEE float.
    Double,
    /// A byte string of any length.
    ByteArray,
    /// A byte string whose length the schema fixes.
    FixedLenByteArray,
}

impl PhysicalType {
    /// The type for a `Type` value from `parquet.thrift`.
    pub(super) const fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            0 => Self::Boolean,
            1 => Self::Int32,
            2 => Self::Int64,
            3 => Self::Int96,
            4 => Self::Float,
            5 => Self::Double,
            6 => Self::ByteArray,
            7 => Self::FixedLenByteArray,
            _ => return None,
        })
    }
}

/// Whether a field must be present, may be absent, or may repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Repetition {
    /// Exactly one value.
    Required,
    /// Zero or one value.
    Optional,
    /// Zero or more values.
    Repeated,
}

impl Repetition {
    /// The repetition for a `FieldRepetitionType` value from `parquet.thrift`.
    pub(super) const fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            0 => Self::Required,
            1 => Self::Optional,
            2 => Self::Repeated,
            _ => return None,
        })
    }
}

/// The unit of a time or a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeUnit {
    /// Thousandths of a second.
    Milliseconds,
    /// Millionths of a second.
    Microseconds,
    /// Billionths of a second.
    Nanoseconds,
}

/// What the values of a field mean, beyond how they are stored.
///
/// This is the field's `LogicalType` where the file has one, and otherwise the
/// equivalent of its older `ConvertedType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogicalType {
    /// UTF-8 text.
    String,
    /// A map.
    Map,
    /// A list.
    List,
    /// An enumeration, stored as UTF-8 text.
    Enum,
    /// A decimal number.
    Decimal {
        /// Digits after the decimal point.
        scale: i32,
        /// Total digits.
        precision: i32,
    },
    /// A date, in days since the Unix epoch.
    Date,
    /// A time of day.
    Time {
        /// Whether values are normalized to UTC.
        adjusted_to_utc: bool,
        /// The unit values are in.
        unit: TimeUnit,
    },
    /// A point in time.
    Timestamp {
        /// Whether values are normalized to UTC.
        adjusted_to_utc: bool,
        /// The unit values are in.
        unit: TimeUnit,
    },
    /// An integer narrower than, or of other signedness than, its physical type.
    Integer {
        /// The integer's width in bits.
        bit_width: u8,
        /// Whether the integer is signed.
        signed: bool,
    },
    /// A column that holds only nulls (`UNKNOWN` in the format).
    Unknown,
    /// JSON text.
    Json,
    /// A BSON document.
    Bson,
    /// A UUID.
    Uuid,
    /// A 16-bit IEEE float.
    Float16,
    /// A Variant value. Parkay does not read these yet.
    Variant,
    /// A geometry. Parkay does not read these yet.
    Geometry,
    /// A geography. Parkay does not read these yet.
    Geography,
    /// A reference to a file.
    File,
    /// An interval. Only the older `ConvertedType` has this.
    Interval,
    /// The key-value group of a map. Only the older `ConvertedType` has this.
    MapKeyValue,
    /// A logical type newer than the format version Parkay targets.
    Unrecognized,
}

impl LogicalType {
    /// The logical type for a `LogicalType` union member whose own fields, if it has
    /// any, Parkay does not need.
    pub(super) const fn from_member_id(id: i16) -> Self {
        match id {
            1 => Self::String,
            2 => Self::Map,
            3 => Self::List,
            4 => Self::Enum,
            6 => Self::Date,
            11 => Self::Unknown,
            12 => Self::Json,
            13 => Self::Bson,
            14 => Self::Uuid,
            15 => Self::Float16,
            16 => Self::Variant,
            17 => Self::Geometry,
            18 => Self::Geography,
            19 => Self::File,
            _ => Self::Unrecognized,
        }
    }

    /// The logical type equivalent to a `ConvertedType` value from `parquet.thrift`.
    ///
    /// A decimal is returned with zero scale and precision; the caller fills them in
    /// from the schema element.
    pub(super) const fn from_converted_code(code: i32) -> Option<Self> {
        const fn integer(bit_width: u8, signed: bool) -> LogicalType {
            LogicalType::Integer { bit_width, signed }
        }
        // The format says the older time types are normalized to UTC.
        const fn time(unit: TimeUnit) -> LogicalType {
            LogicalType::Time {
                adjusted_to_utc: true,
                unit,
            }
        }
        const fn timestamp(unit: TimeUnit) -> LogicalType {
            LogicalType::Timestamp {
                adjusted_to_utc: true,
                unit,
            }
        }
        Some(match code {
            0 => Self::String,
            1 => Self::Map,
            2 => Self::MapKeyValue,
            3 => Self::List,
            4 => Self::Enum,
            5 => Self::Decimal {
                scale: 0,
                precision: 0,
            },
            6 => Self::Date,
            7 => time(TimeUnit::Milliseconds),
            8 => time(TimeUnit::Microseconds),
            9 => timestamp(TimeUnit::Milliseconds),
            10 => timestamp(TimeUnit::Microseconds),
            11 => integer(8, false),
            12 => integer(16, false),
            13 => integer(32, false),
            14 => integer(64, false),
            15 => integer(8, true),
            16 => integer(16, true),
            17 => integer(32, true),
            18 => integer(64, true),
            19 => Self::Json,
            20 => Self::Bson,
            21 => Self::Interval,
            _ => return None,
        })
    }
}

/// How a column chunk's pages are compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompressionCodec {
    /// Not compressed.
    Uncompressed,
    /// Snappy.
    Snappy,
    /// Gzip.
    Gzip,
    /// LZO.
    Lzo,
    /// Brotli.
    Brotli,
    /// LZ4 in the older Hadoop framing, deprecated in the format.
    Lz4,
    /// Zstandard.
    Zstd,
    /// LZ4 as a raw block.
    Lz4Raw,
}

impl CompressionCodec {
    /// The codec for a `CompressionCodec` value from `parquet.thrift`.
    pub(super) const fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            0 => Self::Uncompressed,
            1 => Self::Snappy,
            2 => Self::Gzip,
            3 => Self::Lzo,
            4 => Self::Brotli,
            5 => Self::Lz4,
            6 => Self::Zstd,
            7 => Self::Lz4Raw,
            _ => return None,
        })
    }
}

/// How values, levels, or dictionary indices are encoded in a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    /// Values stored plainly.
    Plain,
    /// The older name for dictionary encoding.
    PlainDictionary,
    /// The run-length and bit-packed hybrid.
    Rle,
    /// Bit-packed levels, deprecated in the format.
    BitPacked,
    /// Delta encoding for integers.
    DeltaBinaryPacked,
    /// Byte arrays with their lengths delta-encoded.
    DeltaLengthByteArray,
    /// Byte arrays stored as shared prefixes and suffixes.
    DeltaByteArray,
    /// Dictionary indices in the run-length and bit-packed hybrid.
    RleDictionary,
    /// The bytes of fixed-width values split into streams.
    ByteStreamSplit,
    /// Adaptive lossless floating-point encoding.
    Alp,
}

impl Encoding {
    /// The value this encoding has in `parquet.thrift`, which is also its bit in an
    /// [`Encodings`] set.
    const fn code(self) -> u8 {
        match self {
            Self::Plain => 0,
            Self::PlainDictionary => 2,
            Self::Rle => 3,
            Self::BitPacked => 4,
            Self::DeltaBinaryPacked => 5,
            Self::DeltaLengthByteArray => 6,
            Self::DeltaByteArray => 7,
            Self::RleDictionary => 8,
            Self::ByteStreamSplit => 9,
            Self::Alp => 10,
        }
    }
}

/// The set of bits an [`Encodings`] may hold: one for each encoding the format defines.
pub(super) const ENCODINGS_ALL: u16 = 0b111_1111_1101;

/// A set of encodings: those a column chunk uses, or those its data pages use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Encodings(pub(super) u16);

impl Encodings {
    /// Whether `encoding` is in the set.
    #[must_use]
    pub const fn contains(self, encoding: Encoding) -> bool {
        self.0 & (1 << encoding.code()) != 0
    }
}

/// How the minimum and maximum of a column's statistics are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnOrder {
    /// The order defined by the column's logical type, or by its physical type when it
    /// has no logical type.
    TypeDefined,
    /// IEEE 754 total order, for floating-point columns.
    Ieee754Total,
    /// Chronological order, for `INT96` timestamp columns.
    Int96Timestamp,
    /// An order newer than the format version Parkay targets. The format says a reader
    /// must then ignore the column's minimum and maximum.
    Unrecognized,
}

/// Where a column chunk's bytes are in the file, and how they are encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub(super) data_page_offset: u64,
    pub(super) dictionary_page_offset: u64,
    pub(super) compressed_size: u64,
    pub(super) uncompressed_size: u64,
    pub(super) value_count: u64,
    pub(super) encodings: Encodings,
    pub(super) data_page_encodings: Encodings,
    pub(super) codec: CompressionCodec,
    pub(super) has_dictionary_page_offset: bool,
    pub(super) has_data_page_encodings: bool,
}

impl Location {
    pub(super) const EMPTY: Self = Self {
        data_page_offset: 0,
        dictionary_page_offset: 0,
        compressed_size: 0,
        uncompressed_size: 0,
        value_count: 0,
        encodings: Encodings(0),
        data_page_encodings: Encodings(0),
        codec: CompressionCodec::Uncompressed,
        has_dictionary_page_offset: false,
        has_data_page_encodings: false,
    };

    /// Where the chunk's bytes start in the file.
    ///
    /// This is the dictionary page when the chunk has one and the first data page
    /// otherwise. Some older writers recorded a dictionary page offset of zero, or one
    /// after the data pages, for a chunk with no dictionary page; both are ignored, as
    /// other readers ignore them.
    #[must_use]
    pub const fn start(&self) -> u64 {
        let dictionary = self.dictionary_page_offset;
        if self.has_dictionary_page_offset && dictionary > 0 && dictionary < self.data_page_offset {
            dictionary
        } else {
            self.data_page_offset
        }
    }

    /// The bytes the chunk's pages take in the file, from [`start`](Self::start).
    #[must_use]
    pub const fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// The bytes the chunk's pages take once decompressed.
    #[must_use]
    pub const fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// Where the first data page starts in the file.
    #[must_use]
    pub const fn data_page_offset(&self) -> u64 {
        self.data_page_offset
    }

    /// Where the dictionary page starts in the file, as the footer records it.
    #[must_use]
    pub const fn dictionary_page_offset(&self) -> Option<u64> {
        if self.has_dictionary_page_offset {
            Some(self.dictionary_page_offset)
        } else {
            None
        }
    }

    /// The number of values in the chunk, nulls included.
    #[must_use]
    pub const fn value_count(&self) -> u64 {
        self.value_count
    }

    /// How the chunk's pages are compressed.
    #[must_use]
    pub const fn codec(&self) -> CompressionCodec {
        self.codec
    }

    /// The encodings the chunk uses.
    #[must_use]
    pub const fn encodings(&self) -> Encodings {
        self.encodings
    }

    /// The encodings of the chunk's data pages alone, from the footer's
    /// `encoding_stats`. A chunk whose data pages are all dictionary-encoded has only
    /// [`Encoding::RleDictionary`] or [`Encoding::PlainDictionary`] here, which
    /// [`encodings`](Self::encodings) cannot show: it also covers the dictionary page
    /// and the levels.
    ///
    /// `None` if the request did not ask for them with
    /// [`Request::with_data_page_encodings`](super::Request::with_data_page_encodings),
    /// or if the footer does not record them, as footers of older writers do not.
    #[must_use]
    pub const fn data_page_encodings(&self) -> Option<Encodings> {
        if self.has_data_page_encodings {
            Some(self.data_page_encodings)
        } else {
            None
        }
    }
}

/// A run of bytes in the footer, by offset and size.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Bytes {
    pub(super) offset: u32,
    pub(super) size: u32,
}

/// A column chunk's statistics.
///
/// The minimum and maximum are kept as positions in the footer, never copied, and are
/// read back through the [`Metadata`](super::Metadata) they came from. Their bytes are
/// the plain encoding of the column's physical type, without a length prefix for byte
/// arrays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    pub(super) min: Bytes,
    pub(super) max: Bytes,
    pub(super) null_count: u64,
    pub(super) distinct_count: u64,
    pub(super) nan_count: u64,
    pub(super) flags: u16,
}

// The minimum and maximum are read through the `Metadata` they came from, so their
// accessors are in `extract.rs`. Keeping them there leaves this file below every
// module that decodes or holds a footer.
impl Statistics {
    pub(super) const EMPTY: Self = Self {
        min: Bytes { offset: 0, size: 0 },
        max: Bytes { offset: 0, size: 0 },
        null_count: 0,
        distinct_count: 0,
        nan_count: 0,
        flags: 0,
    };

    pub(super) const HAS_MIN: u16 = 1 << 0;
    pub(super) const HAS_MAX: u16 = 1 << 1;
    pub(super) const BOUNDS_DEPRECATED: u16 = 1 << 2;
    pub(super) const HAS_MIN_EXACT: u16 = 1 << 3;
    pub(super) const MIN_EXACT: u16 = 1 << 4;
    pub(super) const HAS_MAX_EXACT: u16 = 1 << 5;
    pub(super) const MAX_EXACT: u16 = 1 << 6;
    pub(super) const HAS_NULL_COUNT: u16 = 1 << 7;
    pub(super) const HAS_DISTINCT_COUNT: u16 = 1 << 8;
    pub(super) const HAS_NAN_COUNT: u16 = 1 << 9;

    pub(super) const fn has(&self, flag: u16) -> bool {
        self.flags & flag != 0
    }

    /// The number of null values, where the footer records it. A missing count does not
    /// mean there are none.
    #[must_use]
    pub const fn null_count(&self) -> Option<u64> {
        if self.has(Self::HAS_NULL_COUNT) {
            Some(self.null_count)
        } else {
            None
        }
    }

    /// The number of distinct values, where the footer records it.
    #[must_use]
    pub const fn distinct_count(&self) -> Option<u64> {
        if self.has(Self::HAS_DISTINCT_COUNT) {
            Some(self.distinct_count)
        } else {
            None
        }
    }

    /// The number of NaN values, where the footer records it. A missing count means
    /// there may be some.
    #[must_use]
    pub const fn nan_count(&self) -> Option<u64> {
        if self.has(Self::HAS_NAN_COUNT) {
            Some(self.nan_count)
        } else {
            None
        }
    }

    /// Whether the minimum and maximum come from the footer's older `min` and `max`
    /// fields. Those were compared as signed values whatever the column's type, so
    /// they can be trusted only for types where that is the right order.
    #[must_use]
    pub const fn bounds_are_deprecated(&self) -> bool {
        self.has(Self::BOUNDS_DEPRECATED)
    }

    /// Whether the minimum is a value in the chunk, and not a shorter lower bound.
    /// `None` when the footer does not say.
    #[must_use]
    pub const fn min_is_exact(&self) -> Option<bool> {
        if self.has(Self::HAS_MIN_EXACT) {
            Some(self.has(Self::MIN_EXACT))
        } else {
            None
        }
    }

    /// Whether the maximum is a value in the chunk, and not a shorter upper bound.
    /// `None` when the footer does not say.
    #[must_use]
    pub const fn max_is_exact(&self) -> Option<bool> {
        if self.has(Self::HAS_MAX_EXACT) {
            Some(self.has(Self::MAX_EXACT))
        } else {
            None
        }
    }
}

/// One key-value pair of a file's metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyValue<'a> {
    pub(super) key: &'a str,
    pub(super) value: Option<&'a str>,
}

impl<'a> KeyValue<'a> {
    /// The key.
    #[must_use]
    pub const fn key(&self) -> &'a str {
        self.key
    }

    /// The value, where the pair has one.
    #[must_use]
    pub const fn value(&self) -> Option<&'a str> {
        self.value
    }
}
