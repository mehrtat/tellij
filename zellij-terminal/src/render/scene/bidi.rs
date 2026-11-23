//! Right-to-left support for one rendered row.
//!
//! The grid stores text in logical order, one character per cell. Before a
//! row is drawn, every pane segment that holds Arabic-script or Hebrew text
//! is reordered into visual order with the Unicode Bidirectional Algorithm
//! (UAX #9), and Arabic-script letters are replaced by their contextual
//! presentation forms so they join. Cells move as whole units (wide
//! characters and grapheme groups stay together), and every cell keeps its
//! logical column in `RowCell::source` for cluster, cursor and link lookups.

use unicode_bidi::{bidi_class, BidiClass, BidiInfo, Level};

use super::runs::region_of;
use super::{RowCell, RowContext};
use crate::emulation::screen_buffer::Occupancy;
use crate::emulation::terminal::TerminalState;

const ZWNJ: char = '\u{200c}';
const ZWJ: char = '\u{200d}';
const TATWEEL: char = '\u{640}';

pub(super) fn reorder_row(context: &RowContext<'_>, row: usize, cells: &mut [RowCell]) {
    let geometry = context.state.geometry();
    let segments: Vec<Option<usize>> = cells
        .iter()
        .enumerate()
        .map(|(col, entry)| {
            (!is_wall(entry.cell.character()))
                .then(|| region_of(geometry, row, col))
                .flatten()
        })
        .collect();
    let mut start = 0;
    while start < cells.len() {
        let region = segments[start];
        let mut end = start + 1;
        while end < cells.len() && segments[end] == region {
            end += 1;
        }
        if region.is_some() {
            reorder_segment(context, row, &mut cells[start..end]);
        }
        start = end;
    }
}

/// Box-drawing and block characters fence a segment: a frame, a sidebar
/// border or a table column rule stays where it is, and the text on each
/// side of it is reordered on its own.
fn is_wall(c: char) -> bool {
    matches!(c, '\u{2500}'..='\u{259f}')
}

struct Unit {
    start: usize,
    end: usize,
    base: char,
    breaks_join: bool,
    plain: bool,
}

impl Unit {
    fn of(start: usize, end: usize, base: char, cluster: Option<&str>) -> Self {
        match cluster {
            Some(text) => {
                let mut chars = text.chars();
                let base = chars.next().unwrap_or(' ');
                let rest: Vec<char> = chars.collect();
                Unit {
                    start,
                    end,
                    base,
                    breaks_join: rest.contains(&ZWNJ),
                    plain: rest.iter().all(|c| *c == ZWNJ || *c == ZWJ),
                }
            },
            None => Unit {
                start,
                end,
                base,
                breaks_join: false,
                plain: true,
            },
        }
    }
}

/// Groups `len` cells into units (a cell for which `joins_previous` holds
/// belongs to the unit before it) and reports whether any unit is
/// right-to-left.
fn units_of(
    len: usize,
    joins_previous: impl Fn(usize) -> bool,
    unit: impl Fn(usize, usize) -> Unit,
) -> Option<Vec<Unit>> {
    let mut units = Vec::new();
    let mut any_rtl = false;
    let mut col = 0;
    while col < len {
        let mut end = col + 1;
        while end < len && joins_previous(end) {
            end += 1;
        }
        let made = unit(col, end);
        any_rtl |= is_rtl_char(made.base);
        units.push(made);
        col = end;
    }
    any_rtl.then_some(units)
}

/// The logical column drawn at visual column `col` of `row`, so a click or a
/// drag on reordered right-to-left text lands on the cell the eye sees.
pub fn logical_column(state: &TerminalState, row: usize, col: usize) -> usize {
    let size = state.size();
    if row >= size.rows || col >= size.cols {
        return col;
    }
    let geometry = state.geometry();
    let screen = state.screen();
    let segment_of = |at: usize| {
        (!is_wall(screen.cell(row, at).character()))
            .then(|| region_of(geometry, row, at))
            .flatten()
    };
    let region = segment_of(col);
    if region.is_none() {
        return col;
    }
    let mut start = col;
    while start > 0 && segment_of(start - 1) == region {
        start -= 1;
    }
    let mut end = col + 1;
    while end < size.cols && segment_of(end) == region {
        end += 1;
    }
    let units = units_of(
        end - start,
        |at| {
            let occupancy = state.occupancy(row, start + at);
            occupancy == Occupancy::WideTail
                || screen.cell(row, start + at).is_group_continuation()
        },
        |from, to| {
            let cluster = screen.cluster_at(row, start + from);
            let base = screen.cell(row, start + from).character();
            Unit::of(from, to, base, cluster.map(|text| &**text))
        },
    );
    let Some(units) = units else {
        return col;
    };
    let bases: Vec<char> = units.iter().map(|unit| unit.base).collect();
    let (order, _) = visual_order(&bases);
    let mut at = start;
    for index in order {
        let unit = &units[index];
        let width = unit.end - unit.start;
        if col < at + width {
            return start + unit.start + (col - at);
        }
        at += width;
    }
    col
}

