//! Limits on what a file may ask Parkay to do.
//!
//! A size or a count read from a file is compared with its limit before it is used to
//! allocate, to ask for bytes, or to bound a loop. Exceeding a limit is an ordinary
//! error, not a panic. A caller reading untrusted files sets lower limits than the
//! defaults.

/// Default for [`Limits::footer_size_max`]: 1 GiB.
///
/// Large external stress footers reach ~240 MB (for example, a 100,000-column table in
/// `alamb/parquet_footer_parsing`). The default leaves about four times that.
pub const FOOTER_SIZE_MAX_DEFAULT: u32 = 1 << 30;

/// Default for [`Limits::schema_depth_max`]: 128 levels.
///
/// Real schemas nest a handful of levels. The default is the depth that common parsers
/// of nested data stop at, and it keeps definition and repetition levels within a byte.
pub const SCHEMA_DEPTH_MAX_DEFAULT: u8 = 128;

/// The limits applied to one file, given when it is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    footer_size_max: u32,
    schema_depth_max: u8,
}

impl Limits {
    /// The default limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            footer_size_max: FOOTER_SIZE_MAX_DEFAULT,
            schema_depth_max: SCHEMA_DEPTH_MAX_DEFAULT,
        }
    }

    /// Sets the largest footer, in bytes, that opening accepts.
    #[must_use]
    pub const fn with_footer_size_max(mut self, footer_size_max: u32) -> Self {
        self.footer_size_max = footer_size_max;
        self
    }

    /// Sets the deepest nesting a schema may have. A top-level column is at depth 1.
    #[must_use]
    pub const fn with_schema_depth_max(mut self, schema_depth_max: u8) -> Self {
        self.schema_depth_max = schema_depth_max;
        self
    }

    /// The largest footer, in bytes, that opening accepts.
    #[must_use]
    pub const fn footer_size_max(&self) -> u32 {
        self.footer_size_max
    }

    /// The deepest nesting a schema may have. A top-level column is at depth 1.
    #[must_use]
    pub const fn schema_depth_max(&self) -> u8 {
        self.schema_depth_max
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}
