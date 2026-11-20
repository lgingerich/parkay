// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

// Copied from the arrow-rs `parquet` crate, version 60.1.0, file
// `parquet/src/parquet_thrift.rs`: the Thrift compact input protocol and its slice
// reader, for a like-for-like benchmark against parkay's reader. Unchanged except:
// - only the input side and the slice reader are kept;
// - `pub(crate)` items are `pub`, so the benchmark crate can call them;
// - the I/O and UTF-8 error variants, and the methods that need `ParquetError`
//   (`read_bytes_owned`, `read_string`, `skip_empty_struct`), are removed.

#![allow(clippy::all, missing_docs, dead_code)]

use std::num::TryFromIntError;

#[derive(Debug)]
pub enum ThriftProtocolError {
    Eof,
    InvalidFieldType(u8),
    InvalidElementType(u8),
    FieldDeltaOverflow { field_delta: u8, last_field_id: i16 },
    InvalidBoolean(u8),
    IntegerOverflow,
    SkipDepth(FieldType),
    SkipUnsupportedType(FieldType),
}

impl From<TryFromIntError> for ThriftProtocolError {
    fn from(_: TryFromIntError) -> Self {
        // ignore error payload to reduce the size of ThriftProtocolError
        Self::IntegerOverflow
    }
}

pub type ThriftProtocolResult<T> = Result<T, ThriftProtocolError>;

// Thrift compact protocol types for struct fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldType {
    Stop = 0,
    BooleanTrue = 1,
    BooleanFalse = 2,
    Byte = 3,
    I16 = 4,
    I32 = 5,
    I64 = 6,
    Double = 7,
    Binary = 8,
    List = 9,
    Set = 10,
    Map = 11,
    Struct = 12,
    Uuid = 13,
}

impl TryFrom<u8> for FieldType {
    type Error = ThriftProtocolError;
    fn try_from(value: u8) -> ThriftProtocolResult<Self> {
        match value {
            0 => Ok(Self::Stop),
            1 => Ok(Self::BooleanTrue),
            2 => Ok(Self::BooleanFalse),
            3 => Ok(Self::Byte),
            4 => Ok(Self::I16),
            5 => Ok(Self::I32),
            6 => Ok(Self::I64),
            7 => Ok(Self::Double),
            8 => Ok(Self::Binary),
            9 => Ok(Self::List),
            10 => Ok(Self::Set),
            11 => Ok(Self::Map),
            12 => Ok(Self::Struct),
            13 => Ok(Self::Uuid),
            _ => Err(ThriftProtocolError::InvalidFieldType(value)),
        }
    }
}

impl From<ElementType> for FieldType {
    fn from(value: ElementType) -> Self {
        match value {
            ElementType::Bool => Self::BooleanTrue,
            ElementType::Byte => Self::Byte,
            ElementType::I16 => Self::I16,
            ElementType::I32 => Self::I32,
            ElementType::I64 => Self::I64,
            ElementType::Double => Self::Double,
            ElementType::Binary => Self::Binary,
            ElementType::List => Self::List,
            ElementType::Set => Self::Set,
            ElementType::Map => Self::Map,
            ElementType::Struct => Self::Struct,
            ElementType::Uuid => Self::Uuid,
        }
    }
}

// Thrift compact protocol types for list elements
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElementType {
    Bool = 2,
    Byte = 3,
    I16 = 4,
    I32 = 5,
    I64 = 6,
    Double = 7,
    Binary = 8,
    List = 9,
    Set = 10,
    Map = 11,
    Struct = 12,
    Uuid = 13,
}

impl TryFrom<u8> for ElementType {
    type Error = ThriftProtocolError;
    fn try_from(value: u8) -> ThriftProtocolResult<Self> {
        match value {
            // For historical and compatibility reasons, a reader should be capable to deal with both cases.
            // The only valid value in the original spec was 2, but due to an widespread implementation bug
            // the defacto standard across large parts of the library became 1 instead.
            // As a result, both values are now allowed.
            // https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md#list-and-set
            1 | 2 => Ok(Self::Bool),
            3 => Ok(Self::Byte),
            4 => Ok(Self::I16),
            5 => Ok(Self::I32),
            6 => Ok(Self::I64),
            7 => Ok(Self::Double),
            8 => Ok(Self::Binary),
            9 => Ok(Self::List),
            10 => Ok(Self::Set),
            11 => Ok(Self::Map),
            12 => Ok(Self::Struct),
            13 => Ok(Self::Uuid),
            _ => Err(ThriftProtocolError::InvalidElementType(value)),
        }
    }
}

