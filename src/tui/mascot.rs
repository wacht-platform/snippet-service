use ratatui::prelude::*;

static MASCOT_BYTES: &[u8] = include_bytes!("mascot.bin");

pub fn mascot_halfblock_lines(total_width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let pad_len = total_width.saturating_sub(48) / 2;
    let pad = " ".repeat(pad_len);

    for row_chunk in MASCOT_BYTES.chunks_exact(48 * 6) {
        let mut spans = vec![Span::raw(pad.clone())];
        for pixel in row_chunk.chunks_exact(6) {
            spans.push(Span::styled(
                "▄",
                Style::default()
                    .fg(Color::Rgb(pixel[0], pixel[1], pixel[2]))
                    .bg(Color::Rgb(pixel[3], pixel[4], pixel[5])),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines
}
