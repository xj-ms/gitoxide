use std::{iter::Peekable, ops::Range};

use bstr::{BStr, ByteSlice, ByteVec};

use crate::blob::builtin_driver::text::ConflictStyle;

/// Used only when `diff3` is the conflict style as `zdiff3` automatically reduces hunks into nothing.
/// Here we check if all hunks are the same.
pub fn hunks_differ_in_diff3(
    style: ConflictStyle,
    a: &[Hunk],
    b: &[Hunk],
    input: &imara_diff::InternedInput<&[u8]>,
    current_tokens: &[imara_diff::Token],
) -> bool {
    if style != ConflictStyle::Diff3 {
        return true;
    }

    let tokens_for_hunk = |hunk: &Hunk| -> &[imara_diff::Token] {
        &tokens_for_side(hunk.side, input, current_tokens)[hunk.after.start as usize..hunk.after.end as usize]
    };

    a.iter()
        .flat_map(tokens_for_hunk)
        .ne(b.iter().flat_map(tokens_for_hunk))
}

pub fn contains_lines(hunks: &[Hunk]) -> bool {
    hunks.iter().any(|h| !h.after.is_empty())
}

/// ## Deviation
///
/// This implementation definitely isn't the same as in Git, primarily because it seemed impossible
/// to understand what's going on there without investing more time than it seemed worth.
pub fn detect_line_ending(
    hunks: &[Hunk],
    input: &imara_diff::InternedInput<&[u8]>,
    current_tokens: &[imara_diff::Token],
) -> Option<&'static BStr> {
    fn is_eol_crlf(
        hunks: &[Hunk],
        input: &imara_diff::InternedInput<&[u8]>,
        current_tokens: &[imara_diff::Token],
    ) -> Option<bool> {
        let (range, side) = hunks.iter().rev().find_map(|h| {
            (!h.after.is_empty())
                .then_some((&h.after, h.side))
                .or((!h.before.is_empty()).then_some((&h.before, Side::Ancestor)))
        })?;

        let tokens = tokens_for_side(side, input, current_tokens);
        {
            let last_line = tokens
                .get(range.end as usize - 1)
                .map(|token| &input.interner[*token])?;
            if last_line.last() == Some(&b'\n') {
                return last_line.get(last_line.len().checked_sub(2)?).map(|c| *c == b'\r');
            }
        }
        let second_to_last_line = tokens
            .get(range.end.checked_sub(2)? as usize)
            .map(|token| &input.interner[*token])?;
        second_to_last_line
            .get(second_to_last_line.len().checked_sub(2)?)
            .map(|c| *c == b'\r')
    }
    is_eol_crlf(hunks, input, current_tokens).map(|is_crlf| if is_crlf { b"\r\n".into() } else { b"\n".into() })
}

pub fn detect_line_ending_or_nl(
    hunks: &[Hunk],
    input: &imara_diff::InternedInput<&[u8]>,
    current_tokens: &[imara_diff::Token],
) -> &'static BStr {
    detect_line_ending(hunks, input, current_tokens).unwrap_or(b"\n".into())
}

fn tokens_for_side<'a>(
    side: Side,
    input: &'a imara_diff::InternedInput<&[u8]>,
    current_tokens: &'a [imara_diff::Token],
) -> &'a [imara_diff::Token] {
    match side {
        Side::Current => current_tokens,
        Side::Other => &input.after,
        Side::Ancestor => &input.before,
    }
}

pub fn assure_ends_with_nl(out: &mut Vec<u8>, nl: &BStr) {
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push_str(nl);
    }
}

pub fn write_conflict_marker(out: &mut Vec<u8>, marker: u8, label: Option<&BStr>, marker_size: u8, nl: &BStr) {
    assure_ends_with_nl(out, nl);
    out.extend(std::iter::repeat_n(marker, marker_size as usize));
    if let Some(label) = label {
        out.push(b' ');
        out.extend_from_slice(label);
    }
    out.push_str(nl);
}

pub fn write_ancestor(input: &imara_diff::InternedInput<&[u8]>, from: u32, to: usize, out: &mut Vec<u8>) {
    if to < from as usize {
        return;
    }
    if let Some(tokens) = input.before.get(from as usize..to) {
        write_tokens(&input.interner, tokens, out);
    }
}

