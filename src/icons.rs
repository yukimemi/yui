//! Icon character sets for terminal output.
//!
//! Three modes (see [`crate::config::IconsMode`]):
//!   - `Unicode` (default): `✓ ✗ → ─` — universally renderable
//!   - `Nerd`: Nerd-Font glyphs — requires a patched font
//!   - `Ascii`: `[+] [-] -> -` — pure ASCII fallback for CI logs

use crate::config::IconsMode;

#[derive(Debug, Clone, Copy)]
pub struct Icons {
    pub active: &'static str,
    pub inactive: &'static str,
    pub arrow: &'static str,
    pub sep: char,
}

impl Icons {
    pub const UNICODE: Self = Self {
        active: "\u{2713}",   // ✓
        inactive: "\u{2717}", // ✗
        arrow: "\u{2192}",    // →
        sep: '\u{2500}',      // ─
    };
    pub const NERD: Self = Self {
        active: "\u{f058}",   //   nf-fa-check_circle
        inactive: "\u{f057}", //   nf-fa-times_circle
        arrow: "\u{2192}",    // → (no need for a special arrow glyph)
        sep: '\u{2500}',      // ─
    };
    pub const ASCII: Self = Self {
        active: "[+]",
        inactive: "[-]",
        arrow: "->",
        sep: '-',
    };

    pub const fn for_mode(mode: IconsMode) -> Self {
        match mode {
            IconsMode::Unicode => Self::UNICODE,
            IconsMode::Nerd => Self::NERD,
            IconsMode::Ascii => Self::ASCII,
        }
    }
}
