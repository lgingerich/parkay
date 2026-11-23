//! Comparing a column chunk with its template: the part of the templates that has no
//! state.
//!
//! Everything here is a function of a template, the marks it was built with, and the
//! footer's bytes. Each answers one question: does this chunk match, and if so, where
//! does it end? The argument that a match is sound is on [`matched_end`].

use super::Template;
use crate::metadata::utils::footer_u32;
use crate::thrift::{CompactReader, Marks};

/// The most lengths a template remembers as varying. A string column's statistics
/// have two to four, and a file offset that has grown a byte is one more.
// Note: reasoned from what a column chunk holds, and not compared with other values.
// Each one adds eight bytes to every column's template.
pub(super) const VARIABLE_COUNT_MAX: usize = 4;

/// The most lengths that may differ between a chunk and its template before the chunk
/// is parsed instead. Each costs about as much as parsing a field, so beyond a few the
/// comparison saves little.
// Note: reasoned, and not compared with other values. It only decides between two slow
// paths: comparing twice, or parsing.
pub(super) const RESYNC_COUNT_MAX: usize = 8;

const _: () = assert!(VARIABLE_COUNT_MAX <= RESYNC_COUNT_MAX);
const _: () = assert!(VARIABLE_COUNT_MAX <= u8::MAX as usize);

/// A stretch of a template chunk that holds one integer or one binary whose length
/// varies from chunk to chunk. Parsing takes the same steps whatever the length, so a
/// comparison steps over the stretch, and over its counterpart in the other chunk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Variable {
    /// Where the integer, or the binary's length, starts in the template chunk.
    pub(super) start: u32,
    /// Where the integer, or the binary's contents, ends in the template chunk.
    pub(super) end: u32,
}

/// Where the chunk that starts at `start` ends, if it matches `template`: it agrees
/// with the template chunk on every marked bit outside the stretches whose lengths
/// vary, and has an integer or a binary where the template has each of those.
///
/// Such a chunk is parsed by the same steps as the template chunk: the marked bits
/// decide every step, and a step that passes over an integer or a binary takes any
/// length of one.
pub(super) fn matched_end(
    template: &Template,
    footer: &[u8],
    mask: &[u8],
    start: usize,
) -> Option<usize> {
    let size = template.size as usize;
    if size == 0 {
        // The column has no template.
        return None;
    }
    let template_bytes = footer.get(template.position as usize..)?.get(..size)?;
    let template_mask = mask.get(template.mask_start as usize..)?.get(..size)?;
    // How much of the template is behind, and where the candidate has got to.
    let mut template_offset = 0;
    let mut position = start;
    for variable in template.variables() {
        let variable_start = variable.start as usize;
        // The stretch before the variable, which must agree.
        let fixed_size = variable_start.checked_sub(template_offset)?;
        let fixed = Compared {
            template: template_bytes.get(template_offset..variable_start)?,
            mask: template_mask.get(template_offset..variable_start)?,
            candidate: footer.get(position..)?.get(..fixed_size)?,
        };
        if !fixed.agrees() {
            return None;
        }
        // Then the candidate's own integer or binary, of whatever length.
        let candidate_variable = position + fixed_size;
        position = if *template_mask.get(variable_start)? == Marks::VARINT {
            varint_end(footer, candidate_variable)?
        } else {
            binary_end(footer, candidate_variable)?
        };
        template_offset = variable.end as usize;
    }
    let rest_size = size.checked_sub(template_offset)?;
    let rest = Compared {
        template: template_bytes.get(template_offset..)?,
        mask: template_mask.get(template_offset..)?,
        candidate: footer.get(position..)?.get(..rest_size)?,
    };
    rest.agrees().then_some(position + rest_size)
}

/// A place in a comparison of a candidate chunk with a template.
#[derive(Debug, Clone, Copy)]
pub(super) struct Place {
    /// Bytes of the template compared or stepped over so far.
    pub(super) template_offset: usize,
    /// The position in the footer of the candidate's byte that comes next.
    pub(super) candidate_position: usize,
}

/// What a comparison that stepped over every length it found to differ came to.
#[derive(Debug)]
pub(super) struct Stepped {
    /// Where the chunk ends.
    pub(super) end: usize,
    /// The stretches of the template that were stepped over, in order.
    pub(super) variables: [Variable; RESYNC_COUNT_MAX],
    /// How many of `variables` are in use.
    pub(super) variable_count: usize,
    /// Whether an integer of the chunk is longer than the template's.
    pub(super) integer_grew: bool,
}

impl Stepped {
    /// The stretches of the template that were stepped over, in order.
    pub(super) fn variables(&self) -> &[Variable] {
        self.variables
            .get(..self.variable_count)
            .unwrap_or_default()
    }
}