/// Look at all hunks in `in_out` and fill in the ancestor in the range of `ancestor_range`.
/// This is all based on knowing the ranges are sequences of tokens.
pub fn fill_ancestor(Range { start, end }: &Range<u32>, in_out: &mut Vec<Hunk>) {
    fn is_nonzero(num: &u32) -> bool {
        *num > 0
    }
    if in_out.is_empty() {
        return;
    }
    let first = &in_out[0];
    let mut first_idx = 0;
    if let Some(lines_to_add) = first.before.start.checked_sub(*start).filter(is_nonzero) {
        in_out.insert(0, ancestor_hunk(*start, lines_to_add));
        first_idx += 1;
    }

    let mut added_hunks = false;
    for (idx, next_idx) in (first_idx..in_out.len()).map(|idx| (idx, idx + 1)) {
        let Some(next_hunk) = in_out.get(next_idx) else {
            break;
        };
        let hunk = &in_out[idx];
        if let Some(lines_to_add) = next_hunk.before.start.checked_sub(hunk.before.end).filter(is_nonzero) {
            in_out.push(ancestor_hunk(hunk.before.end, lines_to_add));
            added_hunks = true;
        }
    }
    let in_out_len = in_out.len();
    if added_hunks {
        in_out[first_idx..in_out_len].sort_by_key(|hunk| hunk.before.start);
    }

    let last = &in_out[in_out_len - 1];
    if let Some(lines_to_add) = end.checked_sub(last.before.end).filter(is_nonzero) {
        in_out.push(ancestor_hunk(last.before.end, lines_to_add));
    }
}

fn ancestor_hunk(start: u32, num_lines: u32) -> Hunk {
    let range = start..start + num_lines;
    Hunk {
        before: range.clone(),
        after: range,
        side: Side::Ancestor,
    }
}

/// Reduce the area of `a_hunks` and the hunks in `b_hunks` so that only those lines that are
/// actually different remain. Note that we have to compare the resolved values, not only the tokens,
/// so `current_tokens` is expected to be known to the `input` (and its `interner`).
/// Hunks from all input arrays maybe removed in the process from the front and back, in case they
/// are entirely equal to each other.
/// Note also that `a_hunks` and `b_hunks` are treated to be consecutive, so [`fill_ancestor()`] must
/// have been called beforehand, and are assumed to cover the same space in the ancestor buffer.
///
/// Return a new vector of all the hunks that were removed from front and back, with partial hunks inserted,
/// along with the amount of hunks that go front, with the remaining going towards the back.
#[must_use]
pub fn zealously_contract_hunks(
    a_hunks: &mut Vec<Hunk>,
    b_hunks: &mut Vec<Hunk>,
    input: &imara_diff::InternedInput<&[u8]>,
    current_tokens: &[imara_diff::Token],
) -> (Vec<Hunk>, usize) {
    let line_content = |token_idx: u32, side: Side| {
        let tokens = match side {
            Side::Current => current_tokens,
            Side::Other => &input.after,
            Side::Ancestor => &input.before,
        };
        &input.interner[tokens[token_idx as usize]]
    };
    let (mut last_a_hunk_idx, mut last_b_hunk_idx) = (0, 0);
    let (mut out, hunks_in_front) = {
        let (mut remove_leading_a_hunks_from, mut remove_leading_b_hunks_from) = (None, None);
        let (mut a_hunk_token_equal_till, mut b_hunk_token_equal_till) = (None, None);
        for ((a_token_idx, a_hunk_idx, a_hunk_side), (b_token_idx, b_hunk_idx, b_hunk_side)) in
            iterate_hunks(a_hunks).zip(iterate_hunks(b_hunks))
        {
            let a_line = line_content(a_token_idx, a_hunk_side).as_bstr();
            let b_line = line_content(b_token_idx, b_hunk_side).as_bstr();

            if last_a_hunk_idx != a_hunk_idx {
                a_hunk_token_equal_till = None;
                last_a_hunk_idx = a_hunk_idx;
            }
            if last_b_hunk_idx != b_hunk_idx {
                b_hunk_token_equal_till = None;
                last_b_hunk_idx = b_hunk_idx;
            }
            if a_line == b_line {
                (remove_leading_a_hunks_from, remove_leading_b_hunks_from) = (Some(a_hunk_idx), Some(b_hunk_idx));
                (a_hunk_token_equal_till, b_hunk_token_equal_till) = (Some(a_token_idx), Some(b_token_idx));
            } else {
                break;
            }
        }

        let mut out = Vec::with_capacity(
            remove_leading_a_hunks_from.unwrap_or_else(|| if a_hunk_token_equal_till.is_some() { 1 } else { 0 }),
        );
        truncate_hunks_from_from_front(
            a_hunks,
            remove_leading_a_hunks_from,
            a_hunk_token_equal_till,
            Some(&mut out),
        );
        truncate_hunks_from_from_front(b_hunks, remove_leading_b_hunks_from, b_hunk_token_equal_till, None);
        let hunks_in_front = out.len();
        (out, hunks_in_front)
    };

    (last_a_hunk_idx, last_b_hunk_idx) = (0, 0);
    {
        let (mut remove_trailing_a_hunks_from, mut remove_trailing_b_hunks_from) = (None, None);
        let (mut a_hunk_token_equal_from, mut b_hunk_token_equal_from) = (None, None);
        for ((a_token_idx, a_hunk_idx, a_hunk_side), (b_token_idx, b_hunk_idx, b_hunk_side)) in
            iterate_hunks_rev(a_hunks).zip(iterate_hunks_rev(b_hunks))
        {
            let a_line = line_content(a_token_idx, a_hunk_side).as_bstr();
            let b_line = line_content(b_token_idx, b_hunk_side).as_bstr();

            if last_a_hunk_idx != a_hunk_idx {
                a_hunk_token_equal_from = None;
                last_a_hunk_idx = a_hunk_idx;
            }
            if last_b_hunk_idx != b_hunk_idx {
                b_hunk_token_equal_from = None;
                last_b_hunk_idx = b_hunk_idx;
            }

            if a_line == b_line {
                (remove_trailing_a_hunks_from, remove_trailing_b_hunks_from) = (Some(a_hunk_idx), Some(b_hunk_idx));
                (a_hunk_token_equal_from, b_hunk_token_equal_from) = (Some(a_token_idx), Some(b_token_idx));
            } else {
                break;
            }
        }

        truncate_hunks_from_from_back(
            a_hunks,
            remove_trailing_a_hunks_from,
            a_hunk_token_equal_from,
            Some(&mut out),
        );
        truncate_hunks_from_from_back(b_hunks, remove_trailing_b_hunks_from, b_hunk_token_equal_from, None);
    }

    (out, hunks_in_front)
}

