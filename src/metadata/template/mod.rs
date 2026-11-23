//! Passing over column chunks by comparing them with earlier ones.
//!
//! Thrift has no length prefixes, so the pass that opens a file finds where a column
//! chunk ends by parsing it, one field after another, each step waiting on the one
//! before. But a column's chunks in successive row groups are usually alike: the same
//! fields in the same order, holding other values.
//!
//! Parsing a chunk reads only some of its bits, and
//! [`skip_marking`](CompactReader::skip_marking) records which. A later chunk of the
//! same column that agrees with an earlier one, its template, on every bit that was
//! read is parsed by the same steps and so has the same length. Comparing the two under
//! that mask finds this out at close to the speed of reading the bytes.
//!
//! Integers and binaries may differ in length without changing the steps: a file
//! offset grows by a byte, or a string minimum is longer. A chunk that differs from
//! its template only in those is found by stepping over them, and the template then
//! remembers which of its lengths vary, so that later chunks are compared around them
//! without being stopped by them.
//!
//! Whatever the comparison cannot settle is parsed as before, so this accepts and
//! rejects exactly what parsing does. Debug builds, the tests, and the fuzz targets
//! parse every chunk that matched and check that it ends where the comparison said.

mod compare;

use self::compare::{Stepped, VARIABLE_COUNT_MAX, Variable, matched_end, stepped_end};
use super::errors::FooterError;
use super::utils::footer_u32;
use crate::thrift::{CompactReader, ElementType, Marks};

/// What the pass keeps for one column.
#[derive(Debug, Clone, Copy)]
struct Template {
    /// Where the template chunk starts in the footer.
    position: u32,
    /// The template chunk's length. Zero when the column has no template.
    size: u32,
    /// Where the template's marks start in the mask.
    mask_start: u32,
    /// Where the template's binaries start in the list of binaries.
    binaries_start: u32,
    /// How many binaries the template has there.
    binaries_count: u32,
    /// Chunks of the column that matched a template.
    hits: u32,
    /// Chunks of the column that were compared with a template and did not match.
    misses: u32,
    /// How many of `variables` are in use.
    variable_count: u8,
    /// The stretches of the template whose lengths vary, in order.
    variables: [Variable; VARIABLE_COUNT_MAX],
}

impl Template {
    const NONE: Self = Self {
        position: 0,
        size: 0,
        mask_start: 0,
        binaries_start: 0,
        binaries_count: 0,
        hits: 0,
        misses: 0,
        variable_count: 0,
        variables: [Variable { start: 0, end: 0 }; VARIABLE_COUNT_MAX],
    };

    /// Misses before a column is judged. Below this it has not been seen enough.
    // Note: a judgment, and not compared with other values. What it risks is small
    // either way: matches must pay for every template after a column's first.
    const MISSES_MIN: u32 = 3;

    /// Whether the column's chunks have differed from their templates too often for
    /// another template to be worth building.
    const fn misses_too_often(&self) -> bool {
        self.misses >= Self::MISSES_MIN && self.misses > self.hits
    }

    /// Whether no chunk of the column has been compared with a template yet.
    const fn is_untried(&self) -> bool {
        self.hits == 0 && self.misses == 0
    }

    /// The stretches of the template whose lengths vary, in order.
    fn variables(&self) -> &[Variable] {
        self.variables
            .get(..usize::from(self.variable_count))
            .unwrap_or_default()
    }