/// Where the chunk that starts at `start` ends, if parsing it takes the same steps as
/// parsing the template chunk took, give or take the lengths of integers and
/// binaries. Finds those that differ without being told, by comparing up to each.
pub(super) fn stepped_end(
    template: &Template,
    footer: &[u8],
    marks: &Marks,
    start: usize,
) -> Option<Stepped> {
    let size = template.size as usize;
    let template_start = template.position as usize;
    let mask_start = template.mask_start as usize;
    let template_bytes = footer.get(template_start..template_start + size)?;
    let mask = marks.mask().get(mask_start..mask_start + size)?;
    let mut place = Place {
        template_offset: 0,
        candidate_position: start,
    };
    let mut stepped = Stepped {
        end: start,
        variables: [Variable::default(); RESYNC_COUNT_MAX],
        variable_count: 0,
        integer_grew: false,
    };
    // Every round but the last steps over one length, and there is room to record only
    // so many.
    loop {
        let candidate = footer.get(place.candidate_position..)?;
        let size_left = size - place.template_offset;
        let size_compared = size_left.min(candidate.len());
        let compared_end = place.template_offset + size_compared;
        let agreeing = Compared {
            template: &template_bytes[place.template_offset..compared_end],
            mask: &mask[place.template_offset..compared_end],
            candidate: &candidate[..size_compared],
        }
        .agreeing_size();
        if agreeing == size_left {
            stepped.end = place.candidate_position + agreeing;
            return Some(stepped);
        }
        if agreeing == size_compared {
            // The footer ends inside the candidate. Parsing it says how.
            return None;
        }
        let difference = Place {
            template_offset: place.template_offset + agreeing,
            candidate_position: place.candidate_position + agreeing,
        };
        let step = step_over(template, footer, marks, difference)?;
        // Every round steps over at least the byte that differed.
        debug_assert!(step.variable.end as usize > difference.template_offset);
        debug_assert!(step.variable.end as usize <= size);
        *stepped.variables.get_mut(stepped.variable_count)? = step.variable;
        stepped.variable_count += 1;
        stepped.integer_grew |= step.integer_grew;
        place = Place {
            template_offset: step.variable.end as usize,
            candidate_position: step.candidate_position,
        };
    }
}

/// One length stepped over in both chunks.
struct Step {
    /// The stretch of the template that holds the integer or binary.
    variable: Variable,
    /// Where the candidate goes on after its own.
    pub(super) candidate_position: usize,
    /// Whether it is an integer that is longer in the candidate.
    pub(super) integer_grew: bool,
}

/// Steps over the integer or binary that the two chunks differ in at `difference`.
/// `None` if they differ in anything else, and so are not parsed by the same steps.
fn step_over(template: &Template, footer: &[u8], marks: &Marks, difference: Place) -> Option<Step> {
    let template_start = template.position as usize;
    let template_at = template_start + difference.template_offset;
    let mask_start = template.mask_start as usize;
    let mask_index = mask_start + difference.template_offset;
    let mark = *marks.mask().get(mask_index)?;
    if mark == Marks::VARINT {
        // The two integers differ in a continuation bit, so one ends here and the
        // other goes on. Parsing steps over an integer of any length. The template's
        // began where the bytes before this one stop being continued varint bytes.
        let before = marks.mask().get(mask_start..mask_index)?;
        let bytes_before = footer.get(template_start..template_at)?;
        let continued = before
            .iter()
            .zip(bytes_before)
            .rev()
            .take_while(|&(&mark_before, &byte)| mark_before == Marks::VARINT && byte >= 0x80)
            .count();
        let template_end = varint_end(footer, template_at)?;
        return Some(Step {
            variable: Variable {
                start: footer_u32(difference.template_offset - continued),
                end: footer_u32(template_end - template_start),
            },
            candidate_position: varint_end(footer, difference.candidate_position)?,
            // The template's integer ends at the byte that differs, so the
            // candidate's goes on.
            integer_grew: template_end == template_at + 1,
        });
    }
    // A byte that parsing read whole. The steps stay the same only if it is part of a
    // binary's length.
    debug_assert_eq!(mark, Marks::STRUCTURE);
    let binaries_start = template.binaries_start as usize;
    let binaries_end = binaries_start + template.binaries_count as usize;
    let binaries = marks.binaries().get(binaries_start..binaries_end)?;
    // The last binary of the template that starts at or before the byte.
    let before = binaries.partition_point(|&binary| binary as usize <= mask_index);
    let binary = *binaries.get(before.checked_sub(1)?)? as usize;
    // Bytes of that binary's length that come before the byte that differs. The two
    // chunks agree on them, so the candidate's binary starts as far back.
    let agreed = mask_index - binary;
    let template_binary = template_at.checked_sub(agreed)?;
    let candidate_binary = difference.candidate_position.checked_sub(agreed)?;
    if varint_end(footer, template_binary)? <= template_at {
        // The binary's length ends before the byte, which is something else.
        return None;
    }
    Some(Step {
        variable: Variable {
            start: footer_u32(template_binary - template_start),
            end: footer_u32(binary_end(footer, template_binary)? - template_start),
        },
        candidate_position: binary_end(footer, candidate_binary)?,
        integer_grew: false,
    })
}