fn range_by_side(hunk: &mut Hunk) -> &mut Range<u32> {
    match hunk.side {
        Side::Current | Side::Other => &mut hunk.after,
        Side::Ancestor => &mut hunk.before,
    }
}
fn truncate_hunks_from_from_front(
    hunks: &mut Vec<Hunk>,
    hunks_to_remove_until_idx: Option<usize>,
    hunk_token_equal_till: Option<u32>,
    mut out_hunks: Option<&mut Vec<Hunk>>,
) {
    let Some(hunks_to_remove_until_idx) = hunks_to_remove_until_idx else {
        assert!(hunk_token_equal_till.is_none());
        return;
    };
    let mut last_index_to_remove = Some(hunks_to_remove_until_idx);
    let hunk = &mut hunks[hunks_to_remove_until_idx];
    let range = range_by_side(hunk);
    if let Some(hunk_token_equal_till) = hunk_token_equal_till {
        let orig_start = range.start;
        let new_start = hunk_token_equal_till + 1;
        range.start = new_start;
        if Range::<u32>::is_empty(range) {
            range.start = orig_start;
        } else if let Some(out) = out_hunks.as_deref_mut() {
            last_index_to_remove = hunks_to_remove_until_idx.checked_sub(1);
            let mut removed_hunk = hunk.clone();
            let new_range = range_by_side(&mut removed_hunk);

            new_range.start = orig_start;
            new_range.end = new_start;

            out.push(removed_hunk);
        } else {
            last_index_to_remove = hunks_to_remove_until_idx.checked_sub(1);
        }
    }
    if let Some(last_index_to_remove) = last_index_to_remove {
        let mut current_idx = 0;
        hunks.retain(|hunk| {
            if current_idx > last_index_to_remove {
                true
            } else {
                current_idx += 1;
                if let Some(out) = out_hunks.as_deref_mut() {
                    out.push(hunk.clone());
                }
                false
            }
        });
    }
}