    /// Adds `found`, stretches a chunk was just seen to differ in, to the stretches
    /// whose lengths vary. Returns whether there was room for them all; if not,
    /// nothing is added.
    fn variables_add(&mut self, found: &[Variable]) -> bool {
        let mut merged = [Variable::default(); VARIABLE_COUNT_MAX];
        let mut count = 0;
        let mut ours = self.variables().iter().copied().peekable();
        let mut theirs = found.iter().copied().peekable();
        // Both are in order. Every round takes one from either, so there are no more
        // rounds than stretches.
        loop {
            let next = match (ours.peek(), theirs.peek()) {
                (Some(our), Some(their)) if our.start == their.start => {
                    theirs.next();
                    ours.next()
                }
                (Some(our), Some(their)) if our.start < their.start => ours.next(),
                (_, Some(_)) => theirs.next(),
                (Some(_), None) => ours.next(),
                (None, None) => break,
            };
            let (Some(next), Some(slot)) = (next, merged.get_mut(count)) else {
                return false;
            };
            *slot = next;
            count += 1;
        }
        // One integer or binary each, so no two overlap.
        debug_assert!(merged[..count].is_sorted_by(|a, b| a.end <= b.start));
        self.variables = merged;
        self.variable_count = u8::try_from(count).unwrap_or(u8::MAX);
        true
    }
}

/// Credit for building templates, in bytes of column chunk.
///
/// Building a template costs more than parsing its chunk, and is wasted if no later
/// chunk matches it. So templates are paid for. The pass is granted credit for a part
/// of the footer, which only a column's first template may spend. A chunk that matches
/// earns its length, which any template may spend. Whatever a footer holds, then, its
/// templates cost at most the grant beyond what they have saved, and a footer in which
/// nothing ever matches costs one template for each column at most.
#[derive(Debug)]
struct Budget {
    /// Bytes of chunk that may still be made into a column's first template.
    granted: i64,
    /// Bytes of chunk that matches have earned and templates have not yet spent.
    earned: i64,
}

impl Budget {
    /// The credit granted is the footer's remaining length divided by this: enough for
    /// a template for every column when there are at least this many row groups.
    /// Marking costs about a fifth more than parsing (`docs/perf/open.md`), so a
    /// footer in which nothing matches is passed over about one part in forty slower
    /// at the worst.
    // Note: the bound is measured: on the fixture where nothing matches, opening takes
    // the same time with and without templates. Other divisors have not been compared.
    // A smaller one gives more columns a template in the first row group of a file
    // with few row groups, and raises the worst case in proportion.
    const CREDIT_DIVISOR: usize = 8;

    /// The budget for a list of row groups that takes up at most `footer_size_left`
    /// bytes.
    fn new(footer_size_left: usize) -> Self {
        let granted = footer_size_left / Self::CREDIT_DIVISOR;
        Self {
            granted: i64::try_from(granted).unwrap_or(i64::MAX),
            earned: 0,
        }
    }

    /// Records that a chunk of `size` bytes matched its template.
    const fn earn(&mut self, size: i64) {
        self.earned = self.earned.saturating_add(size);
    }

    /// Whether a template can be paid for. `is_first` says the column has had none
    /// before; only such a template may spend the grant.
    const fn can_pay(&self, is_first: bool) -> bool {
        if is_first {
            self.granted > 0 || self.earned > 0
        } else {
            // The column has had a template before. Only what matches have earned
            // pays for another.
            self.earned > 0
        }
    }

    /// Pays `cost` bytes for a template.
    const fn pay(&mut self, is_first: bool, cost: i64) {
        if is_first && self.granted > 0 {
            self.granted = self.granted.saturating_sub(cost);
        } else {
            self.earned = self.earned.saturating_sub(cost);
        }
    }

    /// Whether matches have earned anything that is not yet spent.
    const fn has_earned(&self) -> bool {
        self.earned > 0
    }
}

/// The templates of one pass over a footer.
#[derive(Debug)]
pub(super) struct Templates {
    /// One for each column, in order. Filled as the first row group is passed over.
    columns: Vec<Template>,
    /// The marks of every template built, one after another.
    marks: Marks,
    /// What building templates may still cost.
    budget: Budget,
    /// The most bytes of mask to keep.
    mask_size_max: usize,
    /// The most columns to keep a template for.
    column_count_max: usize,
    /// Columns that have a template now.
    live_count: usize,
    /// Row groups begun so far.
    row_groups_begun: u32,
    /// Row groups still to come after the one being passed over.
    row_groups_left: u32,
    /// Chunks that matched only by being compared a second time, stepping over the
    /// lengths found to differ. Counted for the tests, which check that this stays
    /// rare: it costs several times what a match in the first comparison does.
    #[cfg(test)]
    stepped_count: u32,
}

