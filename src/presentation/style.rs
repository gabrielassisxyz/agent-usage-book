//! The terminal style layer: one place deciding colour, the palette and the
//! terminal width for every text renderer.
//!
//! May not depend on:
//! - provider adapters
//! - the store or the calibration module
//!
//! A renderer never spells an escape string: it asks this module for a named
//! style and wraps its text with [`Style::paint`]. The colour decision is made
//! here once, from the three inputs that decide it, so the renderers stay free
//! of terminal concerns and their tests run under [`Style::plain`], which
//! emits nothing and leaves every expected string exactly as written.
//!
//! Colour is on only when all three hold: stdout is a tty, `NO_COLOR` is unset
//! or empty, and `--no-color` was not passed. `--format json` never carries
//! styling because the JSON renderers never touch this module.
//!
//! The palette is the tokyonight table the design fixed, kept in one table in
//! one place ([`palette`]); hand-copying one of these values into a renderer
//! is the defect this module exists to prevent.

use crate::domain::quota::QuotaFractionPpm;

/// The width a renderer falls back to when stdout is not a terminal and
/// `COLUMNS` says nothing.
const DEFAULT_WIDTH: u16 = 80;

/// The tone thresholds on the remaining fraction, decided here once so no
/// renderer carries its own: at half the window remaining or above the tone is
/// green, below half it is yellow, and below a fifth it is red. A reading with
/// no remaining fraction to tone (a window that has not started, a stale
/// reading whose value is approximate) is the renderer's to mark with
/// [`Style::idle`] instead.
const TONE_YELLOW_BELOW_PPM: u32 = 500_000;
const TONE_RED_BELOW_PPM: u32 = 200_000;

/// The style a text renderer renders with: whether escapes are emitted at all,
/// and the terminal width the layout may right-align against.
///
/// Constructed from the three colour inputs with [`Style::new`], measured from
/// the environment with [`Style::detect`], or forced silent with
/// [`Style::plain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    colour: bool,
    width: u16,
}

impl Style {
    /// Constructs a style from the three colour inputs: whether stdout is a
    /// tty, whether `NO_COLOR` suppresses colour (present and non-empty), and
    /// whether `--no-color` was passed. Colour is on only when all three
    /// allow it.
    ///
    /// This constructor is pure so the truth table over the three inputs can
    /// be tested without a terminal; it has no terminal to ask, so it carries
    /// the default width. [`Style::detect`] is the constructor that measures.
    pub fn new(is_tty: bool, no_color_env: bool, no_color_flag: bool) -> Self {
        Self {
            colour: colour_enabled(is_tty, no_color_env, no_color_flag),
            width: DEFAULT_WIDTH,
        }
    }

    /// The style every renderer test runs under: no escape ever reaches the
    /// output, so a renderer's expected text is exactly what it renders, and
    /// the width is the default a renderer layout assumes when nothing is
    /// known about the terminal.
    pub fn plain() -> Self {
        Self::new(false, true, true)
    }

    /// The style a command renders with, measured from the environment: the
    /// tty state of stdout, the `NO_COLOR` environment and the caller's
    /// `--no-color` flag decide the colour; the terminal width comes from
    /// `TIOCGWINSZ` on stdout, then `COLUMNS`, then the default.
    pub fn detect(no_color_flag: bool) -> Self {
        let is_tty = stdout_is_a_terminal();
        // NO_COLOR: present and non-empty suppresses colour, whatever the
        // value; an empty value counts as unset, which is the convention's
        // own wording.
        let no_color_env = matches!(std::env::var("NO_COLOR"), Ok(value) if !value.is_empty());
        // COLUMNS is consulted only when the size query failed, because it is
        // the shell's belief about the terminal, not the terminal's answer.
        let width = resolve_width(&StdoutColumns, std::env::var("COLUMNS").ok().as_deref());
        Self {
            colour: colour_enabled(is_tty, no_color_env, no_color_flag),
            width,
        }
    }

