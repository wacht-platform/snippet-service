use super::*;

#[derive(Clone, Copy)]
pub(super) struct Theme {
    pub(super) accent: Color,
    pub(super) text: Color,
    pub(super) soft: Color,
    pub(super) muted: Color,
    pub(super) faint: Color,
    pub(super) success: Color,
    pub(super) danger: Color,
    pub(super) warn: Color,
    pub(super) lane: Color,
    pub(super) code: Color,
    pub(super) surface1: Color,
    pub(super) surface2: Color,
    pub(super) surface3: Color,
    pub(super) border2: Color,
}

const DARK: Theme = Theme {
    accent: Color::Rgb(0x6E, 0xA2, 0xFF),
    text: Color::Rgb(0xED, 0xED, 0xEF),
    soft: Color::Rgb(0xC8, 0xC8, 0xCC),
    muted: Color::Rgb(0x9A, 0x9A, 0xA2),
    faint: Color::Rgb(0x6E, 0x6E, 0x76),
    success: Color::Rgb(0x39, 0xC5, 0x7E),
    danger: Color::Rgb(0xF0, 0x64, 0x64),
    warn: Color::Rgb(0xD4, 0x98, 0x2F),
    lane: Color::Rgb(0x6E, 0xA2, 0xFF),
    code: Color::Rgb(0xC8, 0xC8, 0xCC),
    surface1: Color::Rgb(0x1C, 0x1C, 0x1E),
    surface2: Color::Rgb(0x23, 0x23, 0x25),
    surface3: Color::Rgb(0x2A, 0x2A, 0x2D),
    border2: Color::Rgb(0x36, 0x36, 0x3A),
};

pub(super) fn theme() -> Theme {
    DARK
}

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
pub(super) fn soft() -> Color {
    theme().soft
}
pub(super) fn surface1() -> Color {
    theme().surface1
}
pub(super) fn surface2() -> Color {
    theme().surface2
}
pub(super) fn surface3() -> Color {
    theme().surface3
}
pub(super) fn border2() -> Color {
    theme().border2
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
            ("soft", t.soft),
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
            (t.text, t.soft, "text/soft"),
            (t.soft, t.muted, "soft/muted"),
            (t.muted, t.faint, "muted/faint"),
        ] {
            let c = contrast(a, b);
            assert!(c >= 1.25, "{name} separates by only {c:.2}");
        }
    }

    #[test]
    fn the_accent_is_a_distinct_readable_hue() {
        let t = theme();
        assert!(saturation(t.accent) > 0.3, "the accent must read as a colour");
        for (surface, name) in [(Color::Rgb(0x15, 0x15, 0x16), "bg"), (t.surface1, "surface1"), (t.surface2, "surface2")] {
            let c = contrast(t.accent, surface);
            assert!(c >= 4.5, "accent on {name} = {c:.2}");
            let c = contrast(t.faint, surface);
            assert!(c >= 3.0, "faint on {name} = {c:.2}");
        }
    }
}