fn truncate_hunks_from_from_back(
    hunks: &mut Vec<Hunk>,
    remove_trailing_hunks_from_idx: Option<usize>,
    hunk_token_equal_from: Option<u32>,
    mut out_hunks: Option<&mut Vec<Hunk>>,
) {
    let Some(mut remove_trailing_hunks_from_idx) = remove_trailing_hunks_from_idx else {
        assert!(hunk_token_equal_from.is_none());
        return;
    };

    let hunk = &mut hunks[remove_trailing_hunks_from_idx];
    let range = range_by_side(hunk);
    if let Some(hunk_token_equal_from) = hunk_token_equal_from {
        let orig_end = range.end;
        let new_end = hunk_token_equal_from;
        range.end = new_end;
        if Range::<u32>::is_empty(range) {
            range.end = orig_end;
        } else if let Some(out) = out_hunks.as_deref_mut() {
            remove_trailing_hunks_from_idx += 1;
            let mut removed_hunk = hunk.clone();
            let new_range = range_by_side(&mut removed_hunk);

            new_range.start = new_end;
            new_range.end = orig_end;

            out.push(removed_hunk);
        } else {
            remove_trailing_hunks_from_idx += 1;
        }
    }
    if let Some(out) = out_hunks {
        out.extend_from_slice(&hunks[remove_trailing_hunks_from_idx..]);
    }
    hunks.truncate(remove_trailing_hunks_from_idx);
}

/// Return an iterator over `(token_idx, hunk_idx, hunk_side)` from `hunks`.
fn iterate_hunks(hunks: &[Hunk]) -> impl Iterator<Item = (u32, usize, Side)> + '_ {
    hunks.iter().enumerate().flat_map(|(hunk_idx, hunk)| {
        match hunk.side {
            Side::Current | Side::Other => &hunk.after,
            Side::Ancestor => &hunk.before,
        }
        .clone()
        .map(move |idx| (idx, hunk_idx, hunk.side))
    })
}

/// Return a reverse iterator over `(token_idx, hunk_idx, hunk_side)` from `hunks`.
fn iterate_hunks_rev(hunks: &[Hunk]) -> impl Iterator<Item = (u32, usize, Side)> + '_ {
    hunks.iter().enumerate().rev().flat_map(|(hunk_idx, hunk)| {
        match hunk.side {
            Side::Current | Side::Other => &hunk.after,
            Side::Ancestor => &hunk.before,
        }
        .clone()
        .rev()
        .map(move |idx| (idx, hunk_idx, hunk.side))
    })
}

pub fn write_hunks(
    hunks: &[Hunk],
    input: &imara_diff::InternedInput<&[u8]>,
    current_tokens: &[imara_diff::Token],
    out: &mut Vec<u8>,
) {
    for hunk in hunks {
        let (tokens, range) = match hunk.side {
            Side::Current => (current_tokens, &hunk.after),
            Side::Other => (input.after.as_slice(), &hunk.after),
            Side::Ancestor => (input.before.as_slice(), &hunk.before),
        };
        write_tokens(&input.interner, &tokens[usize_range(range)], out);
    }
}

fn usize_range(range: &Range<u32>) -> Range<usize> {
    range.start as usize..range.end as usize
}

fn write_tokens(interner: &imara_diff::Interner<&[u8]>, tokens: &[imara_diff::Token], out: &mut Vec<u8>) {
    for token in tokens {
        out.extend_from_slice(interner[*token]);
    }
}

/// Find all hunks in `iter` which aren't from the same side as `hunk` and intersect with it.
/// Also put `hunk` into `input` so it's the first item, and possibly put more hunks of the side of `hunk` so
/// `iter` doesn't have any overlapping hunks left.
/// Return `Some` if `intersecting` is non-empty after the operation, indicating overlapping hunks were found.
pub fn take_intersecting(
    iter: &mut Peekable<impl Iterator<Item = Hunk>>,
    input: &mut Vec<Hunk>,
    intersecting: &mut Vec<Hunk>,
) -> Option<()> {
    input.clear();
    input.push(iter.next()?);
    intersecting.clear();

    fn left_overlaps_right(left: &Hunk, right: &Hunk) -> bool {
        left.side != right.side
            && (right.before.contains(&left.before.start)
                || (right.before.is_empty() && right.before.start == left.before.start))
    }

    loop {
        let hunk = input.last().expect("just pushed");
        while iter.peek().filter(|b_hunk| left_overlaps_right(b_hunk, hunk)).is_some() {
            intersecting.extend(iter.next());
        }
        // The hunks that overlap might themselves overlap with a following hunk of the other side.
        // If so, split it so it doesn't overlap anymore.
        let mut found_more_intersections = false;
        while intersecting
            .last_mut()
            .zip(iter.peek_mut())
            .filter(|(last_intersecting, candidate)| left_overlaps_right(candidate, last_intersecting))
            .is_some()
        {
            input.extend(iter.next());
            found_more_intersections = true;
        }
        if !found_more_intersections {
            break;
        }
    }
    Some(())
}