/// Three runs of bytes of one length: a stretch of a template chunk, its mask, and the
/// stretch of a chunk being compared with it.
pub(super) struct Compared<'a> {
    pub(super) template: &'a [u8],
    pub(super) mask: &'a [u8],
    pub(super) candidate: &'a [u8],
}

impl Compared<'_> {
    /// Whether the candidate agrees with the template on every bit that is set in the
    /// mask.
    pub(super) fn agrees(&self) -> bool {
        debug_assert_eq!(self.template.len(), self.mask.len());
        debug_assert_eq!(self.candidate.len(), self.mask.len());
        let (template_words, _) = self.template.as_chunks::<8>();
        let (mask_words, _) = self.mask.as_chunks::<8>();
        let (candidate_words, _) = self.candidate.as_chunks::<8>();
        // Differences are gathered and tested once at the end, so the loop has no
        // branch in it.
        let mut difference = 0;
        let words = template_words.iter().zip(mask_words).zip(candidate_words);
        for ((template_word, mask_word), candidate_word) in words {
            difference |= (u64::from_le_bytes(*template_word)
                ^ u64::from_le_bytes(*candidate_word))
                & u64::from_le_bytes(*mask_word);
        }
        let last_words = (
            self.template.last_chunk::<8>(),
            self.mask.last_chunk::<8>(),
            self.candidate.last_chunk::<8>(),
        );
        if let (Some(template_word), Some(mask_word), Some(candidate_word)) = last_words {
            // The last word, which overlaps the words above unless they left nothing
            // over. Comparing some bytes twice costs less than a loop over the rest.
            difference |= (u64::from_le_bytes(*template_word)
                ^ u64::from_le_bytes(*candidate_word))
                & u64::from_le_bytes(*mask_word);
        } else {
            // Shorter than a word.
            let bytes = self.template.iter().zip(self.mask).zip(self.candidate);
            for ((template_byte, mask_byte), candidate_byte) in bytes {
                difference |= u64::from((template_byte ^ candidate_byte) & mask_byte);
            }
        }
        difference == 0
    }

    /// How many leading bytes of the candidate agree with the template on every bit
    /// that is set in the mask.
    fn agreeing_size(&self) -> usize {
        debug_assert_eq!(self.template.len(), self.mask.len());
        debug_assert_eq!(self.candidate.len(), self.mask.len());
        let (template_words, template_tail) = self.template.as_chunks::<8>();
        let (mask_words, mask_tail) = self.mask.as_chunks::<8>();
        let (candidate_words, candidate_tail) = self.candidate.as_chunks::<8>();
        let words = template_words.iter().zip(mask_words).zip(candidate_words);
        for (index, ((template_word, mask_word), candidate_word)) in words.enumerate() {
            let differing = (u64::from_le_bytes(*template_word)
                ^ u64::from_le_bytes(*candidate_word))
                & u64::from_le_bytes(*mask_word);
            if differing != 0 {
                // The words are read little-endian, so the lowest bit that differs is
                // in the first byte that differs.
                return index * 8 + (differing.trailing_zeros() / 8) as usize;
            }
        }
        let tail_start = template_words.len() * 8;
        let tail = template_tail.iter().zip(mask_tail).zip(candidate_tail);
        for (index, ((template_byte, mask_byte), candidate_byte)) in tail.enumerate() {
            if (template_byte ^ candidate_byte) & mask_byte != 0 {
                return tail_start + index;
            }
        }
        self.mask.len()
    }
}

/// Where the varint that holds the byte at `position` ends: after its first byte, from
/// there on, without a continuation bit. `None` if the footer ends first.
pub(super) fn varint_end(footer: &[u8], position: usize) -> Option<usize> {
    // What `skip` does for an integer, without a reader: this runs for every length
    // that varies in every chunk compared.
    let last = footer
        .get(position..)?
        .iter()
        .position(|&byte| byte < 0x80)?;
    Some(position + last + 1)
}

/// Where the binary whose length starts at `position` ends. `None` if its length is
/// not valid or it does not fit in the footer.
pub(super) fn binary_end(footer: &[u8], position: usize) -> Option<usize> {
    let mut reader = CompactReader::new(footer, position);
    reader.read_binary().ok()?;
    Some(reader.position())
}