impl Templates {
    /// The fewest row groups for which templates are used: a template needs a later
    /// chunk to be compared with.
    const ROW_GROUPS_MIN: u32 = 2;

    /// The smallest list of row groups, in bytes, for which templates are used. Below
    /// this the memory the templates need costs more to get than they can save.
    // Note: a judgment, and not measured. The templates take three allocations, about a
    // tenth of a microsecond, which is what parsing some hundreds of bytes costs. The
    // smallest footer they help has not been looked for.
    const FOOTER_SIZE_MIN: usize = 4096;

    /// The fewest row groups that must still come for a template to be replaced when
    /// an integer has grown. Building a template costs a little more than parsing a
    /// chunk, and stepping over one length costs a small part of that, so replacing
    /// pays only over many chunks.
    // Note: estimated from two measured costs, about 90 ns to build a template and a
    // few ns a chunk to step over a length. Replacing made the fixtures of 100 and of
    // 1000 row groups a third faster. Where exactly it starts to pay has not been
    // measured.
    const DRIFT_ROW_GROUPS_LEFT_MIN: u32 = 32;

    /// The mask may grow to the footer's remaining length divided by this, and so may
    /// the list of binaries. One row group's worth of each is all the templates need
    /// unless the columns keep changing, and a footer of this few row groups is the
    /// only kind that has more.
    const MASK_SIZE_DIVISOR: usize = 4;

    /// The list of columns may grow to the footer's remaining length divided by this.
    /// That is room for every column whose chunks take up twice what its entry in the
    /// list does, which a real column's chunks do in two row groups.
    ///
    /// A footer can claim a column for every byte it has, in chunks of a byte each.
    /// These bounds keep what the templates use to about the footer's own length.
    const COLUMNS_SIZE_DIVISOR: usize = 2;

    /// Templates for a list of `row_group_count` row groups that takes up at most
    /// `footer_size_left` bytes, if there is enough of it for them to pay.
    pub(super) fn for_row_groups(footer_size_left: usize, row_group_count: u32) -> Option<Self> {
        if row_group_count < Self::ROW_GROUPS_MIN {
            return None;
        }
        if footer_size_left < Self::FOOTER_SIZE_MIN {
            return None;
        }
        Some(Self::new(footer_size_left, row_group_count))
    }

    /// Templates for a list of `row_group_count` row groups that takes up at most
    /// `footer_size_left` bytes.
    pub(super) fn new(footer_size_left: usize, row_group_count: u32) -> Self {
        debug_assert!(row_group_count >= Self::ROW_GROUPS_MIN);
        Self {
            columns: Vec::new(),
            marks: Marks::new(),
            budget: Budget::new(footer_size_left),
            mask_size_max: footer_size_left / Self::MASK_SIZE_DIVISOR,
            column_count_max: footer_size_left / Self::COLUMNS_SIZE_DIVISOR / size_of::<Template>(),
            live_count: 0,
            row_groups_begun: 0,
            row_groups_left: row_group_count,
            #[cfg(test)]
            stepped_count: 0,
        }
    }

    /// Finds where every column chunk of the next row group starts.
    ///
    /// The chunks, one for each entry of `chunk_positions`, begin at `start`. Where
    /// each one starts is written to its entry, and where the last one ends is
    /// returned.
    pub(super) fn chunks_locate(
        &mut self,
        footer: &[u8],
        start: usize,
        chunk_positions: &mut [u32],
    ) -> Result<usize, FooterError> {
        self.row_group_begin();
        if self.is_idle() {
            chunks_parse(footer, start, chunk_positions)
        } else {
            self.columns_locate(0, footer, start, chunk_positions)
        }
    }

