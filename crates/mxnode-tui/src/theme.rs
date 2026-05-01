//! Color palette + style helpers, inspired by the upstream Go termui's
//! visual language (cyan accents, yellow node-name highlight, green
//! synchronized status, dim debug logs) but tuned for ratatui's
//! truecolor surface + a darker default background.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Rgb(120, 220, 240); // brighter cyan than Color::Cyan
pub const HIGHLIGHT: Color = Color::Rgb(255, 196, 87); // warm yellow for node-name highlight

pub const HEADER_BG: Color = Color::Rgb(28, 30, 42);
pub const STATUS_BG: Color = Color::Rgb(20, 22, 32);
/// Background tint behind an "ON" toggle chip in the status bar.
pub const CHIP_ON_BG: Color = Color::Rgb(38, 56, 50);

pub const OK: Color = Color::Rgb(0, 224, 134);
pub const WARN: Color = Color::Rgb(255, 184, 60);
pub const FAIL: Color = Color::Rgb(255, 95, 95);
// MUTED is what every "label" / "secondary" span uses. We deliberately
// pushed it brighter — the previous Rgb(150,156,174) plus Modifier::DIM
// rendered as a flat near-#666 on common terminals, which made labels
// hard to read against the dark background. The new value is calibrated
// for ≥7:1 contrast against #16161f (status bar bg) at 100% lightness.
pub const MUTED: Color = Color::Rgb(190, 198, 220);
pub const BORDER: Color = Color::Rgb(110, 122, 150); // bumped from #50586e

// Log-level palette — picked for readability on dark terminals. We
// deliberately do NOT use `Modifier::DIM` anywhere because most
// terminals crush every fg through the same desaturation lookup,
// which makes every "dim" colour render as the same flat grey.
// Distinct fg colours at full brightness keep level differentiation
// visible without sacrificing readability.
pub const LOG_INFO: Color = Color::Rgb(232, 236, 244); // near-white
pub const LOG_DEBUG: Color = Color::Rgb(135, 200, 230); // cyan-blue (Go termui DEBUG)
pub const LOG_TRACE: Color = Color::Rgb(165, 175, 200); // bumped from #767c91
pub const LOG_OTHER: Color = Color::Rgb(210, 216, 232); // bumped from #aab0c3

pub fn header_bar() -> Style {
    Style::default()
        .bg(HEADER_BG)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

pub fn status_bar() -> Style {
    Style::default().bg(STATUS_BG).fg(Color::White)
}

pub fn brand() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn title() -> Style {
    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub fn label() -> Style {
    // Field-name colour. Distinct from `dim()` — labels are
    // information, not afterthoughts, so they get a bit more
    // saturation than secondary/separator text.
    Style::default().fg(Color::Rgb(165, 175, 205))
}

pub fn ok() -> Style {
    Style::default().fg(OK)
}

pub fn warn() -> Style {
    Style::default().fg(WARN)
}

pub fn fail() -> Style {
    Style::default().fg(FAIL)
}

pub fn dim() -> Style {
    // Note: NO `Modifier::DIM`. Most terminals (Apple Terminal, iTerm
    // default, kitty, alacritty default) render the DIM attribute by
    // halving the foreground intensity, which crushes every "dim"
    // colour to a flat grey and destroys readability. We rely on a
    // lower-saturation fg colour alone for the dim effect.
    Style::default().fg(MUTED)
}

pub fn log_info() -> Style {
    Style::default().fg(LOG_INFO)
}

pub fn log_debug() -> Style {
    Style::default().fg(LOG_DEBUG)
}

pub fn log_trace() -> Style {
    Style::default()
        .fg(LOG_TRACE)
        .add_modifier(Modifier::ITALIC)
}

pub fn log_other() -> Style {
    Style::default().fg(LOG_OTHER)
}

pub fn border() -> Style {
    Style::default().fg(BORDER)
}

pub fn accent_gauge() -> Style {
    Style::default().fg(ACCENT).bg(Color::Rgb(40, 44, 60))
}