    /// The column count the renderer layout may use: the terminal's own size
    /// when one was measured, otherwise the default the pure constructor
    /// carries. One measurement, taken when the style was built, so every
    /// renderer in one invocation lays out against the same width.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Wraps text in a named style's prefix and the reset escape, or returns
    /// the text unchanged when this style emits nothing. A renderer that wraps
    /// through this method cannot forget the reset.
    pub fn paint(&self, prefix: &str, text: &str) -> String {
        if self.colour {
            format!("{prefix}{text}{}", palette::RESET)
        } else {
            text.to_string()
        }
    }

    /// The bold attribute, for the one element a line may raise above the rest.
    pub fn bold(&self) -> &'static str {
        self.emit(palette::BOLD)
    }

    /// The dim foreground: one level down the hierarchy, for text that is
    /// present but secondary.
    pub fn dim(&self) -> &'static str {
        self.emit(palette::DIM)
    }

    /// The dimmer foreground: a second level down, for text that is present
    /// only to be findable.
    pub fn dimmer(&self) -> &'static str {
        self.emit(palette::DIMMER)
    }

    /// The accent foreground: the one saturated colour, for the element the
    /// eye is meant to land on first.
    pub fn accent(&self) -> &'static str {
        self.emit(palette::ACCENT)
    }

    /// The text foreground: headings and other highest-contrast text.
    pub fn text(&self) -> &'static str {
        self.emit(palette::TEXT)
    }

    /// The body foreground: the ordinary text colour.
    pub fn body(&self) -> &'static str {
        self.emit(palette::BODY)
    }

    /// The muted foreground: labels and units that carry structure, not
    /// values.
    pub fn muted(&self) -> &'static str {
        self.emit(palette::MUTED)
    }

    /// The green tone: a remaining fraction at or above half the window.
    pub fn green(&self) -> &'static str {
        self.emit(palette::GREEN)
    }

    /// The yellow tone: a remaining fraction below half but at or above a
    /// fifth of the window.
    pub fn yellow(&self) -> &'static str {
        self.emit(palette::YELLOW)
    }

    /// The red tone: a remaining fraction below a fifth of the window.
    pub fn red(&self) -> &'static str {
        self.emit(palette::RED)
    }

    /// The idle tone: a reading with no remaining fraction to tone, such as a
    /// window that has not started. Deliberately the same value the table
    /// carries for `dimmer`: idleness is the dimmest thing on the screen.
    pub fn idle(&self) -> &'static str {
        self.emit(palette::IDLE)
    }

    /// The track foreground: the darkest band, for background rules and
    /// separators.
    pub fn track(&self) -> &'static str {
        self.emit(palette::TRACK)
    }

    /// The tone of a remaining quota fraction: green at or above half the
    /// window, yellow below half, red below a fifth. Freshness is never
    /// conveyed by colour alone, so the renderer keeps the words and this
    /// only tints them.
    pub fn tone(&self, remaining: QuotaFractionPpm) -> &'static str {
        if remaining.get() < TONE_RED_BELOW_PPM {
            self.red()
        } else if remaining.get() < TONE_YELLOW_BELOW_PPM {
            self.yellow()
        } else {
            self.green()
        }
    }

    /// The reset escape that closes any opened style, or nothing when this
    /// style emits nothing.
    pub fn reset(&self) -> &'static str {
        self.emit(palette::RESET)
    }

    /// The escape to emit for a named style: the palette's string when colour
    /// is on, nothing when it is off.
    fn emit(&self, escape: &'static str) -> &'static str {
        if self.colour { escape } else { "" }
    }
}

/// The one colour rule, stated once: on only when the output is a terminal,
/// the environment did not suppress it and the caller did not ask for plain.
fn colour_enabled(is_tty: bool, no_color_env: bool, no_color_flag: bool) -> bool {
    is_tty && !no_color_env && !no_color_flag
}