    /// Says that the chunks of the next row group follow.
    pub(super) const fn row_group_begin(&mut self) {
        self.row_groups_begun = self.row_groups_begun.saturating_add(1);
        self.row_groups_left = self.row_groups_left.saturating_sub(1);
    }

    /// Whether the templates have nothing more to do: no column has one, and none can
    /// get one.
    ///
    /// After the first row group every column has been offered a template. One that
    /// did not get it then gets one later only from what matches earn, and without a
    /// template nothing matches.
    pub(super) const fn is_idle(&self) -> bool {
        self.row_groups_begun > 1 && self.live_count == 0 && !self.budget.has_earned()
    }

    /// Returns where the chunk of `column` that starts at `start` ends.
    #[cfg(test)]
    pub(super) fn chunk_end(
        &mut self,
        column: u32,
        footer: &[u8],
        start: usize,
    ) -> Result<usize, FooterError> {
        self.columns_locate(column, footer, start, &mut [0])
    }

    /// [`Self::chunks_locate`] for the chunks of the columns from `column_first` on.
    ///
    /// Accepts and rejects exactly what [`CompactReader::skip`] does chunk by chunk,
    /// and returns the position `skip` would leave its reader at after the last.
    fn columns_locate(
        &mut self,
        column_first: u32,
        footer: &[u8],
        start: usize,
        chunk_positions: &mut [u32],
    ) -> Result<usize, FooterError> {
        if self.columns.is_empty() {
            // The first row group: room for every column that may get a template, made
            // once and not as the columns are met.
            self.columns
                .reserve_exact(chunk_positions.len().min(self.column_count_max));
        } else {
            // A later row group, which has the same columns.
        }
        let mut position = start;
        let mut located = 0;
        // Every round locates at least one chunk, so there are no more rounds than
        // chunks.
        while located < chunk_positions.len() {
            let column = column_first as usize + located;
            let has_template = self
                .columns
                .get(column)
                .is_some_and(|template| template.size > 0);
            if has_template {
                // A run of chunks that match their templates, which is most chunks of
                // most files.
                let run = Run {
                    templates: self.columns.get_mut(column..).unwrap_or_default(),
                    footer,
                    mask: self.marks.mask(),
                    chunk_positions: &mut chunk_positions[located..],
                }
                .pass(position);
                debug_assert!(run.end >= position);
                let size = i64::try_from(run.end - position).unwrap_or(i64::MAX);
                self.budget.earn(size);
                position = run.end;
                located += run.chunk_count;
                if run.chunk_count > 0 {
                    continue;
                }
                // The first chunk of the run does not match its template as it is.
            } else {
                // No template to compare this chunk with.
            }
            chunk_positions[located] = footer_u32(position);
            position = self.chunk_end_slowly(footer_u32(column), footer, position)?;
            located += 1;
        }
        Ok(position)
    }

    /// Where the chunk of `column` that starts at `start` ends, for a chunk that does
    /// not match a template as the template is: the column has none, or the chunk
    /// differs from it somewhere the template does not expect.
    ///
    /// Accepts and rejects exactly what [`CompactReader::skip`] does, and returns the
    /// position `skip` would leave its reader at.
    fn chunk_end_slowly(
        &mut self,
        column: u32,
        footer: &[u8],
        start: usize,
    ) -> Result<usize, FooterError> {
        let index = column as usize;
        if index >= self.columns.len() {
            if index >= self.column_count_max {
                // More columns than there is room to keep templates for.
                return skip_end(footer, start);
            }
            // Columns come in order in every row group, so a new one is always the next.
            debug_assert_eq!(index, self.columns.len());
            self.columns.push(Template::NONE);
        } else {
            // A column seen in an earlier row group.
        }
        let template = self.columns[index];
        if template.size > 0 {
            // The chunk is still parsed by the same steps as its template if the two
            // differ only in the lengths of integers and binaries.
            if let Some(stepped) = stepped_end(&template, footer, &self.marks, start) {
                debug_assert_eq!(skip_end(footer, start).ok(), Some(stepped.end));
                self.columns[index].hits = template.hits.saturating_add(1);
                self.budget.earn(i64::from(template.size));
                #[cfg(test)]
                {
                    self.stepped_count += 1;
                }
                self.learn(index, footer, start, &stepped);
                return Ok(stepped.end);
            }
            // The column's chunks have changed. Comparing later chunks with this
            // template would cost a little each time and save nothing.
            self.columns[index].misses = template.misses.saturating_add(1);
            self.columns[index].size = 0;
            self.live_count = self.live_count.saturating_sub(1);
        } else {
            // Nothing to compare the chunk with.
        }
        if self.may_build(self.columns[index]) {
            self.template_build(index, footer, start)
        } else {
            skip_end(footer, start)
        }
    }