pub fn tokens(input: &[u8]) -> imara_diff::sources::ByteLines<'_> {
    imara_diff::sources::byte_lines(input)
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Side {
    Current,
    Other,
    /// A special marker that is just used to be able to mix-in hunks that only point to the ancestor.
    /// Only `before` matters then.
    Ancestor,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub before: Range<u32>,
    pub after: Range<u32>,
    pub side: Side,
}

pub fn collect_hunks(
    algorithm: imara_diff::Algorithm,
    input: &imara_diff::InternedInput<&[u8]>,
    side: Side,
    mut hunks: Vec<Hunk>,
) -> Vec<Hunk> {
    let mut diff = imara_diff::Diff::compute(algorithm, input);
    diff.postprocess_lines(input);
    hunks.extend(diff.hunks().map(|hunk| Hunk {
        before: hunk.before,
        after: hunk.after,
        side,
    }));
    hunks
}

const INSERT_ORDER_PREFIX: &[u8] = b"[gix-merge-insert-order:";

/// Parse a standalone `[gix-merge-insert-order: <i32>]` directive line.
///
/// Any surrounding comment syntax made of ASCII whitespace and non-quote ASCII punctuation
/// (such as `//`, `#`, `--`, `;`, `/* ... */`, `<!-- ... -->`, `{/* ... */}`) is accepted.
pub fn parse_insert_order_line(line: &[u8]) -> Option<i32> {
    let trimmed = line.trim_ascii();
    let start = trimmed.find(INSERT_ORDER_PREFIX)?;
    let rest = &trimmed[start + INSERT_ORDER_PREFIX.len()..];
    let end = rest.find_byte(b']')?;
    let prefix = &trimmed[..start];
    let inner = &rest[..end];
    let suffix = &rest[end + 1..];

    let is_comment_wrapper_byte =
        |b: u8| b.is_ascii_whitespace() || (b.is_ascii_punctuation() && !matches!(b, b'"' | b'\'' | b'`'));
    if !prefix.iter().copied().all(is_comment_wrapper_byte) || !suffix.iter().copied().all(is_comment_wrapper_byte) {
        return None;
    }

    std::str::from_utf8(inner.trim_ascii()).ok()?.parse::<i32>().ok()
}

/// Split `num_lines` lines into `(line_range, explicit_order)` segments based on
/// `[gix-merge-insert-order: <i32>]` directives.
///
/// `line_at(idx)` returns the raw bytes of line `idx` (`0..num_lines`).
/// Each returned tuple is `(start..end, Some(order))` for a directive-controlled segment
/// or `(start..end, None)` for an unmarked segment (whose implicit merge order is `0`).
pub fn split_ordered_line_ranges<'a>(
    num_lines: usize,
    mut line_at: impl FnMut(usize) -> &'a [u8],
) -> Vec<(Range<usize>, Option<i32>)> {
    if num_lines == 0 {
        return Vec::new();
    }
    let mut directives = Vec::new();
    for idx in 0..num_lines {
        if let Some(order) = parse_insert_order_line(line_at(idx)) {
            directives.push((idx, order));
        }
    }

    if directives.is_empty() {
        return vec![(0..num_lines, None)];
    }

    let mut segments = Vec::with_capacity(directives.len() + 1);
    let (first_idx, first_order) = directives[0];
    let has_non_blank_before_first = (0..first_idx).any(|idx| !line_at(idx).trim_ascii().is_empty());

    let first_seg_start = if has_non_blank_before_first {
        segments.push((0..first_idx, None));
        first_idx
    } else {
        0
    };

    let first_seg_end = directives.get(1).map_or(num_lines, |(idx, _)| *idx);
    segments.push((first_seg_start..first_seg_end, Some(first_order)));

    for (i, &(dir_idx, dir_order)) in directives.iter().enumerate().skip(1) {
        let seg_end = directives.get(i + 1).map_or(num_lines, |(idx, _)| *idx);
        segments.push((dir_idx..seg_end, Some(dir_order)));
    }

    segments
}

