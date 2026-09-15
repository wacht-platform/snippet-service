use super::*;

// Single TUI palette — same AMOLED Black as the Flutter client.
#[derive(Clone, Copy)]
pub(super) struct Theme {
    pub(super) accent: Color,
    pub(super) text: Color,
    pub(super) muted: Color,
    pub(super) faint: Color,
    pub(super) success: Color,
    pub(super) danger: Color,
    pub(super) warn: Color,
    pub(super) lane: Color,
    pub(super) code: Color,
}

const AMOLED: Theme = Theme {
    // Slate, not blue. A neutral accent keeps the accent channel about BRIGHTNESS
    // rather than hue, so it cannot be confused with a status colour.
    //
    // #B4BECD, not the client's #94A3B8: this palette's `muted` (#9CA3AF) is also
    // a grey, and the two collide at 1.01:1 — and they DO share a surface, since a
    // lane list shows "running" (accent) directly above "cancelled" (muted). This
    // slate sits midway between `muted` (1.35:1) and `text` (1.52:1), the widest
    // separation available inside that narrow band.
    accent: Color::Rgb(180, 190, 205),
    text: Color::Rgb(229, 231, 235),
    muted: Color::Rgb(156, 163, 175),
    faint: Color::Rgb(107, 114, 128),
    success: Color::Rgb(52, 211, 153),
    danger: Color::Rgb(248, 113, 113),
    warn: Color::Rgb(251, 191, 36),
    // Lane activity is the same "something is live" signal as `accent`, so it
    // shares its tone; the lane rows carry a glyph and a label of their own.
    lane: Color::Rgb(180, 190, 205),
    code: Color::Rgb(209, 213, 219),
};

pub(super) fn theme() -> Theme {
    AMOLED
}

/// Persisted config names still load; every name is AMOLED.
pub(super) fn set_theme_by_name(_name: &str) -> bool {
    true
}

pub(super) fn accent() -> Color {
    theme().accent
}
pub(super) fn text() -> Color {
    theme().text
}
pub(super) fn muted() -> Color {
    theme().muted
}
pub(super) fn faint() -> Color {
    theme().faint
}
pub(super) fn success() -> Color {
    theme().success
}
pub(super) fn danger() -> Color {
    theme().danger
}
pub(super) fn warn() -> Color {
    theme().warn
}
pub(super) fn lane() -> Color {
    theme().lane
}
pub(super) fn code() -> Color {
    theme().code
}

pub(super) fn subtle() -> Style {
    Style::default().fg(muted())
}

pub(super) fn blue() -> Color {
    accent()
}