    /// Takes in what a chunk that matched by stepping over lengths showed about its
    /// column: those lengths vary. The template remembers them, so that the next
    /// chunk is compared around them. If it has no room for more, the column's chunks
    /// have drifted away from it, and the chunk at `start` replaces it.
    fn learn(&mut self, index: usize, footer: &[u8], start: usize, stepped: &Stepped) {
        // An integer that has grown stays grown, if it is a file offset, and most that
        // grow are. A template with the new length is then compared without stepping
        // over anything, which repays building it when enough row groups follow.
        let has_drifted =
            stepped.integer_grew && self.row_groups_left >= Self::DRIFT_ROW_GROUPS_LEFT_MIN;
        if !has_drifted && self.columns[index].variables_add(stepped.variables()) {
            return;
        }
        if self.may_build(self.columns[index]) {
            let rebuilt = self.template_build(index, footer, start);
            // The chunk was just found to parse, and to end where it does.
            debug_assert_eq!(rebuilt.ok(), Some(stepped.end));
        } else if has_drifted {
            // No template may be built now. Remembering the lengths is the next best.
            self.columns[index].variables_add(stepped.variables());
        } else {
            // The template stays as it is, and chunks like this one stay slow.
        }
    }

    /// Whether the chunk about to be parsed should become its column's template.
    const fn may_build(&self, template: Template) -> bool {
        // A template in the last row group has nothing left to be compared with.
        if self.row_groups_left == 0 {
            return false;
        }
        if template.misses_too_often() {
            return false;
        }
        if self.marks.mask().len() >= self.mask_size_max {
            return false;
        }
        // A binary takes at least a byte of footer and four bytes here.
        if self.marks.binaries().len() >= self.mask_size_max / size_of::<u32>() {
            return false;
        }
        self.budget.can_pay(template.is_untried())
    }

    /// Parses the chunk of column `index` that starts at `start`, makes it the
    /// column's template, and returns where it ends.
    fn template_build(
        &mut self,
        index: usize,
        footer: &[u8],
        start: usize,
    ) -> Result<usize, FooterError> {
        let mask_start = self.marks.mask().len();
        let binaries_start = self.marks.binaries().len();
        let mut reader = CompactReader::new(footer, start);
        let marked = reader.skip_marking(ElementType::Struct, &mut self.marks);
        // Marking accepts what parsing accepts, and ends where parsing ends.
        debug_assert_eq!(
            skip_end(footer, start).ok(),
            marked.ok().map(|()| reader.position())
        );
        marked?;
        let end = reader.position();
        let size = end - start;
        debug_assert_eq!(self.marks.mask().len(), mask_start + size);
        let cost = i64::try_from(size).unwrap_or(i64::MAX);
        let template = &mut self.columns[index];
        self.budget.pay(template.is_untried(), cost);
        if template.size == 0 {
            self.live_count += 1;
        } else {
            // The column had a template, which this one replaces.
        }
        template.position = footer_u32(start);
        template.size = footer_u32(size);
        // The mask and the list of binaries are kept below the footer's length.
        template.mask_start = footer_u32(mask_start);
        template.binaries_start = footer_u32(binaries_start);
        template.binaries_count = footer_u32(self.marks.binaries().len() - binaries_start);
        template.variable_count = 0;
        Ok(end)
    }
}