struct OrderedSegment {
    order: i32,
    explicit: bool,
    hunk: Hunk,
}

fn split_ordered_segments(
    hunk: &Hunk,
    input: &imara_diff::InternedInput<&[u8]>,
    current_tokens: &[imara_diff::Token],
) -> Vec<OrderedSegment> {
    let tokens = tokens_for_side(hunk.side, input, current_tokens);
    let base_idx = hunk.after.start as usize;
    let num_lines = (hunk.after.end - hunk.after.start) as usize;
    split_ordered_line_ranges(num_lines, |rel_idx| input.interner[tokens[base_idx + rel_idx]])
        .into_iter()
        .map(|(range, explicit_order)| OrderedSegment {
            order: explicit_order.unwrap_or(0),
            explicit: explicit_order.is_some(),
            hunk: Hunk {
                before: hunk.before.clone(),
                after: (hunk.after.start + range.start as u32)..(hunk.after.start + range.end as u32),
                side: hunk.side,
            },
        })
        .collect()
}

fn hunks_tokens_equal(
    a: &Hunk,
    b: &Hunk,
    input: &imara_diff::InternedInput<&[u8]>,
    current_tokens: &[imara_diff::Token],
) -> bool {
    let a_tokens = &tokens_for_side(a.side, input, current_tokens)[usize_range(&a.after)];
    let b_tokens = &tokens_for_side(b.side, input, current_tokens)[usize_range(&b.after)];
    a_tokens == b_tokens
}

fn range_has_insert_order(
    tokens: &[imara_diff::Token],
    range: &Range<u32>,
    interner: &imara_diff::Interner<&[u8]>,
) -> bool {
    tokens[usize_range(range)]
        .iter()
        .any(|t| parse_insert_order_line(interner[*t]).is_some())
}

pub fn try_resolve_ordered_hunks(
    our_hunks: &[Hunk],
    their_hunks: &[Hunk],
    input: &imara_diff::InternedInput<&[u8]>,
    current_tokens: &[imara_diff::Token],
    ancestor_integrated_until: &mut u32,
    out: &mut Vec<u8>,
) -> bool {
    // Case 1: Both sides are pure insertions at the same base line `k`.
    if our_hunks.len() == 1 && their_hunks.len() == 1 {
        let our_hunk = &our_hunks[0];
        let their_hunk = &their_hunks[0];
        if our_hunk.before.is_empty()
            && their_hunk.before.is_empty()
            && our_hunk.before.start == their_hunk.before.start
            && !our_hunk.after.is_empty()
            && !their_hunk.after.is_empty()
        {
            let mut segments = split_ordered_segments(our_hunk, input, current_tokens);
            segments.extend(split_ordered_segments(their_hunk, input, current_tokens));
            if segments.iter().any(|s| s.explicit) {
                segments.sort_by_key(|s| s.order);
                let mut deduped: Vec<OrderedSegment> = Vec::with_capacity(segments.len());
                let mut tie_conflict = false;
                for seg in segments {
                    if let Some(prev) = deduped.last()
                        && prev.order == seg.order
                    {
                        if hunks_tokens_equal(&prev.hunk, &seg.hunk, input, current_tokens) {
                            continue;
                        }
                        tie_conflict = true;
                        break;
                    }
                    deduped.push(seg);
                }
                if !tie_conflict {
                    let k = our_hunk.before.start;
                    write_ancestor(input, *ancestor_integrated_until, k as usize, out);
                    *ancestor_integrated_until = k;
                    let nl = detect_line_ending_or_nl(our_hunks, input, current_tokens);
                    for (idx, seg) in deduped.iter().enumerate() {
                        if idx > 0 {
                            assure_ends_with_nl(out, nl);
                        }
                        write_hunks(std::slice::from_ref(&seg.hunk), input, current_tokens, out);
                    }
                    return true;
                }
            }
        }
    }

    // Case 2: One side is a pure deletion `del_start..del_end -> empty`, and the other side is
    // one or more pure insertions within `del_start..=del_end`, where either the insertion or the
    // deleted range carries `[gix-merge-insert-order: ...]`.
    let maybe_del_ins = if our_hunks.len() == 1 && !our_hunks[0].before.is_empty() && our_hunks[0].after.is_empty() {
        Some((&our_hunks[0], their_hunks))
    } else if their_hunks.len() == 1 && !their_hunks[0].before.is_empty() && their_hunks[0].after.is_empty() {
        Some((&their_hunks[0], our_hunks))
    } else {
        None
    };

    if let Some((del_hunk, ins_hunks)) = maybe_del_ins
        && !ins_hunks.is_empty()
        && ins_hunks.iter().all(|h| {
            h.before.is_empty()
                && !h.after.is_empty()
                && del_hunk.before.start <= h.before.start
                && h.before.start <= del_hunk.before.end
        })
    {
        let has_order_marker = range_has_insert_order(&input.before, &del_hunk.before, &input.interner)
            || ins_hunks.iter().any(|h| {
                let side_tokens = tokens_for_side(h.side, input, current_tokens);
                range_has_insert_order(side_tokens, &h.after, &input.interner)
            });
        if has_order_marker {
            let mut del_start = del_hunk.before.start;
            let mut del_end = del_hunk.before.end;
            let first_ins_start = ins_hunks[0].before.start;
            while del_start < first_ins_start
                && input.before.get(del_end as usize) == input.before.get(del_start as usize)
            {
                del_start += 1;
                del_end += 1;
            }
            write_ancestor(input, *ancestor_integrated_until, del_start as usize, out);
            let nl = detect_line_ending_or_nl(ins_hunks, input, current_tokens);
            for (idx, ins_hunk) in ins_hunks.iter().enumerate() {
                if idx > 0 {
                    assure_ends_with_nl(out, nl);
                }
                let ins_tokens = tokens_for_side(ins_hunk.side, input, current_tokens);
                let mut ins_before = ins_hunk.before.start;
                let mut a_start = ins_hunk.after.start;
                let a_end = ins_hunk.after.end;
                while ins_before < del_end
                    && a_start < a_end
                    && ins_tokens.get(a_start as usize) == input.before.get(ins_before as usize)
                {
                    a_start += 1;
                    ins_before += 1;
                }
                if a_start < a_end {
                    let main_part = Hunk {
                        before: ins_hunk.before.clone(),
                        after: a_start..a_end,
                        side: ins_hunk.side,
                    };
                    write_hunks(std::slice::from_ref(&main_part), input, current_tokens, out);
                }
                if ins_hunk.after.start < a_start {
                    let rotated_part = Hunk {
                        before: ins_hunk.before.clone(),
                        after: ins_hunk.after.start..a_start,
                        side: ins_hunk.side,
                    };
                    write_hunks(std::slice::from_ref(&rotated_part), input, current_tokens, out);
                }
            }
            *ancestor_integrated_until = del_end;
            return true;
        }
    }

    false
}

