//! The shared box frame: the bordered layout the report commands render in.
//!
//! May not depend on:
//! - provider adapters
//! - the store or the calibration module
//!
//! A box is drawn at one width for every line: the style layer's terminal
//! width clamped to the frame's own minimum and maximum, so a report never
//! wraps a row to fit and never runs past a narrow terminal. The frame owns
//! only the rails and the padding; callers paint their own text and hand the
//! finished line over, because which words are bold or dim is the renderer's
//! decision, not the frame's.
//!
//! Padding measures display width, not bytes: a caller may paint part of a
//! line, and the escapes a painted segment carries must not push the closing
//! rail out of alignment.

use crate::presentation::style::Style;

/// The narrowest box the frame draws. A table narrower than this reads as a
/// strip rather than a panel; the width floor keeps one-line reports present
/// on a terminal of any size.
pub const BOXED_MIN_WIDTH: usize = 80;

/// The widest box the frame draws. Beyond this a table's columns spread past
/// what an operator scans in one glance, so a wide terminal is met at 120 and
/// the padding absorbs the rest.
pub const BOXED_MAX_WIDTH: usize = 120;

/// The box width a report renders at: the style layer's measured width held
/// between the frame's floor and ceiling. The one place the frame touches the
/// style layer; every other function takes the width that came out of this
/// one.
pub fn boxed_width(style: &Style) -> usize {
    clamped(style.width())
}

/// The clamp behind [`boxed_width`], split out so both ends of the range are
/// testable without a terminal to measure.
fn clamped(width: u16) -> usize {
    usize::from(width.clamp(BOXED_MIN_WIDTH as u16, BOXED_MAX_WIDTH as u16))
}

/// The columns a body line's content may occupy inside a box of `width`:
/// between the corner glyphs, the two rail padding columns on each side are
/// the frame's own.
pub fn boxed_content_area(width: usize) -> usize {
    width.saturating_sub(6)
}

/// The display length of text that may carry paint escapes: an escape
/// sequence is invisible on the terminal, so it occupies none of the width
/// the padding must fill. The escape character is computed rather than
/// spelled so this file stays out of the business of writing escape strings;
/// it only skips over the ones a caller's paint produced.
fn boxed_display_len(text: &str) -> usize {
    let escape = char::from_u32(0x1b).expect("0x1b is a valid Unicode scalar");
    let mut length = 0;
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character == escape {
            for painted in characters.by_ref() {
                if painted == 'm' {
                    break;
                }
            }
        } else {
            length += 1;
        }
    }
    length
}

/// The box's top rail with the title inset: `┌─ title ────┐`, the dashes
/// carrying the line out to the full width. A title longer than the box
/// keeps one dash of separation rather than disappearing.
pub fn boxed_top(title: &str, width: usize) -> String {
    let dashes = width
        .saturating_sub(3 + boxed_display_len(title) + 2)
        .max(1);
    format!("┌─ {title} {}┐", "─".repeat(dashes))
}

/// One body line: the content behind a two-column rail padding on each side,
/// padded out to the box width. Content longer than the content area is kept
/// whole and the line grows: clipping would report a number the renderer did
/// not render, and the frame does not wrap, because wrapping is a decision
/// about the content that only the caller can make.
pub fn boxed_body(content: &str, width: usize) -> String {
    let area = boxed_content_area(width);
    let padding = area.saturating_sub(boxed_display_len(content));
    format!("│  {content}{}  │", " ".repeat(padding))
}

/// An empty body line.
pub fn boxed_blank(width: usize) -> String {
    boxed_body("", width)
}

/// A horizontal rule `rule_width` columns wide, laid inside the box as a body
/// line: the separator a table draws under its header row.
pub fn boxed_rule(rule_width: usize, width: usize) -> String {
    boxed_body(&"─".repeat(rule_width), width)
}

/// The box's bottom rail.
pub fn boxed_bottom(width: usize) -> String {
    format!("└{}┘", "─".repeat(width.saturating_sub(2)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame at the default width: every line is the same length, the
    /// title sits inset on the top rail and the body content is padded
    /// between the rails. The planted negative is the blank line: a body
    /// line with no content must still carry both rails, so a renderer that
    /// forgets the padding on empty lines cannot pass while the filled lines
    /// do.
    #[test]
    fn the_frame_draws_every_line_at_one_width() {
        let width = 80;
        let top = boxed_top("coverage · last 24h", width);
        let body = boxed_body("account  attempts", width);
        let blank = boxed_blank(width);
        let bottom = boxed_bottom(width);
        assert_eq!(top.chars().count(), width, "{top}");
        assert_eq!(body.chars().count(), width, "{body}");
        assert_eq!(blank.chars().count(), width, "{blank}");
        assert_eq!(bottom.chars().count(), width, "{bottom}");
        assert!(top.starts_with("┌─ coverage · last 24h "));
        assert!(top.ends_with('┐'));
        assert_eq!(
            body,
            "│  account  attempts                                                           │"
        );
        assert_eq!(blank, format!("│{}│", " ".repeat(width - 2)));
        assert!(bottom.starts_with('└') && bottom.ends_with('┘'));
    }

    /// A painted segment occupies its text width, not its byte length: the
    /// escape around a bold title or account name must not push the closing
    /// rail right. The painted and unpainted lines have the same display
    /// width, so the rails line up down the box.
    #[test]
    fn paint_escapes_do_not_shift_the_padding() {
        let coloured = Style::new(true, false, false);
        let painted_title = coloured.paint(coloured.bold(), "coverage");
        let painted = boxed_top(&painted_title, 80);
        let plain = boxed_top("coverage", 80);
        assert_eq!(boxed_display_len(&painted), boxed_display_len(&plain));
        assert_eq!(
            boxed_display_len(&boxed_body(&painted_title, 80)),
            boxed_display_len(&boxed_body("coverage", 80))
        );
        assert_ne!(painted, plain, "the paint must reach the frame");
    }

    /// Content longer than the content area is kept whole: the line grows
    /// rather than clipping a number the renderer did render. The negative is
    /// the fitting line beside it, which stays at the box width.
    #[test]
    fn overlong_content_grows_the_line_instead_of_clipping() {
        let long = "a".repeat(boxed_content_area(80) + 5);
        let grown = boxed_body(&long, 80);
        assert_eq!(grown.chars().count(), 80 + 5);
        assert!(grown.starts_with("│  "));
        assert!(grown.ends_with("  │"));
        assert_eq!(boxed_body("fits", 80).chars().count(), 80);
    }

    /// The width bridge holds the measured width between the frame's floor
    /// and ceiling: a narrow terminal is met at the minimum, a wide one at
    /// the maximum, and the default in between passes through untouched.
    #[test]
    fn the_width_is_clamped_to_the_frame_floor_and_ceiling() {
        let rows = [
            (0, BOXED_MIN_WIDTH),
            (40, BOXED_MIN_WIDTH),
            (80, 80),
            (100, 100),
            (200, BOXED_MAX_WIDTH),
        ];
        for (measured, expected) in rows {
            assert_eq!(clamped(measured), expected, "clamp of {measured}");
        }
        assert_eq!(
            BOXED_MIN_WIDTH,
            Style::plain().width() as usize,
            "the plain style's default width is the frame floor"
        );
        assert_eq!(boxed_width(&Style::plain()), BOXED_MIN_WIDTH);
    }
}
