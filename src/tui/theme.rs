use super::*;

// --- Theme: a runtime-selectable palette. Every UI color routes through the
// active theme (switch with `/theme`, persisted to config). Presets below. ---
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

pub(super) struct ThemePreset {
    pub(super) name: &'static str,
    pub(super) label: &'static str,
    pub(super) theme: Theme,
}

pub(super) const MIDNIGHT: Theme = Theme {
    accent: Color::Rgb(96, 165, 250),
    text: Color::Rgb(222, 225, 230),
    muted: Color::Rgb(124, 130, 142),
    faint: Color::Rgb(82, 86, 96),
    success: Color::Rgb(126, 186, 120),
    danger: Color::Rgb(224, 108, 117),
    warn: Color::Rgb(214, 182, 106),
    lane: Color::Rgb(110, 184, 200),
    code: Color::Rgb(224, 196, 132),
};

pub(super) const LIGHT: Theme = Theme {
    accent: Color::Rgb(37, 99, 235),
    text: Color::Rgb(30, 41, 59),
    muted: Color::Rgb(90, 105, 125),
    faint: Color::Rgb(176, 184, 198),
    success: Color::Rgb(21, 128, 76),
    danger: Color::Rgb(193, 41, 46),
    warn: Color::Rgb(168, 113, 10),
    lane: Color::Rgb(13, 124, 156),
    code: Color::Rgb(146, 64, 14),
};

pub(super) const HIGH_CONTRAST: Theme = Theme {
    accent: Color::Rgb(125, 205, 255),
    text: Color::Rgb(255, 255, 255),
    muted: Color::Rgb(190, 196, 206),
    faint: Color::Rgb(120, 126, 136),
    success: Color::Rgb(120, 240, 130),
    danger: Color::Rgb(255, 112, 112),
    warn: Color::Rgb(255, 214, 92),
    lane: Color::Rgb(120, 232, 250),
    code: Color::Rgb(245, 222, 150),
};

pub(super) const EMBER: Theme = Theme {
    accent: Color::Rgb(245, 158, 11),
    text: Color::Rgb(237, 224, 212),
    muted: Color::Rgb(168, 148, 130),
    faint: Color::Rgb(96, 84, 74),
    success: Color::Rgb(158, 188, 108),
    danger: Color::Rgb(228, 110, 92),
    warn: Color::Rgb(232, 180, 90),
    lane: Color::Rgb(206, 150, 110),
    code: Color::Rgb(230, 190, 130),
};

pub(super) const NORD: Theme = Theme {
    accent: Color::Rgb(136, 192, 208),
    text: Color::Rgb(216, 222, 233),
    muted: Color::Rgb(129, 140, 158),
    faint: Color::Rgb(76, 86, 106),
    success: Color::Rgb(163, 190, 140),
    danger: Color::Rgb(191, 97, 106),
    warn: Color::Rgb(235, 203, 139),
    lane: Color::Rgb(129, 161, 193),
    code: Color::Rgb(143, 188, 187),
};

pub(super) const DRACULA: Theme = Theme {
    accent: Color::Rgb(189, 147, 249),
    text: Color::Rgb(248, 248, 242),
    muted: Color::Rgb(130, 138, 165),
    faint: Color::Rgb(98, 114, 164),
    success: Color::Rgb(80, 250, 123),
    danger: Color::Rgb(255, 85, 85),
    warn: Color::Rgb(241, 250, 140),
    lane: Color::Rgb(139, 233, 253),
    code: Color::Rgb(255, 184, 108),
};

pub(super) const GRUVBOX: Theme = Theme {
    accent: Color::Rgb(250, 189, 47),
    text: Color::Rgb(235, 219, 178),
    muted: Color::Rgb(168, 153, 132),
    faint: Color::Rgb(102, 92, 84),
    success: Color::Rgb(184, 187, 38),
    danger: Color::Rgb(251, 73, 52),
    warn: Color::Rgb(254, 128, 25),
    lane: Color::Rgb(142, 192, 124),
    code: Color::Rgb(131, 165, 152),
};

pub(super) const TOKYO_NIGHT: Theme = Theme {
    accent: Color::Rgb(122, 162, 247),
    text: Color::Rgb(192, 202, 245),
    muted: Color::Rgb(140, 148, 184),
    faint: Color::Rgb(65, 72, 104),
    success: Color::Rgb(158, 206, 106),
    danger: Color::Rgb(247, 118, 142),
    warn: Color::Rgb(224, 175, 104),
    lane: Color::Rgb(125, 207, 255),
    code: Color::Rgb(187, 154, 247),
};

pub(super) const CATPPUCCIN: Theme = Theme {
    accent: Color::Rgb(203, 166, 247),
    text: Color::Rgb(205, 214, 244),
    muted: Color::Rgb(147, 153, 178),
    faint: Color::Rgb(88, 91, 112),
    success: Color::Rgb(166, 227, 161),
    danger: Color::Rgb(243, 139, 168),
    warn: Color::Rgb(249, 226, 175),
    lane: Color::Rgb(137, 220, 235),
    code: Color::Rgb(250, 179, 135),
};

pub(super) const SOLARIZED: Theme = Theme {
    accent: Color::Rgb(38, 139, 210),
    text: Color::Rgb(147, 161, 161),
    muted: Color::Rgb(101, 123, 131),
    faint: Color::Rgb(68, 93, 100),
    success: Color::Rgb(133, 153, 0),
    danger: Color::Rgb(220, 50, 47),
    warn: Color::Rgb(181, 137, 0),
    lane: Color::Rgb(42, 161, 152),
    code: Color::Rgb(203, 75, 22),
};

pub(super) const PRESETS: &[ThemePreset] = &[
    ThemePreset { name: "midnight", label: "Midnight (dark)", theme: MIDNIGHT },
    ThemePreset { name: "light", label: "Light", theme: LIGHT },
    ThemePreset { name: "high-contrast", label: "High-contrast", theme: HIGH_CONTRAST },
    ThemePreset { name: "ember", label: "Ember (warm)", theme: EMBER },
    ThemePreset { name: "nord", label: "Nord", theme: NORD },
    ThemePreset { name: "dracula", label: "Dracula", theme: DRACULA },
    ThemePreset { name: "gruvbox", label: "Gruvbox", theme: GRUVBOX },
    ThemePreset { name: "tokyo-night", label: "Tokyo Night", theme: TOKYO_NIGHT },
    ThemePreset { name: "catppuccin", label: "Catppuccin", theme: CATPPUCCIN },
    ThemePreset { name: "solarized", label: "Solarized Dark", theme: SOLARIZED },
];

pub(super) static THEME_INDEX: AtomicUsize = AtomicUsize::new(0);

pub(super) fn theme() -> Theme {
    PRESETS
        .get(THEME_INDEX.load(Ordering::Relaxed))
        .map(|p| p.theme)
        .unwrap_or(MIDNIGHT)
}

pub(super) fn set_theme_index(i: usize) {
    THEME_INDEX.store(i.min(PRESETS.len().saturating_sub(1)), Ordering::Relaxed);
}

/// Apply a theme by its config name; returns false if no preset matches.
pub(super) fn set_theme_by_name(name: &str) -> bool {
    if let Some(i) = PRESETS.iter().position(|p| p.name.eq_ignore_ascii_case(name.trim())) {
        set_theme_index(i);
        true
    } else {
        false
    }
}

pub(super) fn current_theme_index() -> usize {
    THEME_INDEX.load(Ordering::Relaxed)
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