/// The tokyonight table, one table in one place: 24-bit foreground escapes
/// spelled once, with the design's hex value in the comment beside each. The
/// strings carry the raw escape byte (0x1b) rather than the `\x1b` spelling,
/// so the escape grep that pins this module to the crate's one escape surface
/// has a positive control to match against. A renderer that wants one of
/// these asks a named [`Style`] method for it; nothing else in the crate
/// spells an escape.
mod palette {
    pub const ACCENT: &str = "[38;2;122;162;247m"; // #7aa2f7
    pub const TEXT: &str = "[38;2;192;202;245m"; // #c0caf5
    pub const BODY: &str = "[38;2;169;177;214m"; // #a9b1d6
    pub const MUTED: &str = "[38;2;120;124;153m"; // #787c99
    pub const DIM: &str = "[38;2;86;95;137m"; // #565f89
    pub const DIMMER: &str = "[38;2;63;68;99m"; // #3f4463
    pub const GREEN: &str = "[38;2;158;206;106m"; // #9ece6a
    pub const YELLOW: &str = "[38;2;224;175;104m"; // #e0af68
    pub const RED: &str = "[38;2;247;118;142m"; // #f7768e
    pub const IDLE: &str = "[38;2;63;68;99m"; // #3f4463
    pub const TRACK: &str = "[38;2;32;33;47m"; // #20212f
    /// The bold attribute: the one non-colour attribute the palette carries.
    pub const BOLD: &str = "[1m";
    /// The reset that closes any opened style.
    pub const RESET: &str = "[0m";
}

/// The source of the terminal's column count. The real source asks stdout
/// with `TIOCGWINSZ`; the width tests inject a stub to pin the resolution
/// order without a terminal.
pub trait TerminalColumns {
    /// The column count, or `None` when stdout is not a terminal or the size
    /// query failed.
    fn columns(&self) -> Option<u16>;
}

/// The real column source: the size of the terminal stdout writes to.
pub struct StdoutColumns;

impl TerminalColumns for StdoutColumns {
    fn columns(&self) -> Option<u16> {
        stdout_columns()
    }
}

/// The width a renderer lays out against, in resolution order: the terminal's
/// own size first, then `COLUMNS` when the size query failed and the shell
/// exported a belief, then the default. A zero from either source is no
/// answer, not a layout width, so it falls through like a failure.
pub(crate) fn resolve_width(source: &dyn TerminalColumns, columns_env: Option<&str>) -> u16 {
    if let Some(width) = source.columns().filter(|width| *width > 0) {
        return width;
    }
    if let Some(width) = columns_env
        .and_then(|raw| raw.trim().parse::<u16>().ok())
        .filter(|width| *width > 0)
    {
        return width;
    }
    DEFAULT_WIDTH
}

/// The two C entry points the style layer needs, declared here because the
/// repository takes explicit dependencies only and `libc` is not one of them.
/// The non-unix arm answers "not a terminal, 80 columns" and never calls
/// either.
#[cfg(unix)]
mod sys {
    use std::ffi::{c_int, c_ulong};