fn reorder_segment(context: &RowContext<'_>, row: usize, cells: &mut [RowCell]) {
    let units = units_of(
        cells.len(),
        |at| cells[at].occupancy == Occupancy::WideTail || cells[at].continuation,
        |from, to| {
            let head = &cells[from];
            let cluster = head
                .cluster
                .then(|| context.cluster_at(row, head.source))
                .flatten();
            Unit::of(from, to, head.cell.character(), cluster.as_deref())
        },
    );
    let Some(units) = units else {
        return;
    };

    let bases: Vec<char> = units.iter().map(|unit| unit.base).collect();
    let breaks: Vec<bool> = units.iter().map(|unit| unit.breaks_join).collect();
    let forms = joining_forms(&bases, &breaks);
    for (unit, form) in units.iter().zip(&forms) {
        let head = &mut cells[unit.start];
        if let Some(form) = form {
            if unit.plain && !head.composing && !head.held {
                head.cell.ch = *form as u32;
                head.cluster = false;
            }
        }
    }

    let (order, rtl) = visual_order(&bases);
    let logical: Vec<RowCell> = cells.to_vec();
    let mut at = 0;
    for index in order {
        let unit = &units[index];
        let head_at = at;
        for cell in &logical[unit.start..unit.end] {
            cells[at] = *cell;
            at += 1;
        }
        let head = &mut cells[head_at];
        if rtl[index] && !head.cluster {
            if let Some(mirrored) = mirror(head.cell.character()) {
                head.cell.ch = mirrored as u32;
            }
        }
    }
}

/// Visual position to logical unit index, and whether each logical unit
/// sits on a right-to-left embedding level.
pub(super) fn visual_order(bases: &[char]) -> (Vec<usize>, Vec<bool>) {
    let text: String = bases
        .iter()
        .map(|c| if c.is_control() || *c == '\u{2029}' { ' ' } else { *c })
        .collect();
    let info = BidiInfo::new(&text, Some(paragraph_level(bases)));
    let Some(paragraph) = info.paragraphs.first() else {
        return ((0..bases.len()).collect(), vec![false; bases.len()]);
    };
    let levels: Vec<Level> = info.reordered_levels_per_char(paragraph, paragraph.range.clone());
    let order = BidiInfo::reorder_visual(&levels);
    let rtl = levels.iter().map(|level| level.is_rtl()).collect();
    (order, rtl)
}

/// A row is a right-to-left paragraph when it has at least as many
/// right-to-left letters as left-to-right ones, so a Persian sentence that
/// opens with a Latin word still reads from the right.
fn paragraph_level(bases: &[char]) -> Level {
    let (mut rtl, mut ltr) = (0usize, 0usize);
    for c in bases {
        match bidi_class(*c) {
            BidiClass::R | BidiClass::AL => rtl += 1,
            BidiClass::L => ltr += 1,
            _ => {},
        }
    }
    if rtl > 0 && rtl >= ltr {
        Level::rtl()
    } else {
        Level::ltr()
    }
}