#[cfg(test)]
mod insert_order_tests {
    use super::parse_insert_order_line;
    use crate::blob::{
        Resolution, builtin_driver,
        builtin_driver::text::{Conflict, ConflictStyle, Labels, Options},
    };
    use imara_diff::InternedInput;

    fn run_merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> (Resolution, String) {
        let labels = Labels {
            ancestor: Some("base".into()),
            current: Some("ours".into()),
            other: Some("theirs".into()),
        };
        let options = Options {
            diff_algorithm: imara_diff::Algorithm::Myers,
            conflict: Conflict::Keep {
                style: ConflictStyle::Merge,
                marker_size: 7.try_into().unwrap(),
            },
        };
        let mut out = Vec::new();
        let mut input = InternedInput::default();
        let res = builtin_driver::text(&mut out, &mut input, labels, ours, base, theirs, options);
        (res, String::from_utf8(out).unwrap())
    }

    #[test]
    fn parse_insert_order_line_is_language_agnostic_and_rejects_code_or_quotes() {
        assert_eq!(
            parse_insert_order_line(b"  // [gix-merge-insert-order: -10]\n"),
            Some(-10)
        );
        assert_eq!(parse_insert_order_line(b"# [gix-merge-insert-order: 20]\r\n"), Some(20));
        assert_eq!(
            parse_insert_order_line(b"<!-- [gix-merge-insert-order: 5] -->\n"),
            Some(5)
        );
        assert_eq!(parse_insert_order_line(b"/* [gix-merge-insert-order: 0] */\n"), Some(0));
        assert_eq!(
            parse_insert_order_line(b"{/* [gix-merge-insert-order: 42] */}\n"),
            Some(42)
        );
        assert_eq!(
            parse_insert_order_line(b"-- [gix-merge-insert-order: 100]\n"),
            Some(100)
        );

        // Rejects string literals and code on the same line
        assert_eq!(
            parse_insert_order_line(b"let s = \"[gix-merge-insert-order: 10]\";\n"),
            None
        );
        assert_eq!(
            parse_insert_order_line(b"foo(); // [gix-merge-insert-order: 10]\n"),
            None
        );
    }