    unsafe extern "C" {
        pub fn isatty(fd: c_int) -> c_int;
        /// The terminal ioctls are variadic in C; the style layer always
        /// passes exactly one pointer argument, the `winsize` to fill.
        pub fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    /// `TIOCGWINSZ`, per platform: Linux spells the terminal ioctls as plain
    /// numbers, while the BSDs and macOS encode them as
    /// `_IOR('t', 104, struct winsize)`.
    #[cfg(target_os = "linux")]
    pub const TIOCGWINSZ: c_ulong = 0x5413;
    #[cfg(all(unix, not(target_os = "linux")))]
    pub const TIOCGWINSZ: c_ulong = 0x4008_7468;

    /// The `winsize` the size query fills, in the field order the kernel
    /// writes: rows, columns, then the pixel sizes this module ignores.
    #[repr(C)]
    #[derive(Default)]
    pub struct Winsize {
        pub rows: u16,
        pub cols: u16,
        pub xpixel: u16,
        pub ypixel: u16,
    }
}

/// The file descriptor the renderers write to, and therefore the one whose
/// terminal state the colour decision follows.
#[cfg(unix)]
const STDOUT_FILENO: std::ffi::c_int = 1;

/// Whether stdout is a terminal, which is the first of the three colour
/// inputs.
#[cfg(unix)]
fn stdout_is_a_terminal() -> bool {
    unsafe { sys::isatty(STDOUT_FILENO) == 1 }
}

/// The terminal's column count from `TIOCGWINSZ` on stdout, or `None` when
/// stdout is not a terminal, the query failed, or the query answered a size
/// with no columns (a pty before its size was set is as unusable as a failed
/// query).
#[cfg(unix)]
fn stdout_columns() -> Option<u16> {
    let mut size = sys::Winsize::default();
    let filled = unsafe { sys::ioctl(STDOUT_FILENO, sys::TIOCGWINSZ, &raw mut size) == 0 };
    (filled && size.cols > 0).then_some(size.cols)
}

/// The non-unix arm: never a terminal, never a width, so colour is off and
/// the layout falls back to 80 columns.
#[cfg(not(unix))]
fn stdout_is_a_terminal() -> bool {
    false
}

/// The non-unix arm of the column source.
#[cfg(not(unix))]
fn stdout_columns() -> Option<u16> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The colour truth table over the three inputs, all eight rows: colour is
    /// on only when stdout is a tty, `NO_COLOR` is unset or empty and
    /// `--no-color` was not passed. The seven remaining rows are the planted
    /// negatives: each holds one input that alone must silence the output.
    #[test]
    fn the_truth_table_turns_colour_on_only_when_all_three_allow_it() {
        let rows = [
            // (is_tty, no_color_env, no_color_flag, colour_expected)
            (true, false, false, true),
            (true, false, true, false),
            (true, true, false, false),
            (true, true, true, false),
            (false, false, false, false),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, false),
        ];
        for (is_tty, no_color_env, no_color_flag, colour_expected) in rows {
            let style = Style::new(is_tty, no_color_env, no_color_flag);
            let emits = !style.bold().is_empty();
            assert_eq!(
                emits, colour_expected,
                "bold emission wrong for (tty={is_tty}, env={no_color_env}, flag={no_color_flag})"
            );
            assert_eq!(
                !style.reset().is_empty(),
                colour_expected,
                "reset emission must follow colour for (tty={is_tty}, env={no_color_env}, flag={no_color_flag})"
            );
            assert_eq!(
                !style.green().is_empty(),
                colour_expected,
                "green emission must follow colour for (tty={is_tty}, env={no_color_env}, flag={no_color_flag})"
            );
        }
    }

    /// `Style::plain()` emits nothing for every named style, including every
    /// tone and the reset: a renderer's expected text is exactly what it
    /// renders, with no escape hiding in a prefix or a suffix.
    #[test]
    fn plain_emits_nothing_for_every_named_style() {
        let style = Style::plain();
        let every_named_style = [
            ("bold", style.bold().to_string()),
            ("dim", style.dim().to_string()),
            ("dimmer", style.dimmer().to_string()),
            ("accent", style.accent().to_string()),
            ("text", style.text().to_string()),
            ("body", style.body().to_string()),
            ("muted", style.muted().to_string()),
            ("green", style.green().to_string()),
            ("yellow", style.yellow().to_string()),
            ("red", style.red().to_string()),
            ("idle", style.idle().to_string()),
            ("track", style.track().to_string()),
            ("reset", style.reset().to_string()),
            (
                "tone at zero",
                style.tone(QuotaFractionPpm::new(0).unwrap()).to_string(),
            ),
            (
                "tone at full",
                style
                    .tone(QuotaFractionPpm::new(QuotaFractionPpm::MAX as i32).unwrap())
                    .to_string(),
            ),
        ];
        for (name, emitted) in every_named_style {
            assert!(emitted.is_empty(), "plain() must emit nothing for {name}");
        }
        assert_eq!(style.paint(style.bold(), "text"), "text");
        // The pure constructor has no terminal to ask, so the width it
        // carries is the default.
        assert_eq!(style.width(), DEFAULT_WIDTH);
    }

    /// The tokyonight table is the design's fixed values, pinned here against
    /// a transcription error in the decimal spells: one wrong digit in the
    /// table would tint every renderer.
    #[test]
    fn the_palette_carries_the_designs_fixed_values() {
        let coloured = Style::new(true, false, false);
        let pinned = [
            ("accent", coloured.accent(), "\x1b[38;2;122;162;247m"),
            ("text", coloured.text(), "\x1b[38;2;192;202;245m"),
            ("body", coloured.body(), "\x1b[38;2;169;177;214m"),
            ("muted", coloured.muted(), "\x1b[38;2;120;124;153m"),
            ("dim", coloured.dim(), "\x1b[38;2;86;95;137m"),
            ("dimmer", coloured.dimmer(), "\x1b[38;2;63;68;99m"),
            ("green", coloured.green(), "\x1b[38;2;158;206;106m"),
            ("yellow", coloured.yellow(), "\x1b[38;2;224;175;104m"),
            ("red", coloured.red(), "\x1b[38;2;247;118;142m"),
            ("idle", coloured.idle(), "\x1b[38;2;63;68;99m"),
            ("track", coloured.track(), "\x1b[38;2;32;33;47m"),
            ("bold", coloured.bold(), "\x1b[1m"),
            ("reset", coloured.reset(), "\x1b[0m"),
        ];
        for (name, emitted, expected) in pinned {
            assert_eq!(emitted, expected, "{name} must be the design's fixed value");
        }
    }

    /// The tone follows the remaining fraction at its two thresholds, with
    /// the boundary rows exactly on them: the fractions at and above half are
    /// green, the fractions below half down to a fifth are yellow, and
    /// anything below a fifth is red.
    #[test]
    fn tone_follows_the_remaining_fraction_at_both_thresholds() {
        let style = Style::new(true, false, false);
        let rows = [
            (QuotaFractionPpm::MAX, "green"),
            (500_000, "green"),
            (499_999, "yellow"),
            (250_000, "yellow"),
            (200_000, "yellow"),
            (199_999, "red"),
            (0, "red"),
        ];
        for (ppm, expected) in rows {
            let remaining = QuotaFractionPpm::new(ppm as i32)
                .unwrap_or_else(|| panic!("{ppm} ppm must be a valid fraction"));
            let tone = style.tone(remaining);
            let expected_escape = match expected {
                "green" => "\x1b[38;2;158;206;106m",
                "yellow" => "\x1b[38;2;224;175;104m",
                "red" => "\x1b[38;2;247;118;142m",
                other => panic!("unknown tone expectation: {other}"),
            };
            assert_eq!(
                tone, expected_escape,
                "tone at {ppm} ppm must be {expected}"
            );
        }
    }

    /// A style that paints wraps with the prefix and the reset; a plain style
    /// returns the text unchanged, so `paint` is byte-transparent under
    /// `Style::plain()`.
    #[test]
    fn paint_wraps_with_prefix_and_reset_and_is_transparent_when_plain() {
        let coloured = Style::new(true, false, false);
        assert_eq!(
            coloured.paint(coloured.green(), "38% left"),
            "\x1b[38;2;158;206;106m38% left\x1b[0m"
        );
        assert_eq!(
            Style::plain().paint(Style::plain().green(), "38% left"),
            "38% left"
        );
    }

    /// A column source the width tests control, standing in for the terminal
    /// size query.
    struct FixedColumns(Option<u16>);

    impl TerminalColumns for FixedColumns {
        fn columns(&self) -> Option<u16> {
            self.0
        }
    }

    /// The width resolution order: the terminal's own size first, then
    /// `COLUMNS` when the size query failed, then the default. The negative
    /// that keeps the order honest is the row where both sources answer: the
    /// query must win, because `COLUMNS` is the shell's belief, not the
    /// terminal's answer.
    #[test]
    fn width_resolves_query_then_columns_then_default() {
        let rows = [
            // (query, COLUMNS, expected)
            (Some(120), None, 120),
            (Some(120), Some("200"), 120),
            (Some(80), Some("200"), 80),
            (Some(0), Some("100"), 100),
            (Some(0), None, 80),
            (None, Some("100"), 100),
            (None, Some(" 72 "), 72),
            (None, Some("0"), 80),
            (None, Some("bogus"), 80),
            (None, None, 80),
        ];
        for (query, columns_env, expected) in rows {
            let width = resolve_width(&FixedColumns(query), columns_env);
            assert_eq!(
                width, expected,
                "width for query={query:?} and COLUMNS={columns_env:?} must be {expected}"
            );
        }
    }
}