pub(super) fn is_rtl_char(c: char) -> bool {
    matches!(c,
        '\u{590}'..='\u{8ff}'
        | '\u{fb1d}'..='\u{fdff}'
        | '\u{fe70}'..='\u{feff}'
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Joining {
    Dual,
    Right,
}

/// Isolated, final, initial and medial forms. Right-joining letters have no
/// initial or medial form.
fn forms_of(c: char) -> Option<(Joining, [char; 4])> {
    use Joining::{Dual, Right};
    let dual = |base: u32| {
        let at = |n: u32| char::from_u32(base + n).unwrap_or(c);
        Some((Dual, [at(0), at(1), at(2), at(3)]))
    };
    let right = |base: u32| {
        let at = |n: u32| char::from_u32(base + n).unwrap_or(c);
        Some((Right, [at(0), at(1), at(0), at(1)]))
    };
    match c {
        '\u{622}' => right(0xfe81),
        '\u{623}' => right(0xfe83),
        '\u{624}' => right(0xfe85),
        '\u{625}' => right(0xfe87),
        '\u{626}' => dual(0xfe89),
        '\u{627}' => right(0xfe8d),
        '\u{628}' => dual(0xfe8f),
        '\u{629}' => right(0xfe93),
        '\u{62a}' => dual(0xfe95),
        '\u{62b}' => dual(0xfe99),
        '\u{62c}' => dual(0xfe9d),
        '\u{62d}' => dual(0xfea1),
        '\u{62e}' => dual(0xfea5),
        '\u{62f}' => right(0xfea9),
        '\u{630}' => right(0xfeab),
        '\u{631}' => right(0xfead),
        '\u{632}' => right(0xfeaf),
        '\u{633}' => dual(0xfeb1),
        '\u{634}' => dual(0xfeb5),
        '\u{635}' => dual(0xfeb9),
        '\u{636}' => dual(0xfebd),
        '\u{637}' => dual(0xfec1),
        '\u{638}' => dual(0xfec5),
        '\u{639}' => dual(0xfec9),
        '\u{63a}' => dual(0xfecd),
        '\u{641}' => dual(0xfed1),
        '\u{642}' => dual(0xfed5),
        '\u{643}' => dual(0xfed9),
        '\u{644}' => dual(0xfedd),
        '\u{645}' => dual(0xfee1),
        '\u{646}' => dual(0xfee5),
        '\u{647}' => dual(0xfee9),
        '\u{648}' => right(0xfeed),
        '\u{649}' => right(0xfeef),
        '\u{64a}' => dual(0xfef1),
        '\u{67e}' => dual(0xfb56),
        '\u{686}' => dual(0xfb7a),
        '\u{698}' => right(0xfb8a),
        '\u{6a9}' => dual(0xfb8e),
        '\u{6af}' => dual(0xfb92),
        '\u{6c0}' => right(0xfba4),
        '\u{6cc}' => dual(0xfbfc),
        _ => None,
    }
}

fn joins_following(c: char) -> bool {
    c == TATWEEL || c == ZWJ || matches!(forms_of(c), Some((Joining::Dual, _)))
}

fn joins_preceding(c: char) -> bool {
    c == TATWEEL || c == ZWJ || forms_of(c).is_some()
}

/// The presentation form of every unit, in logical order. `breaks[i]` is set
/// when a zero-width non-joiner follows unit `i`.
pub(super) fn joining_forms(bases: &[char], breaks: &[bool]) -> Vec<Option<char>> {
    (0..bases.len())
        .map(|index| {
            let (joining, forms) = forms_of(bases[index])?;
            let after_joiner =
                index > 0 && !breaks[index - 1] && joins_following(bases[index - 1]);
            let before_joiner = joining == Joining::Dual
                && !breaks[index]
                && bases.get(index + 1).is_some_and(|next| joins_preceding(*next));
            Some(match (after_joiner, before_joiner) {
                (false, false) => forms[0],
                (true, false) => forms[1],
                (false, true) => forms[2],
                (true, true) => forms[3],
            })
        })
        .collect()
}

fn mirror(c: char) -> Option<char> {
    Some(match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        '‹' => '›',
        '›' => '‹',
        '❮' => '❯',
        '❯' => '❮',
        '❰' => '❱',
        '❱' => '❰',
        '⟨' => '⟩',
        '⟩' => '⟨',
        '→' => '←',
        '←' => '→',
        '⇒' => '⇐',
        '⇐' => '⇒',
        '▶' => '◀',
        '◀' => '▶',
        '►' => '◄',
        '◄' => '►',
        '⏵' => '⏴',
        '⏴' => '⏵',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    fn visual(text: &str) -> String {
        let bases = chars(text);
        let (order, _) = visual_order(&bases);
        order.iter().map(|index| bases[*index]).collect()
    }

    #[test]
    fn latin_text_keeps_its_order() {
        assert_eq!(visual("hello world"), "hello world");
    }

    #[test]
    fn a_persian_line_reads_right_to_left_and_right_aligns() {
        // Trailing spaces of a right-to-left paragraph move to the left edge.
        assert_eq!(visual("سلام  "), "  مالس");
    }

    #[test]
    fn a_latin_word_inside_persian_keeps_its_own_order() {
        assert_eq!(visual("از api است"), "تسا api زا");
    }

    #[test]
    fn a_persian_line_after_a_neutral_bullet_is_right_to_left() {
        assert_eq!(visual("● باز "), " زاب ●");
    }

    #[test]
    fn a_persian_sentence_opening_with_a_latin_word_is_right_to_left() {
        assert_eq!(visual("Session قبلی فقط یک  "), "  کی طقف یلبق Session");
    }

    #[test]
    fn a_latin_line_with_one_persian_word_stays_left_to_right() {
        assert_eq!(visual("echo the word سلام "), "echo the word مالس ");
    }

    #[test]
    fn a_prompt_arrow_points_the_other_way_in_a_right_to_left_row() {
        assert_eq!(mirror('❯'), Some('❮'));
        assert_eq!(mirror('→'), Some('←'));
    }

    #[test]
    fn letters_take_joining_forms() {
        let bases = chars("سلام");
        let forms = joining_forms(&bases, &[false; 4]);
        assert_eq!(
            forms,
            vec![Some('\u{feb3}'), Some('\u{fee0}'), Some('\u{fe8e}'), Some('\u{fee1}')]
        );
    }

    #[test]
    fn a_non_joiner_breaks_the_join() {
        // «می‌شود»: the non-joiner after «ی» leaves it final, «ش» initial.
        let bases = chars("میشود");
        let mut breaks = vec![false; bases.len()];
        breaks[1] = true;
        let forms = joining_forms(&bases, &breaks);
        assert_eq!(forms[1], Some('\u{fbfd}'));
        assert_eq!(forms[2], Some('\u{feb7}'));
    }

    #[test]
    fn persian_letters_join() {
        let forms = joining_forms(&chars("پگ"), &[false; 2]);
        assert_eq!(forms, vec![Some('\u{fb58}'), Some('\u{fb93}')]);
    }

    #[test]
    fn a_rendered_persian_row_places_its_latin_word_left_of_the_persian() {
        use crate::render::atlas::Lookup;
        use crate::render::scene::test_helpers::build;
        use crate::render::scene::tests::cache;
        use crate::render::scene::FaceStyle;
        use crate::testing::painter::Painter;

        let mut cache = cache();
        let width = cache.metrics().width as i32;
        let state = Painter::state(1, 10, |painter| painter.text(0, 0, "سلام ab"));
        let glyphs = build(&state, &mut cache).glyphs;
        let mut column_of = |c: char| {
            let Lookup::Rendered(rendered) = cache.glyph(FaceStyle::Regular, c, 1) else {
                panic!("{c:?} did not rasterize");
            };
            glyphs
                .iter()
                .find(|quad| quad.entry == rendered.entry)
                .map(|quad| quad.x / width)
        };
        // Logical «سلام ab»: the paragraph is right-to-left, so the three
        // trailing blanks go to the left edge and «ab» follows them.
        assert_eq!(column_of('a'), Some(3));
        assert_eq!(column_of('b'), Some(4));
    }

    #[test]
    fn a_click_on_reordered_text_maps_to_the_cell_drawn_there() {
        use crate::testing::painter::Painter;

        let state = Painter::state(1, 10, |painter| painter.text(0, 0, "سلام ab"));
        // Drawn: three blanks, «ab», a blank, then «سلام» right to left.
        assert_eq!(logical_column(&state, 0, 3), 5);
        assert_eq!(logical_column(&state, 0, 4), 6);
        assert_eq!(logical_column(&state, 0, 9), 0);
        assert_eq!(logical_column(&state, 0, 0), 9);

        let latin = Painter::state(1, 10, |painter| painter.text(0, 0, "hello"));
        assert_eq!(logical_column(&latin, 0, 3), 3);
    }

    #[test]
    fn a_frame_border_stays_put_and_each_side_reorders_alone() {
        use crate::testing::painter::Painter;

        // «ab │سلام  »: the bar stays at column 3, the Persian is
        // right-aligned between the bar and the row end.
        let state = Painter::state(1, 10, |painter| painter.text(0, 0, "ab │سلام"));
        assert_eq!(logical_column(&state, 0, 3), 3);
        assert_eq!(logical_column(&state, 0, 0), 0);
        assert_eq!(logical_column(&state, 0, 9), 4);
        assert_eq!(logical_column(&state, 0, 4), 9);
    }

    #[test]
    fn a_right_joining_letter_never_joins_forward() {
        let forms = joining_forms(&chars("دب"), &[false; 2]);
        assert_eq!(forms, vec![Some('\u{fea9}'), Some('\u{fe8f}')]);
    }
}
