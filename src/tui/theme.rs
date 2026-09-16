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

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance. Every palette entry is `Color::Rgb`.
    fn luminance(c: Color) -> f64 {
        let (r, g, b) = match c {
            Color::Rgb(r, g, b) => (r, g, b),
            other => panic!("palette entries must be Rgb, got {other:?}"),
        };
        fn ch(v: u8) -> f64 {
            let v = v as f64 / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// How colourful a value is, 0.0 (grey) to 1.0.
    fn saturation(c: Color) -> f64 {
        let (r, g, b) = match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => return 0.0,
        };
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        if mx == 0 {
            0.0
        } else {
            (mx - mn) as f64 / mx as f64
        }
    }

    /// The neutral ramp must descend in luminance, or the hierarchy inverts.
    #[test]
    fn the_neutral_ramp_descends() {
        let t = theme();
        let ramp = [
            ("text", t.text),
            ("accent", t.accent),
            ("muted", t.muted),
            ("faint", t.faint),
        ];
        for pair in ramp.windows(2) {
            let (hi_name, hi) = pair[0];
            let (lo_name, lo) = pair[1];
            assert!(
                luminance(hi) > luminance(lo),
                "{hi_name} must be lighter than {lo_name}, but {:.3} <= {:.3}",
                luminance(hi),
                luminance(lo),
            );
        }
    }

    /// Adjacent steps must actually separate, not merely be ordered: two greys a
    /// hundredth of a ratio apart read as the same tone.
    #[test]
    fn adjacent_ramp_steps_separate() {
        let t = theme();
        for (a, b, name) in [
            (t.text, t.accent, "text/accent"),
            (t.accent, t.muted, "accent/muted"),
            (t.muted, t.faint, "muted/faint"),
        ] {
            let c = contrast(a, b);
            assert!(c >= 1.25, "{name} separates by only {c:.2}");
        }
    }

    /// A lane list renders a RUNNING lane's dot directly above a CANCELLED one's:
    /// running is `lane()`, cancelled is `muted()`. They share a surface, so they
    /// must not collide. Under the old blue these measured 1.00:1 — the two
    /// states were literally indistinguishable.
    #[test]
    fn running_and_cancelled_lane_dots_separate() {
        let c = contrast(theme().lane, theme().muted);
        assert!(c >= 1.25, "running vs cancelled lane dots = {c:.2}");
    }

    /// The accent marks ACTIONS; success/danger/warn mark STATE. A vivid accent
    /// competes with them, so the accent is neutral by design.
    #[test]
    fn the_accent_stays_neutral_against_the_status_hues() {
        let t = theme();
        let a = saturation(t.accent);
        assert!(a < 0.25, "the accent is too vivid at {a:.2}");
        for (c, name) in [
            (t.success, "success"),
            (t.danger, "danger"),
            (t.warn, "warn"),
        ] {
            assert!(a < saturation(c), "the accent must be duller than {name}");
        }
    }
}
