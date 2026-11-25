//! The schema of a Parquet file: its tree of fields and its leaf columns.
//!
//! The footer stores the schema as a flat list of elements in depth-first order, each
//! saying how many children it has. [`Schema`] keeps that list, with every name in one
//! validated string, and adds what a reader needs from the tree: each node's parent and
//! the definition and repetition levels of each leaf column.

use super::types::{LogicalType, PhysicalType, Repetition};
use super::utils::footer_u32;

/// What the schema keeps for one node of the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NodeData {
    /// Where this node's name ends in [`Schema::names`]. It starts where the previous
    /// node's name ends.
    pub(super) name_end: u32,
    /// Index of the parent node. The root is its own parent.
    pub(super) parent: u32,
    pub(super) child_count: u32,
    pub(super) type_length: Option<u32>,
    pub(super) field_id: Option<i32>,
    pub(super) logical_type: Option<LogicalType>,
    /// Set for leaf columns only.
    pub(super) physical_type: Option<PhysicalType>,
    pub(super) repetition: Option<Repetition>,
    pub(super) definition_level: u8,
    pub(super) repetition_level: u8,
}

/// The schema of a file.
///
/// A schema is its own value, separate from a file's
/// [`Metadata`](super::Metadata), because files in one dataset usually share one.
/// [`open`](super::open) takes a schema from an earlier file and recognizes a footer
/// that begins with the same bytes, without decoding the schema again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// Every node's name, one after another, in the order of `nodes`.
    pub(super) names: String,
    /// The tree in depth-first order. The root is first.
    pub(super) nodes: Vec<NodeData>,
    /// Indices into `nodes` of the leaf columns, in order.
    pub(super) leaves: Vec<u32>,
    /// The leading bytes of the footer this schema was decoded from, up to the end of
    /// the schema. Empty when that footer did not begin with its version and schema, in
    /// which case this schema is never recognized by its bytes.
    pub(super) footer_prefix: Vec<u8>,
    /// The format version stated in those leading bytes.
    pub(super) version: i32,
    /// Deepest nesting level present in `nodes` (0 for root-only, 1 for top-level
    /// columns). Checked against [`Limits::schema_depth_max`](crate::Limits) when this
    /// schema is reused across files.
    pub(super) depth_max: u8,
}

impl Schema {
    /// The number of nodes in the tree, including the root.
    #[must_use]
    pub fn node_count(&self) -> u32 {
        footer_u32(self.nodes.len())
    }

    /// The node at `index` in depth-first order. Index 0 is the root.
    #[must_use]
    pub fn node(&self, index: u32) -> Option<Node<'_>> {
        let in_range = usize::try_from(index).is_ok_and(|index| index < self.nodes.len());
        in_range.then_some(Node {
            schema: self,
            index,
        })
    }

    /// The number of leaf columns. Every row group has one column chunk for each.
    #[must_use]
    pub fn column_count(&self) -> u32 {
        footer_u32(self.leaves.len())
    }

    /// The leaf column at `index`, in the order column chunks are stored.
    #[must_use]
    pub fn column(&self, index: u32) -> Option<Node<'_>> {
        let leaf = *self.leaves.get(usize::try_from(index).ok()?)?;
        self.node(leaf)
    }

    /// The bytes of memory this schema holds.
    #[must_use]
    pub fn memory_size(&self) -> usize {
        size_of::<Self>()
            + self.names.capacity()
            + self.nodes.capacity() * size_of::<NodeData>()
            + self.leaves.capacity() * size_of::<u32>()
            + self.footer_prefix.capacity()
    }

    /// Records the leading bytes of the footer this schema came from, so a later footer
    /// that begins with the same bytes is known to hold the same schema.
    pub(super) fn set_footer_prefix(&mut self, footer_prefix: &[u8], version: i32) {
        debug_assert_eq!(self.footer_prefix.len(), 0);
        self.footer_prefix = footer_prefix.to_vec();
        self.version = version;
    }

    /// The leading footer bytes this schema is recognized by, and the format version
    /// they state. `None` when this schema cannot be recognized by its bytes.
    pub(super) fn footer_prefix(&self) -> Option<(&[u8], i32)> {
        if self.footer_prefix.is_empty() {
            None
        } else {
            Some((&self.footer_prefix, self.version))
        }
    }

    /// Deepest nesting level in this schema.
    pub(super) const fn depth_max(&self) -> u8 {
        self.depth_max
    }

    fn data(&self, index: u32) -> &NodeData {
        // A `Node` is only made for an index within the tree.
        &self.nodes[index as usize]
    }
}

/// One node of a schema: the root, a group, or a leaf column.
#[derive(Debug, Clone, Copy)]
pub struct Node<'a> {
    schema: &'a Schema,
    index: u32,
}

impl<'a> Node<'a> {
    /// This node's index in depth-first order. The root is 0.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// The field's name.
    #[must_use]
    pub fn name(&self) -> &'a str {
        let start = match self.index.checked_sub(1) {
            Some(previous) => self.schema.data(previous).name_end,
            None => 0,
        };
        let end = self.schema.data(self.index).name_end;
        &self.schema.names[start as usize..end as usize]
    }

    /// The names from the top-level field down to this node. Empty for the root.
    #[must_use]
    pub fn path(&self) -> Vec<&'a str> {
        let mut names = Vec::new();
        let mut node = *self;
        while let Some(parent) = node.parent() {
            names.push(node.name());
            node = parent;
        }
        names.reverse();
        names
    }

    /// The node this one is a child of. `None` for the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.index == 0 {
            return None;
        }
        Some(Self {
            schema: self.schema,
            index: self.schema.data(self.index).parent,
        })
    }

    /// The number of children. Zero for a leaf column.
    #[must_use]
    pub fn child_count(&self) -> u32 {
        self.schema.data(self.index).child_count
    }

    /// How values are stored, for a leaf column. `None` for the root and for groups.
    #[must_use]
    pub fn physical_type(&self) -> Option<PhysicalType> {
        self.schema.data(self.index).physical_type
    }

    /// The length of each value, for a fixed-length byte array column.
    #[must_use]
    pub fn type_length(&self) -> Option<u32> {
        self.schema.data(self.index).type_length
    }

    /// Whether the field is required, optional, or repeated. `None` for the root only.
    #[must_use]
    pub fn repetition(&self) -> Option<Repetition> {
        self.schema.data(self.index).repetition
    }

    /// What the field's values mean, where the file says.
    #[must_use]
    pub fn logical_type(&self) -> Option<LogicalType> {
        self.schema.data(self.index).logical_type
    }

    /// The field id a table format assigned to this field, where the file has one.
    #[must_use]
    pub fn field_id(&self) -> Option<i32> {
        self.schema.data(self.index).field_id
    }

    /// The definition level of a value that is present at this node: the number of
    /// optional and repeated fields on the path to it, itself included.
    #[must_use]
    pub fn definition_level_max(&self) -> u8 {
        self.schema.data(self.index).definition_level
    }

    /// The number of repeated fields on the path to this node, itself included.
    #[must_use]
    pub fn repetition_level_max(&self) -> u8 {
        self.schema.data(self.index).repetition_level
    }
}