    #[test]
    fn forward_merge_orders_marked_insertion_against_unmarked_default_zero() {
        let base = b"header\nfooter\n";
        let unmarked = b"header\nline_default_0\nfooter\n";
        let marked_before = b"header\n// [gix-merge-insert-order: -10]\nline_before\nfooter\n";
        let marked_after = b"header\n// [gix-merge-insert-order: 10]\nline_after\nfooter\n";

        let (res1, out1) = run_merge(base, unmarked, marked_before);
        assert_eq!(res1, Resolution::Complete);
        assert_eq!(
            out1,
            "header\n// [gix-merge-insert-order: -10]\nline_before\nline_default_0\nfooter\n"
        );

        let (res2, out2) = run_merge(base, unmarked, marked_after);
        assert_eq!(res2, Resolution::Complete);
        assert_eq!(
            out2,
            "header\nline_default_0\n// [gix-merge-insert-order: 10]\nline_after\nfooter\n"
        );
    }

    #[test]
    fn forward_merge_chains_three_parallel_insertions_commutatively() {
        let base = b"start\nend\n";
        let branch_a = b"start\nblock_a\nend\n"; // implicit order 0
        let branch_b = b"start\n# [gix-merge-insert-order: -5]\nblock_b\nend\n";
        let branch_c = b"start\n# [gix-merge-insert-order: 5]\nblock_c\nend\n";

        // Merge A and C first, then B
        let (res_ac, merged_ac) = run_merge(base, branch_a, branch_c);
        assert_eq!(res_ac, Resolution::Complete);
        let (res_acb, merged_acb) = run_merge(base, merged_ac.as_bytes(), branch_b);
        assert_eq!(res_acb, Resolution::Complete);

        // Merge B and C first, then A
        let (res_bc, merged_bc) = run_merge(base, branch_b, branch_c);
        assert_eq!(res_bc, Resolution::Complete);
        let (res_bca, merged_bca) = run_merge(base, merged_bc.as_bytes(), branch_a);
        assert_eq!(res_bca, Resolution::Complete);

        let expected =
            "start\n# [gix-merge-insert-order: -5]\nblock_b\nblock_a\n# [gix-merge-insert-order: 5]\nblock_c\nend\n";
        assert_eq!(merged_acb, expected);
        assert_eq!(merged_bca, expected);
    }

    #[test]
    fn forward_merge_conflicts_on_distinct_hunks_with_same_order_and_deduplicates_identical() {
        let base = b"start\nend\n";
        let branch_1 = b"start\n// [gix-merge-insert-order: 10]\nalpha\nend\n";
        let branch_2 = b"start\n// [gix-merge-insert-order: 10]\nbeta\nend\n";

        let (res_conflict, _) = run_merge(base, branch_1, branch_2);
        assert_eq!(res_conflict, Resolution::Conflict);

        let (res_same, out_same) = run_merge(base, branch_1, branch_1);
        assert_eq!(res_same, Resolution::Complete);
        assert_eq!(out_same, "start\n// [gix-merge-insert-order: 10]\nalpha\nend\n");
    }

    #[test]
    fn reverse_cherry_pick_resolves_insertion_adjacent_to_deleted_other_branches() {
        // Simulates `but_core::tree::create_tree`:
        // base = HEAD^{tree} (has branch_a and branch_b)
        // ours = target_tree (base without branch_a and branch_b)
        // theirs = worktree (HEAD^{tree} + new branch_c with insert-order marker and trailing `}`)
        let head_tree = b"fn main() {\n    {\n        branch_a();\n    }\n    {\n        branch_b();\n    }\n}\n";
        let target_tree = b"fn main() {\n}\n";
        let worktree = b"fn main() {\n    {\n        branch_a();\n    }\n    {\n        branch_b();\n    }\n    // [gix-merge-insert-order: 10]\n    {\n        branch_c();\n    }\n}\n";

        let (res, out) = run_merge(head_tree, target_tree, worktree);
        assert_eq!(res, Resolution::Complete);
        assert_eq!(
            out,
            "fn main() {\n    // [gix-merge-insert-order: 10]\n    {\n        branch_c();\n    }\n}\n"
        );
    }
}