#[cfg(test)]
impl Templates {
    /// Chunks of `column` that matched a template.
    pub(super) fn hit_count(&self, column: u32) -> u32 {
        self.columns[column as usize].hits
    }

    /// Chunks of `column` that were compared with a template and did not match.
    pub(super) fn miss_count(&self, column: u32) -> u32 {
        self.columns[column as usize].misses
    }

    /// Whether `column` has a template for its next chunk to be compared with.
    pub(super) fn has_template(&self, column: u32) -> bool {
        self.columns[column as usize].size > 0
    }

    /// How many lengths the template of `column` remembers as varying.
    pub(super) fn variable_count(&self, column: u32) -> u8 {
        self.columns[column as usize].variable_count
    }

    /// Chunks that matched only by being compared a second time.
    pub(super) const fn stepped_count(&self) -> u32 {
        self.stepped_count
    }

    /// Where the template chunk of `column` starts, if the column has a template.
    pub(super) fn template_start(&self, column: u32) -> Option<usize> {
        let template = self.columns[column as usize];
        (template.size > 0).then_some(template.position as usize)
    }
}

/// The column chunks of a row group from some column on, with what is needed to pass
/// over those that match their templates.
struct Run<'a> {
    /// The templates of those columns. Fewer than the chunks, if the later columns
    /// have no template yet.
    templates: &'a mut [Template],
    footer: &'a [u8],
    /// The whole mask, which the templates point into.
    mask: &'a [u8],
    /// Where to write the position of each chunk passed over.
    chunk_positions: &'a mut [u32],
}

/// How far a [`Run`] got.
struct RunEnd {
    /// Chunks passed over.
    chunk_count: usize,
    /// Where the last of them ends in the footer.
    end: usize,
}

impl Run<'_> {
    /// Passes over chunks, from the one that starts at `start`, for as long as each
    /// matches its column's template.
    // The loop that most of the pass's time goes to on a file of many row groups. It
    // is its own function, over slices and integers, so that its few values stay in
    // registers: inlined into the pass, they were kept on the stack
    // (`docs/perf/open.md`).
    #[inline(never)]
    fn pass(self, start: usize) -> RunEnd {
        let mut position = start;
        let mut chunk_count = 0;
        for (template, chunk_position) in self.templates.iter_mut().zip(self.chunk_positions) {
            let Some(end) = matched_end(template, self.footer, self.mask, position) else {
                break;
            };
            // Parsing takes the same steps over this chunk as over the template.
            debug_assert_eq!(skip_end(self.footer, position).ok(), Some(end));
            *chunk_position = footer_u32(position);
            template.hits = template.hits.saturating_add(1);
            position = end;
            chunk_count += 1;
        }
        RunEnd {
            chunk_count,
            end: position,
        }
    }
}

/// Finds where each of a run of chunks starts by parsing them. Where each one starts
/// is written to its entry of `chunk_positions`, and where the last ends is returned.
// Note: its own function for the same reason as `Run::pass`. This loop is all of opening
// a file whose chunks cannot be compared, and inlined into the pass its speed moved by a
// tenth with unrelated changes to the pass (`docs/perf/open.md`).
#[inline(never)]
pub(super) fn chunks_parse(
    footer: &[u8],
    start: usize,
    chunk_positions: &mut [u32],
) -> Result<usize, FooterError> {
    let mut reader = CompactReader::new(footer, start);
    for chunk_position in chunk_positions {
        *chunk_position = footer_u32(reader.position());
        reader.skip(ElementType::Struct)?;
    }
    Ok(reader.position())
}

/// Where the struct that starts at `start` ends, by parsing it.
fn skip_end(footer: &[u8], start: usize) -> Result<usize, FooterError> {
    let mut reader = CompactReader::new(footer, start);
    reader.skip(ElementType::Struct)?;
    Ok(reader.position())
}
