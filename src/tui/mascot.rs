use ratatui::prelude::*;

const MOCHI: [&str; 14] = [
    "..............ll........",
    ".............ll.........",
    "........bbbbbbbb........",
    "......bbbbbbbbbbbb......",
    "....bbbbbbbbbbbbbbbb....",
    "...bbbbbbbbbbbbbbbbbb...",
    "..bbbbbbbbbbbbbbbbbbbb..",
    "..bbbbbbebbbbbbebbbbbb..",
    ".bbbbbbbebbbbbbebbbbbbb.",
    ".bbbbppbbbbeebbbbppbbbb.",
    ".bbbbbbbbbbbbbbbbbbbbbb.",
    ".bbbbbbbbbbbbbbbbbbbbbb.",
    "..bbbbbbbbbbbbbbbbbbbb..",
    "....bbbbbbbbbbbbbbbb....",
];

fn colour(c: u8) -> Option<Color> {
    match c {
        b'b' => Some(Color::Rgb(185, 198, 255)),
        b'l' => Some(Color::Rgb(127, 209, 166)),
        b'e' => Some(Color::Rgb(21, 22, 28)),
        b'p' => Some(Color::Rgb(242, 167, 160)),
        _ => None,
    }
}

pub fn mochi_lines(total_width: usize) -> Vec<Line<'static>> {
    let art_width = MOCHI[0].len();
    let pad = " ".repeat(total_width.saturating_sub(art_width) / 2);
    MOCHI
        .chunks(2)
        .map(|pair| {
            let (top, bottom) = (pair[0].as_bytes(), pair[1].as_bytes());
            let mut spans = vec![Span::raw(pad.clone())];
            for x in 0..art_width {
                let span = match (colour(top[x]), colour(bottom[x])) {
                    (None, None) => Span::raw(" "),
                    (Some(t), None) => Span::styled("▀", Style::default().fg(t)),
                    (None, Some(b)) => Span::styled("▄", Style::default().fg(b)),
                    (Some(t), Some(b)) if t == b => Span::styled("█", Style::default().fg(t)),
                    (Some(t), Some(b)) => Span::styled("▀", Style::default().fg(t).bg(b)),
                };
                spans.push(span);
            }
            Line::from(spans)
        })
        .collect()
}