/// Struct used to describe a [thrift struct] field during decoding.
///
/// [thrift struct]: https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md#struct-encoding
pub struct FieldIdentifier {
    /// The type for the field.
    pub field_type: FieldType,
    /// The field's `id`. May be computed from delta or directly decoded.
    pub id: i16,
}

impl FieldIdentifier {
    pub fn bool_val(&self) -> ThriftProtocolResult<bool> {
        match self.field_type {
            FieldType::BooleanTrue => Ok(true),
            FieldType::BooleanFalse => Ok(false),
            _ => Err(ThriftProtocolError::InvalidFieldType(self.field_type as u8)),
        }
    }
}

/// Struct used to describe a [thrift list].
///
/// [thrift list]: https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md#list-and-set
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListIdentifier {
    /// The type for each element in the list.
    pub element_type: ElementType,
    /// Number of elements contained in the list.
    pub size: i32,
}

/// Low-level object used to deserialize structs encoded with the Thrift [compact] protocol.
///
/// Implementation of this trait must provide the low-level functions `read_byte`, `read_bytes`,
/// `skip_bytes`, and `read_double`. These primitives are used by the default functions provided
/// here to perform deserialization.
///
/// [compact]: https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md
pub trait ThriftCompactInputProtocol<'a> {
    /// Read a single byte from the input.
    fn read_byte(&mut self) -> ThriftProtocolResult<u8>;

    /// Read a Thrift encoded [binary] from the input.
    ///
    /// [binary]: https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md#binary-encoding
    fn read_bytes(&mut self) -> ThriftProtocolResult<&'a [u8]>;

    /// Skip the next `n` bytes of input.
    fn skip_bytes(&mut self, n: usize) -> ThriftProtocolResult<()>;

    /// Remaining unread bytes, if this protocol is backed by a finite buffer.
    ///
    /// Used to reject Thrift collection sizes that cannot fit in the remaining
    /// input before allocating.
    fn remaining_bytes(&self) -> Option<usize> {
        None
    }

    /// Read a ULEB128 encoded unsigned varint from the input.
    fn read_vlq(&mut self) -> ThriftProtocolResult<u64> {
        // try the happy path first
        let byte = self.read_byte()?;
        if byte & 0x80 == 0 {
            return Ok(byte as u64);
        }
        let mut in_progress = (byte & 0x7f) as u64;
        let mut shift = 7;
        loop {
            let byte = self.read_byte()?;
            in_progress |= ((byte & 0x7F) as u64).wrapping_shl(shift);
            if byte & 0x80 == 0 {
                return Ok(in_progress);
            }
            shift += 7;
        }
    }

    /// Read a zig-zag encoded signed varint from the input.
    fn read_zig_zag(&mut self) -> ThriftProtocolResult<i64> {
        let val = self.read_vlq()?;
        Ok((val >> 1) as i64 ^ -((val & 1) as i64))
    }

    /// Read the [`ListIdentifier`] for a Thrift encoded list.
    fn read_list_begin(&mut self) -> ThriftProtocolResult<ListIdentifier> {
        let header = self.read_byte()?;
        // some parquet writers will have an element_type of 0 for an empty list.
        // account for that and return a bogus but valid element_type.
        if header == 0 {
            return Ok(ListIdentifier {
                element_type: ElementType::Byte,
                size: 0,
            });
        }
        let element_type = ElementType::try_from(header & 0x0f)?;

        let possible_element_count = (header & 0xF0) >> 4;
        let element_count = if possible_element_count != 15 {
            // high bits set high if count and type encoded separately
            possible_element_count as i32
        } else {
            // The list size on the wire is an unsigned varint, but we represent
            // it as `i32` (matching Java's `int` and the Thrift schema).
            // A varint that decodes above `i32::MAX` is malformed input — reject
            // it here at the protocol layer rather than letting the cast wrap
            // into a negative size that downstream allocation code has to
            // re-validate.
            i32::try_from(self.read_vlq()?)?
        };

        Ok(ListIdentifier {
            element_type,
            size: element_count,
        })
    }

    // Full field ids are uncommon.
    // Not inlining this method reduces the code size of `read_field_begin`, which then ideally gets
    // inlined everywhere.
    #[cold]
    fn read_full_field_id(&mut self) -> ThriftProtocolResult<i16> {
        self.read_i16()
    }

    /// Read the [`FieldIdentifier`] for a field in a Thrift encoded struct.
    fn read_field_begin(&mut self, last_field_id: i16) -> ThriftProtocolResult<FieldIdentifier> {
        // we can read at least one byte, which is:
        // - the type
        // - the field delta and the type
        let field_type = self.read_byte()?;
        if field_type & 0xf == 0 {
            return Ok(FieldIdentifier {
                field_type: FieldType::Stop,
                id: 0,
            });
        }

        let field_delta = (field_type & 0xf0) >> 4;
        let field_type = FieldType::try_from(field_type & 0xf)?;

        let id = if field_delta != 0 {
            last_field_id.checked_add(field_delta as i16).ok_or(
                ThriftProtocolError::FieldDeltaOverflow {
                    field_delta,
                    last_field_id,
                },
            )?
        } else {
            self.read_full_field_id()?
        };

        Ok(FieldIdentifier { field_type, id })
    }

    /// This is a specialized version of [`Self::read_field_begin`], solely for use in parsing
    /// simple structs. This function assumes that the delta field will always be less than 0xf,
    /// fields will be in order, and no boolean fields will be read.
    /// This also skips validation of the field type.
    ///
    /// Returns a tuple of `(field_type, field_delta)`.
    fn read_field_header(&mut self) -> ThriftProtocolResult<(u8, u8)> {
        let field_type = self.read_byte()?;
        let field_delta = (field_type & 0xf0) >> 4;
        let field_type = field_type & 0xf;
        Ok((field_type, field_delta))
    }

    /// Read a boolean list element. This should not be used for struct fields. For the latter,
    /// use the [`FieldIdentifier::bool_val`] field.
    fn read_bool(&mut self) -> ThriftProtocolResult<bool> {
        let b = self.read_byte()?;
        // Previous versions of the thrift specification said to use 0 and 1 inside collections,
        // but that differed from existing implementations.
        // The specification was updated in https://github.com/apache/thrift/commit/2c29c5665bc442e703480bb0ee60fe925ffe02e8.
        // At least the go implementation seems to have followed the previously documented values.
        match b {
            0x01 => Ok(true),
            0x00 | 0x02 => Ok(false),
            _ => Err(ThriftProtocolError::InvalidBoolean(b)),
        }
    }

    /// Read an `i8`.
    fn read_i8(&mut self) -> ThriftProtocolResult<i8> {
        Ok(self.read_byte()? as _)
    }

    /// Read an `i16`.
    fn read_i16(&mut self) -> ThriftProtocolResult<i16> {
        Ok(self.read_zig_zag()? as _)
    }

    /// Read an `i32`.
    fn read_i32(&mut self) -> ThriftProtocolResult<i32> {
        Ok(self.read_zig_zag()? as _)
    }

    /// Read an `i64`.
    fn read_i64(&mut self) -> ThriftProtocolResult<i64> {
        self.read_zig_zag()
    }

    /// Read a Thrift `double` as `f64`.
    fn read_double(&mut self) -> ThriftProtocolResult<f64>;

    /// Skip a ULEB128 encoded varint.
    fn skip_vlq(&mut self) -> ThriftProtocolResult<()> {
        loop {
            let byte = self.read_byte()?;
            if byte & 0x80 == 0 {
                return Ok(());
            }
        }
    }

    /// Skip a thrift [binary].
    ///
    /// [binary]: https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md#binary-encoding
    fn skip_binary(&mut self) -> ThriftProtocolResult<()> {
        let len = self.read_vlq()? as usize;
        self.skip_bytes(len)
    }

    /// Skip a field with type `field_type` recursively until the default
    /// maximum skip depth (currently 64) is reached.
    fn skip(&mut self, field_type: FieldType) -> ThriftProtocolResult<()> {
        const DEFAULT_SKIP_DEPTH: i8 = 64;
        self.skip_till_depth(field_type, DEFAULT_SKIP_DEPTH)
    }

    /// Skip a field with type `field_type` recursively up to `depth` levels.
    fn skip_till_depth(&mut self, field_type: FieldType, depth: i8) -> ThriftProtocolResult<()> {
        if depth == 0 {
            return Err(ThriftProtocolError::SkipDepth(field_type));
        }

        match field_type {
            // boolean field has no data
            FieldType::BooleanFalse | FieldType::BooleanTrue => Ok(()),
            FieldType::Byte => self.read_i8().map(|_| ()),
            FieldType::I16 => self.skip_vlq(),
            FieldType::I32 => self.skip_vlq(),
            FieldType::I64 => self.skip_vlq(),
            FieldType::Double => self.skip_bytes(8),
            FieldType::Binary => self.skip_binary(),
            // see https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md#struct
            FieldType::Struct => {
                loop {
                    // we don't need field id for skipping, so always pass 0 for last id
                    let field_ident = self.read_field_begin(0)?;
                    if field_ident.field_type == FieldType::Stop {
                        break;
                    }
                    self.skip_till_depth(field_ident.field_type, depth - 1)?;
                }
                Ok(())
            }
            // lists and sets are encoded the same
            // see https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md#list-and-set
            FieldType::List | FieldType::Set => {
                let list_ident = self.read_list_begin()?;
                let element_type = FieldType::from(list_ident.element_type);
                for _ in 0..list_ident.size {
                    self.skip_till_depth(element_type, depth - 1)?;
                }
                Ok(())
            }
            // see https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md#map
            FieldType::Map => {
                let size = i32::try_from(self.read_vlq()?)?;
                if size > 0 {
                    let kv = self.read_byte()?;
                    let key_type = FieldType::from(ElementType::try_from(kv >> 4)?);
                    let val_type = FieldType::from(ElementType::try_from(kv & 0xf)?);
                    for _ in 0..size {
                        self.skip_till_depth(key_type, depth - 1)?;
                        self.skip_till_depth(val_type, depth - 1)?;
                    }
                }
                Ok(())
            }
            // see https://github.com/apache/thrift/blob/master/doc/specs/thrift-compact-protocol.md#universal-unique-identifier-encoding
            FieldType::Uuid => self.skip_bytes(16),
            FieldType::Stop => Err(ThriftProtocolError::SkipUnsupportedType(field_type)),
        }
    }
}

/// A high performance Thrift reader that reads from a slice of bytes.
pub struct ThriftSliceInputProtocol<'a> {
    buf: &'a [u8],
}

impl<'a> ThriftSliceInputProtocol<'a> {
    /// Create a new `ThriftSliceInputProtocol` using the bytes in `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Return the current buffer as a slice.
    pub fn as_slice(&self) -> &'a [u8] {
        self.buf
    }
}

impl<'b, 'a: 'b> ThriftCompactInputProtocol<'b> for ThriftSliceInputProtocol<'a> {
    #[inline]
    fn read_byte(&mut self) -> ThriftProtocolResult<u8> {
        let ret = *self.buf.first().ok_or(ThriftProtocolError::Eof)?;
        self.buf = &self.buf[1..];
        Ok(ret)
    }

    fn read_bytes(&mut self) -> ThriftProtocolResult<&'b [u8]> {
        let len = self.read_vlq()? as usize;
        let ret = self.buf.get(..len).ok_or(ThriftProtocolError::Eof)?;
        self.buf = &self.buf[len..];
        Ok(ret)
    }

    #[inline]
    fn skip_bytes(&mut self, n: usize) -> ThriftProtocolResult<()> {
        self.buf.get(..n).ok_or(ThriftProtocolError::Eof)?;
        self.buf = &self.buf[n..];
        Ok(())
    }

    fn read_double(&mut self) -> ThriftProtocolResult<f64> {
        let slice = self.buf.get(..8).ok_or(ThriftProtocolError::Eof)?;
        self.buf = &self.buf[8..];
        match slice.try_into() {
            Ok(slice) => Ok(f64::from_le_bytes(slice)),
            Err(_) => unreachable!(),
        }
    }

    fn remaining_bytes(&self) -> Option<usize> {
        Some(self.buf.len())
    }
}
