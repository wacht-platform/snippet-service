use ratatui::prelude::*;

pub fn mascot_halfblock_lines(total_width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let pad_len = total_width.saturating_sub(48) / 2;
    let pad = " ".repeat(pad_len);

    // Row 0
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 13, 30))
            .bg(Color::Rgb(3, 17, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 15, 31))
            .bg(Color::Rgb(4, 18, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 21, 33))
            .bg(Color::Rgb(2, 12, 30)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 16, 31))
            .bg(Color::Rgb(2, 11, 30)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 11, 30))
            .bg(Color::Rgb(2, 13, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 29, 38))
            .bg(Color::Rgb(6, 35, 40)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 7, 28))
            .bg(Color::Rgb(3, 16, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 65, 53))
            .bg(Color::Rgb(2, 16, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 78, 59))
            .bg(Color::Rgb(2, 18, 33)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 10, 31))
            .bg(Color::Rgb(2, 13, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 26, 37))
            .bg(Color::Rgb(3, 18, 33)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 20, 35))
            .bg(Color::Rgb(5, 24, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 22, 36))
            .bg(Color::Rgb(3, 18, 34)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 25, 39))
            .bg(Color::Rgb(3, 21, 36)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 33, 41))
            .bg(Color::Rgb(4, 24, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 40, 44))
            .bg(Color::Rgb(6, 33, 40)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 32, 42))
            .bg(Color::Rgb(3, 25, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 29, 41))
            .bg(Color::Rgb(3, 24, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 29, 41))
            .bg(Color::Rgb(3, 26, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 35, 44))
            .bg(Color::Rgb(3, 24, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 40, 46))
            .bg(Color::Rgb(4, 28, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 32, 44))
            .bg(Color::Rgb(5, 32, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 26, 40))
            .bg(Color::Rgb(4, 28, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 41, 45))
            .bg(Color::Rgb(3, 25, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 54, 56))
            .bg(Color::Rgb(1, 24, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(17, 54, 55))
            .bg(Color::Rgb(0, 20, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 51, 53))
            .bg(Color::Rgb(0, 18, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 35, 45))
            .bg(Color::Rgb(2, 24, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 45, 47))
            .bg(Color::Rgb(3, 25, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 34, 43))
            .bg(Color::Rgb(2, 19, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 42, 48))
            .bg(Color::Rgb(0, 14, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 37, 44))
            .bg(Color::Rgb(3, 22, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 31, 41))
            .bg(Color::Rgb(2, 21, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 46, 47))
            .bg(Color::Rgb(3, 25, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 33, 40))
            .bg(Color::Rgb(5, 29, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 23, 36))
            .bg(Color::Rgb(5, 28, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 35, 40))
            .bg(Color::Rgb(7, 34, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 33, 40))
            .bg(Color::Rgb(9, 37, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 10, 30))
            .bg(Color::Rgb(2, 11, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 61, 51))
            .bg(Color::Rgb(2, 14, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 51, 46))
            .bg(Color::Rgb(3, 17, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 5, 27))
            .bg(Color::Rgb(2, 9, 29)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 28, 38))
            .bg(Color::Rgb(4, 28, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 48, 45))
            .bg(Color::Rgb(6, 43, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 17, 30))
            .bg(Color::Rgb(1, 7, 28)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 19, 32))
            .bg(Color::Rgb(2, 12, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 18, 32))
            .bg(Color::Rgb(2, 13, 30)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 12, 30))
            .bg(Color::Rgb(4, 17, 31)),
    ));
    lines.push(Line::from(spans));
    // Row 1
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 21, 33))
            .bg(Color::Rgb(2, 12, 30)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 14, 30))
            .bg(Color::Rgb(3, 15, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 20, 34))
            .bg(Color::Rgb(3, 15, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 29, 37))
            .bg(Color::Rgb(8, 35, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 27, 37))
            .bg(Color::Rgb(7, 32, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 34, 39))
            .bg(Color::Rgb(8, 43, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 33, 40))
            .bg(Color::Rgb(5, 32, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 30, 39))
            .bg(Color::Rgb(4, 31, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 26, 39))
            .bg(Color::Rgb(4, 30, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 28, 39))
            .bg(Color::Rgb(3, 22, 36)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 31, 40))
            .bg(Color::Rgb(3, 23, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 69, 60))
            .bg(Color::Rgb(6, 33, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 74, 74))
            .bg(Color::Rgb(13, 59, 52)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 89, 79))
            .bg(Color::Rgb(4, 38, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 86, 67))
            .bg(Color::Rgb(1, 24, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 53, 54))
            .bg(Color::Rgb(5, 38, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 39, 47))
            .bg(Color::Rgb(5, 36, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 42, 50))
            .bg(Color::Rgb(5, 39, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 40, 51))
            .bg(Color::Rgb(6, 41, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 46, 53))
            .bg(Color::Rgb(5, 40, 49)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 46, 56))
            .bg(Color::Rgb(6, 46, 51)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 78, 63))
            .bg(Color::Rgb(3, 43, 49)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(81, 86, 119))
            .bg(Color::Rgb(23, 71, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(69, 82, 107))
            .bg(Color::Rgb(62, 85, 100)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 74, 64))
            .bg(Color::Rgb(54, 89, 91)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 39, 52))
            .bg(Color::Rgb(29, 83, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 36, 50))
            .bg(Color::Rgb(26, 91, 73)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 44, 53))
            .bg(Color::Rgb(13, 57, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 57, 59))
            .bg(Color::Rgb(1, 33, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 100, 79))
            .bg(Color::Rgb(17, 66, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 72, 65))
            .bg(Color::Rgb(55, 135, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 32, 45))
            .bg(Color::Rgb(11, 54, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 56, 55))
            .bg(Color::Rgb(3, 27, 40)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(27, 87, 67))
            .bg(Color::Rgb(7, 51, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 92, 81))
            .bg(Color::Rgb(6, 50, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 71, 73))
            .bg(Color::Rgb(12, 55, 50)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 70, 62))
            .bg(Color::Rgb(8, 44, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 54, 51))
            .bg(Color::Rgb(6, 44, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 30, 40))
            .bg(Color::Rgb(7, 36, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 35, 43))
            .bg(Color::Rgb(6, 43, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 31, 39))
            .bg(Color::Rgb(7, 41, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 25, 36))
            .bg(Color::Rgb(3, 15, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 56, 47))
            .bg(Color::Rgb(5, 30, 36)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 62, 49))
            .bg(Color::Rgb(8, 46, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 22, 34))
            .bg(Color::Rgb(4, 19, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 23, 34))
            .bg(Color::Rgb(4, 19, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 16, 31))
            .bg(Color::Rgb(3, 19, 33)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 23, 34))
            .bg(Color::Rgb(4, 17, 31)),
    ));
    lines.push(Line::from(spans));
    // Row 2
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 26, 36))
            .bg(Color::Rgb(5, 21, 34)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 16, 32))
            .bg(Color::Rgb(2, 12, 29)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 38, 40))
            .bg(Color::Rgb(7, 38, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 40, 42))
            .bg(Color::Rgb(8, 44, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 33, 41))
            .bg(Color::Rgb(7, 31, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 38, 42))
            .bg(Color::Rgb(7, 42, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 58, 51))
            .bg(Color::Rgb(3, 20, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 75, 60))
            .bg(Color::Rgb(9, 62, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 78, 65))
            .bg(Color::Rgb(8, 62, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 31, 44))
            .bg(Color::Rgb(5, 31, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 46, 49))
            .bg(Color::Rgb(3, 32, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 75, 66))
            .bg(Color::Rgb(26, 75, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 47, 56))
            .bg(Color::Rgb(24, 23, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 112, 66))
            .bg(Color::Rgb(31, 34, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(23, 29, 53))
            .bg(Color::Rgb(64, 92, 101)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(61, 89, 93))
            .bg(Color::Rgb(62, 134, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(76, 135, 102))
            .bg(Color::Rgb(36, 101, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(44, 74, 76))
            .bg(Color::Rgb(21, 90, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 75, 76))
            .bg(Color::Rgb(9, 72, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 92, 81))
            .bg(Color::Rgb(3, 60, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 84, 89))
            .bg(Color::Rgb(2, 64, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(59, 91, 103))
            .bg(Color::Rgb(16, 95, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(47, 67, 79))
            .bg(Color::Rgb(45, 51, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 72, 69))
            .bg(Color::Rgb(27, 83, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(59, 103, 100))
            .bg(Color::Rgb(4, 70, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(62, 97, 103))
            .bg(Color::Rgb(9, 68, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(54, 85, 99))
            .bg(Color::Rgb(8, 73, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 81, 78))
            .bg(Color::Rgb(4, 70, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(44, 108, 82))
            .bg(Color::Rgb(33, 106, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 82, 81))
            .bg(Color::Rgb(32, 102, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(46, 74, 77))
            .bg(Color::Rgb(17, 78, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(77, 138, 104))
            .bg(Color::Rgb(36, 97, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(62, 88, 93))
            .bg(Color::Rgb(65, 136, 97)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 29, 52))
            .bg(Color::Rgb(67, 91, 103)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 113, 66))
            .bg(Color::Rgb(31, 32, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(28, 40, 54))
            .bg(Color::Rgb(24, 20, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 92, 76))
            .bg(Color::Rgb(30, 95, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 109, 78))
            .bg(Color::Rgb(13, 108, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 32, 44))
            .bg(Color::Rgb(4, 27, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 38, 46))
            .bg(Color::Rgb(6, 35, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 35, 43))
            .bg(Color::Rgb(7, 39, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 33, 42))
            .bg(Color::Rgb(3, 18, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 49, 48))
            .bg(Color::Rgb(9, 56, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 49, 46))
            .bg(Color::Rgb(13, 81, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 38, 41))
            .bg(Color::Rgb(3, 18, 30)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 33, 38))
            .bg(Color::Rgb(7, 35, 40)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 29, 37))
            .bg(Color::Rgb(4, 22, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 22, 34))
            .bg(Color::Rgb(3, 15, 31)),
    ));
    lines.push(Line::from(spans));
    // Row 3
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 20, 34))
            .bg(Color::Rgb(3, 16, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 22, 34))
            .bg(Color::Rgb(4, 19, 33)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 46, 46))
            .bg(Color::Rgb(8, 50, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 56, 50))
            .bg(Color::Rgb(8, 48, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 55, 51))
            .bg(Color::Rgb(7, 40, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 95, 70))
            .bg(Color::Rgb(12, 79, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 73, 61))
            .bg(Color::Rgb(4, 40, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 99, 74))
            .bg(Color::Rgb(15, 97, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 90, 71))
            .bg(Color::Rgb(21, 116, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 41, 51))
            .bg(Color::Rgb(20, 87, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 48, 55))
            .bg(Color::Rgb(9, 65, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 88, 73))
            .bg(Color::Rgb(24, 75, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 37, 55))
            .bg(Color::Rgb(29, 46, 58)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 87, 65))
            .bg(Color::Rgb(37, 105, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(67, 151, 87))
            .bg(Color::Rgb(57, 142, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 31, 39))
            .bg(Color::Rgb(34, 63, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 27, 60))
            .bg(Color::Rgb(53, 83, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 64, 66))
            .bg(Color::Rgb(67, 133, 90)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 47, 58))
            .bg(Color::Rgb(29, 33, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 50, 67))
            .bg(Color::Rgb(39, 52, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 44, 65))
            .bg(Color::Rgb(53, 74, 82)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 31, 62))
            .bg(Color::Rgb(55, 82, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 22, 58))
            .bg(Color::Rgb(53, 91, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(24, 20, 55))
            .bg(Color::Rgb(51, 86, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 22, 58))
            .bg(Color::Rgb(51, 83, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(28, 24, 57))
            .bg(Color::Rgb(58, 101, 92)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(34, 34, 63))
            .bg(Color::Rgb(52, 84, 85)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(34, 39, 65))
            .bg(Color::Rgb(40, 51, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 46, 63))
            .bg(Color::Rgb(30, 36, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 44, 57))
            .bg(Color::Rgb(29, 34, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 62, 64))
            .bg(Color::Rgb(64, 132, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 26, 59))
            .bg(Color::Rgb(51, 81, 82)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 31, 38))
            .bg(Color::Rgb(35, 66, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(64, 155, 86))
            .bg(Color::Rgb(56, 144, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 86, 63))
            .bg(Color::Rgb(35, 103, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 74, 63))
            .bg(Color::Rgb(37, 59, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 131, 91))
            .bg(Color::Rgb(33, 98, 79)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 115, 86))
            .bg(Color::Rgb(11, 107, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 48, 54))
            .bg(Color::Rgb(17, 75, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 54, 56))
            .bg(Color::Rgb(22, 90, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 55, 58))
            .bg(Color::Rgb(25, 100, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 55, 56))
            .bg(Color::Rgb(23, 87, 73)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 61, 57))
            .bg(Color::Rgb(24, 92, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 59, 57))
            .bg(Color::Rgb(20, 83, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 49, 48))
            .bg(Color::Rgb(6, 37, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 38, 43))
            .bg(Color::Rgb(7, 44, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 38, 42))
            .bg(Color::Rgb(11, 48, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 28, 36))
            .bg(Color::Rgb(9, 41, 41)),
    ));
    lines.push(Line::from(spans));
    // Row 4
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 22, 34))
            .bg(Color::Rgb(3, 19, 33)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 40, 43))
            .bg(Color::Rgb(4, 26, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 79, 62))
            .bg(Color::Rgb(7, 51, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 67, 56))
            .bg(Color::Rgb(7, 47, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 34, 42))
            .bg(Color::Rgb(5, 35, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 42, 46))
            .bg(Color::Rgb(8, 64, 57)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(23, 88, 78))
            .bg(Color::Rgb(4, 47, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 104, 89))
            .bg(Color::Rgb(5, 68, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 120, 100))
            .bg(Color::Rgb(8, 80, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 69, 63))
            .bg(Color::Rgb(12, 66, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 54, 61))
            .bg(Color::Rgb(4, 53, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 110, 85))
            .bg(Color::Rgb(16, 94, 78)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 71, 66))
            .bg(Color::Rgb(41, 74, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 40, 49))
            .bg(Color::Rgb(39, 82, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(58, 137, 77))
            .bg(Color::Rgb(73, 167, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(58, 121, 76))
            .bg(Color::Rgb(38, 84, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 55, 64))
            .bg(Color::Rgb(23, 41, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(71, 63, 105))
            .bg(Color::Rgb(31, 40, 57)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(66, 58, 105))
            .bg(Color::Rgb(44, 43, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(44, 42, 76))
            .bg(Color::Rgb(41, 40, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(48, 45, 83))
            .bg(Color::Rgb(33, 33, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(58, 58, 93))
            .bg(Color::Rgb(36, 35, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(60, 101, 81))
            .bg(Color::Rgb(45, 63, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(56, 49, 95))
            .bg(Color::Rgb(45, 50, 82)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(58, 54, 96))
            .bg(Color::Rgb(46, 47, 85)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(57, 52, 95))
            .bg(Color::Rgb(40, 42, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(56, 53, 93))
            .bg(Color::Rgb(38, 38, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(49, 47, 84))
            .bg(Color::Rgb(37, 38, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(46, 43, 79))
            .bg(Color::Rgb(45, 45, 82)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(72, 68, 113))
            .bg(Color::Rgb(37, 46, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(56, 74, 91))
            .bg(Color::Rgb(25, 41, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(27, 55, 56))
            .bg(Color::Rgb(24, 42, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(57, 127, 77))
            .bg(Color::Rgb(38, 84, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(54, 128, 76))
            .bg(Color::Rgb(71, 166, 87)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 65, 52))
            .bg(Color::Rgb(34, 78, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(44, 89, 66))
            .bg(Color::Rgb(37, 55, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 101, 83))
            .bg(Color::Rgb(26, 104, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 72, 68))
            .bg(Color::Rgb(11, 98, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 53, 59))
            .bg(Color::Rgb(4, 42, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 78, 70))
            .bg(Color::Rgb(21, 88, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 90, 74))
            .bg(Color::Rgb(30, 121, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 69, 63))
            .bg(Color::Rgb(31, 124, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 55, 57))
            .bg(Color::Rgb(30, 123, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 83, 70))
            .bg(Color::Rgb(29, 121, 91)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 75, 61))
            .bg(Color::Rgb(13, 68, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 42, 43))
            .bg(Color::Rgb(9, 58, 49)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 29, 39))
            .bg(Color::Rgb(12, 70, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 26, 36))
            .bg(Color::Rgb(5, 24, 37)),
    ));
    lines.push(Line::from(spans));
    // Row 5
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 21, 35))
            .bg(Color::Rgb(8, 41, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 55, 48))
            .bg(Color::Rgb(8, 45, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 79, 63))
            .bg(Color::Rgb(6, 42, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 60, 55))
            .bg(Color::Rgb(7, 40, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 31, 41))
            .bg(Color::Rgb(6, 39, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 48, 49))
            .bg(Color::Rgb(11, 69, 57)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 43, 53))
            .bg(Color::Rgb(19, 79, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 78, 71))
            .bg(Color::Rgb(51, 142, 121)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 95, 78))
            .bg(Color::Rgb(34, 128, 108)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(69, 166, 145))
            .bg(Color::Rgb(23, 94, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 145, 115))
            .bg(Color::Rgb(13, 89, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 106, 85))
            .bg(Color::Rgb(43, 109, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 132, 103))
            .bg(Color::Rgb(37, 129, 93)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 82, 72))
            .bg(Color::Rgb(32, 61, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 60, 68))
            .bg(Color::Rgb(30, 67, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(46, 78, 76))
            .bg(Color::Rgb(32, 61, 57)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(99, 85, 140))
            .bg(Color::Rgb(95, 90, 133)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(99, 68, 134))
            .bg(Color::Rgb(133, 97, 177)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(85, 63, 116))
            .bg(Color::Rgb(72, 55, 102)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(117, 83, 153))
            .bg(Color::Rgb(86, 66, 120)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(106, 72, 140))
            .bg(Color::Rgb(80, 58, 115)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(136, 111, 164))
            .bg(Color::Rgb(83, 82, 112)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(126, 158, 109))
            .bg(Color::Rgb(86, 132, 95)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(141, 92, 180))
            .bg(Color::Rgb(77, 52, 116)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(143, 100, 180))
            .bg(Color::Rgb(84, 66, 121)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(136, 95, 174))
            .bg(Color::Rgb(85, 66, 122)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(103, 74, 136))
            .bg(Color::Rgb(78, 61, 111)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(116, 83, 152))
            .bg(Color::Rgb(91, 71, 128)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(114, 82, 150))
            .bg(Color::Rgb(74, 58, 108)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(66, 51, 98))
            .bg(Color::Rgb(86, 63, 121)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(100, 78, 139))
            .bg(Color::Rgb(136, 112, 187)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(68, 92, 103))
            .bg(Color::Rgb(54, 76, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 60, 59))
            .bg(Color::Rgb(28, 58, 54)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 59, 69))
            .bg(Color::Rgb(30, 72, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 65, 64))
            .bg(Color::Rgb(31, 52, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(50, 146, 95))
            .bg(Color::Rgb(51, 126, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(60, 151, 96))
            .bg(Color::Rgb(42, 117, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 88, 76))
            .bg(Color::Rgb(15, 86, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 86, 81))
            .bg(Color::Rgb(12, 74, 73)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 96, 83))
            .bg(Color::Rgb(15, 89, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(23, 100, 82))
            .bg(Color::Rgb(18, 92, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 103, 83))
            .bg(Color::Rgb(12, 60, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 103, 83))
            .bg(Color::Rgb(14, 65, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 104, 81))
            .bg(Color::Rgb(14, 66, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 53, 54))
            .bg(Color::Rgb(5, 32, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 38, 43))
            .bg(Color::Rgb(6, 34, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 52, 49))
            .bg(Color::Rgb(10, 56, 49)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 62, 52))
            .bg(Color::Rgb(11, 61, 50)),
    ));
    lines.push(Line::from(spans));
    // Row 6
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 9, 32))
            .bg(Color::Rgb(0, 9, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 33, 42))
            .bg(Color::Rgb(6, 38, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 48, 48))
            .bg(Color::Rgb(9, 68, 58)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 50, 54))
            .bg(Color::Rgb(7, 55, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(17, 76, 73))
            .bg(Color::Rgb(7, 44, 52)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 110, 89))
            .bg(Color::Rgb(9, 63, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 119, 97))
            .bg(Color::Rgb(8, 52, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(23, 133, 103))
            .bg(Color::Rgb(11, 97, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 116, 93))
            .bg(Color::Rgb(12, 109, 87)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 93, 89))
            .bg(Color::Rgb(0, 68, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 110, 97))
            .bg(Color::Rgb(16, 99, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 114, 92))
            .bg(Color::Rgb(41, 107, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 119, 93))
            .bg(Color::Rgb(17, 140, 111)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 73, 82))
            .bg(Color::Rgb(38, 92, 79)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(34, 56, 61))
            .bg(Color::Rgb(31, 47, 58)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(56, 84, 95))
            .bg(Color::Rgb(47, 85, 73)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(50, 85, 73))
            .bg(Color::Rgb(67, 115, 91)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 52, 65))
            .bg(Color::Rgb(48, 51, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(44, 38, 84))
            .bg(Color::Rgb(75, 56, 110)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(109, 69, 91))
            .bg(Color::Rgb(70, 54, 96)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(79, 55, 80))
            .bg(Color::Rgb(75, 54, 98)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(51, 33, 92))
            .bg(Color::Rgb(104, 73, 142)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(73, 108, 94))
            .bg(Color::Rgb(115, 162, 103)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(77, 115, 93))
            .bg(Color::Rgb(122, 93, 149)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(72, 43, 108))
            .bg(Color::Rgb(123, 83, 161)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(56, 47, 90))
            .bg(Color::Rgb(108, 76, 144)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(59, 43, 76))
            .bg(Color::Rgb(74, 53, 101)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(81, 49, 72))
            .bg(Color::Rgb(55, 42, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 40, 82))
            .bg(Color::Rgb(91, 69, 131)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(50, 42, 84))
            .bg(Color::Rgb(43, 35, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 60, 54))
            .bg(Color::Rgb(40, 69, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(80, 125, 87))
            .bg(Color::Rgb(47, 86, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(45, 78, 68))
            .bg(Color::Rgb(35, 62, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 31, 43))
            .bg(Color::Rgb(28, 42, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 66, 80))
            .bg(Color::Rgb(41, 96, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 71, 63))
            .bg(Color::Rgb(42, 121, 93)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(28, 116, 100))
            .bg(Color::Rgb(43, 115, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 135, 116))
            .bg(Color::Rgb(16, 111, 97)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 97, 88))
            .bg(Color::Rgb(12, 85, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 107, 94))
            .bg(Color::Rgb(8, 76, 73)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 96, 87))
            .bg(Color::Rgb(9, 75, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 78, 76))
            .bg(Color::Rgb(11, 67, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 88, 79))
            .bg(Color::Rgb(12, 59, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 86, 74))
            .bg(Color::Rgb(16, 75, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 52, 51))
            .bg(Color::Rgb(10, 54, 54)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 57, 51))
            .bg(Color::Rgb(8, 46, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 83, 63))
            .bg(Color::Rgb(10, 65, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 22, 36))
            .bg(Color::Rgb(5, 37, 41)),
    ));
    lines.push(Line::from(spans));
    // Row 7
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 13, 34))
            .bg(Color::Rgb(0, 9, 33)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 39, 45))
            .bg(Color::Rgb(6, 43, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 79, 64))
            .bg(Color::Rgb(9, 57, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 43, 49))
            .bg(Color::Rgb(3, 34, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 60, 62))
            .bg(Color::Rgb(10, 51, 57)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 119, 93))
            .bg(Color::Rgb(25, 108, 87)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(24, 102, 85))
            .bg(Color::Rgb(28, 119, 95)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(28, 141, 107))
            .bg(Color::Rgb(25, 141, 106)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 156, 116))
            .bg(Color::Rgb(12, 109, 90)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(28, 138, 108))
            .bg(Color::Rgb(11, 88, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 130, 110))
            .bg(Color::Rgb(9, 103, 96)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 148, 117))
            .bg(Color::Rgb(11, 132, 113)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(50, 106, 101))
            .bg(Color::Rgb(38, 101, 82)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(28, 43, 49))
            .bg(Color::Rgb(29, 41, 54)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 64, 76))
            .bg(Color::Rgb(39, 71, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(65, 90, 98))
            .bg(Color::Rgb(64, 92, 99)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(57, 71, 84))
            .bg(Color::Rgb(62, 74, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 27, 57))
            .bg(Color::Rgb(30, 23, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(114, 76, 88))
            .bg(Color::Rgb(29, 25, 52)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(200, 170, 160))
            .bg(Color::Rgb(213, 158, 157)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(126, 100, 109))
            .bg(Color::Rgb(123, 89, 105)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(56, 35, 60))
            .bg(Color::Rgb(25, 21, 54)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(57, 38, 75))
            .bg(Color::Rgb(58, 44, 90)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(60, 46, 84))
            .bg(Color::Rgb(57, 106, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(55, 70, 72))
            .bg(Color::Rgb(52, 60, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(66, 46, 70))
            .bg(Color::Rgb(44, 31, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(80, 57, 76))
            .bg(Color::Rgb(68, 49, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(184, 157, 152))
            .bg(Color::Rgb(214, 156, 158)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(148, 113, 119))
            .bg(Color::Rgb(38, 28, 52)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(51, 35, 60))
            .bg(Color::Rgb(27, 30, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 18, 56))
            .bg(Color::Rgb(39, 41, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(70, 92, 96))
            .bg(Color::Rgb(70, 100, 96)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(52, 67, 85))
            .bg(Color::Rgb(48, 69, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 47, 62))
            .bg(Color::Rgb(28, 48, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 53, 53))
            .bg(Color::Rgb(33, 49, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(52, 107, 101))
            .bg(Color::Rgb(36, 121, 93)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(28, 175, 139))
            .bg(Color::Rgb(19, 157, 137)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 120, 109))
            .bg(Color::Rgb(18, 131, 112)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(17, 113, 103))
            .bg(Color::Rgb(13, 104, 95)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 92, 88))
            .bg(Color::Rgb(16, 92, 85)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 64, 68))
            .bg(Color::Rgb(24, 106, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 58, 64))
            .bg(Color::Rgb(28, 117, 93)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 39, 53))
            .bg(Color::Rgb(19, 82, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 37, 49))
            .bg(Color::Rgb(7, 55, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 58, 55))
            .bg(Color::Rgb(15, 90, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 58, 52))
            .bg(Color::Rgb(11, 72, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 45, 47))
            .bg(Color::Rgb(7, 53, 50)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 9, 32))
            .bg(Color::Rgb(2, 17, 35)),
    ));
    lines.push(Line::from(spans));
    // Row 8
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 12, 34))
            .bg(Color::Rgb(2, 16, 36)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 34, 44))
            .bg(Color::Rgb(5, 30, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 85, 67))
            .bg(Color::Rgb(12, 83, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 47, 51))
            .bg(Color::Rgb(4, 51, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 68, 67))
            .bg(Color::Rgb(13, 57, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 115, 91))
            .bg(Color::Rgb(27, 115, 91)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(24, 111, 91))
            .bg(Color::Rgb(21, 104, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 157, 115))
            .bg(Color::Rgb(19, 123, 98)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 156, 115))
            .bg(Color::Rgb(9, 112, 93)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 131, 110))
            .bg(Color::Rgb(8, 90, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 151, 116))
            .bg(Color::Rgb(9, 128, 108)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(49, 87, 85))
            .bg(Color::Rgb(42, 127, 107)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 62, 70))
            .bg(Color::Rgb(37, 45, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(78, 155, 99))
            .bg(Color::Rgb(60, 111, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 55, 72))
            .bg(Color::Rgb(42, 62, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(60, 61, 88))
            .bg(Color::Rgb(66, 81, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(69, 72, 86))
            .bg(Color::Rgb(54, 63, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 35, 45))
            .bg(Color::Rgb(92, 69, 79)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(53, 64, 79))
            .bg(Color::Rgb(147, 134, 112)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 89, 71))
            .bg(Color::Rgb(154, 136, 119)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 82, 60))
            .bg(Color::Rgb(193, 150, 147)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(145, 106, 112))
            .bg(Color::Rgb(148, 101, 113)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(243, 208, 191))
            .bg(Color::Rgb(117, 83, 97)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(210, 166, 159))
            .bg(Color::Rgb(79, 46, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(185, 139, 142))
            .bg(Color::Rgb(70, 39, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(226, 180, 172))
            .bg(Color::Rgb(96, 67, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(130, 97, 103))
            .bg(Color::Rgb(166, 125, 130)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 92, 64))
            .bg(Color::Rgb(185, 149, 143)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 70, 55))
            .bg(Color::Rgb(167, 150, 130)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(59, 65, 76))
            .bg(Color::Rgb(127, 108, 95)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(53, 40, 53))
            .bg(Color::Rgb(72, 48, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(69, 83, 89))
            .bg(Color::Rgb(67, 89, 93)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 30, 70))
            .bg(Color::Rgb(47, 47, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(54, 81, 81))
            .bg(Color::Rgb(56, 82, 92)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(88, 158, 109))
            .bg(Color::Rgb(65, 114, 85)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 49, 82))
            .bg(Color::Rgb(46, 50, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(56, 83, 97))
            .bg(Color::Rgb(49, 118, 106)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 118, 92))
            .bg(Color::Rgb(9, 120, 105)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(23, 121, 98))
            .bg(Color::Rgb(14, 118, 108)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 95, 89))
            .bg(Color::Rgb(17, 108, 97)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 77, 78))
            .bg(Color::Rgb(20, 106, 93)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 72, 75))
            .bg(Color::Rgb(28, 105, 95)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 52, 61))
            .bg(Color::Rgb(20, 80, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 51, 55))
            .bg(Color::Rgb(12, 66, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 64, 58))
            .bg(Color::Rgb(11, 75, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 59, 54))
            .bg(Color::Rgb(10, 66, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 56, 51))
            .bg(Color::Rgb(9, 63, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 13, 33))
            .bg(Color::Rgb(2, 14, 34)),
    ));
    lines.push(Line::from(spans));
    // Row 9
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 11, 33))
            .bg(Color::Rgb(1, 13, 34)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 45, 48))
            .bg(Color::Rgb(5, 37, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 48, 51))
            .bg(Color::Rgb(11, 68, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 28, 44))
            .bg(Color::Rgb(5, 43, 50)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 45, 55))
            .bg(Color::Rgb(9, 46, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 47, 56))
            .bg(Color::Rgb(5, 43, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 59, 65))
            .bg(Color::Rgb(7, 57, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 80, 79))
            .bg(Color::Rgb(8, 99, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 91, 85))
            .bg(Color::Rgb(8, 112, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(24, 138, 114))
            .bg(Color::Rgb(2, 90, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 127, 93))
            .bg(Color::Rgb(35, 119, 96)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(47, 121, 109))
            .bg(Color::Rgb(53, 120, 101)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 48, 78))
            .bg(Color::Rgb(33, 43, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 73, 64))
            .bg(Color::Rgb(58, 111, 79)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 66, 70))
            .bg(Color::Rgb(51, 70, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(57, 60, 89))
            .bg(Color::Rgb(52, 42, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 27, 60))
            .bg(Color::Rgb(56, 60, 78)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(109, 94, 102))
            .bg(Color::Rgb(128, 80, 102)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(194, 202, 153))
            .bg(Color::Rgb(189, 178, 193)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(180, 196, 155))
            .bg(Color::Rgb(99, 203, 132)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(228, 208, 184))
            .bg(Color::Rgb(81, 193, 114)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(255, 232, 211))
            .bg(Color::Rgb(176, 174, 174)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(253, 222, 199))
            .bg(Color::Rgb(255, 230, 204)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(253, 219, 198))
            .bg(Color::Rgb(255, 231, 205)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(251, 218, 197))
            .bg(Color::Rgb(255, 234, 207)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(251, 223, 200))
            .bg(Color::Rgb(255, 228, 204)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(255, 231, 210))
            .bg(Color::Rgb(157, 169, 165)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(222, 202, 175))
            .bg(Color::Rgb(82, 207, 112)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(180, 196, 153))
            .bg(Color::Rgb(111, 200, 144)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(185, 192, 146))
            .bg(Color::Rgb(192, 165, 183)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(79, 67, 83))
            .bg(Color::Rgb(102, 67, 85)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(53, 49, 76))
            .bg(Color::Rgb(56, 54, 82)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(45, 49, 79))
            .bg(Color::Rgb(41, 30, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(44, 96, 65))
            .bg(Color::Rgb(46, 76, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 67, 64))
            .bg(Color::Rgb(55, 105, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 56, 69))
            .bg(Color::Rgb(29, 28, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(52, 86, 87))
            .bg(Color::Rgb(55, 75, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 136, 114))
            .bg(Color::Rgb(27, 141, 112)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 119, 106))
            .bg(Color::Rgb(12, 112, 100)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 96, 91))
            .bg(Color::Rgb(15, 104, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 87, 83))
            .bg(Color::Rgb(27, 121, 105)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 74, 74))
            .bg(Color::Rgb(20, 93, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 71, 71))
            .bg(Color::Rgb(31, 112, 99)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 63, 66))
            .bg(Color::Rgb(18, 89, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 70, 67))
            .bg(Color::Rgb(15, 69, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 63, 55))
            .bg(Color::Rgb(8, 52, 50)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 70, 59))
            .bg(Color::Rgb(10, 60, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 23, 37))
            .bg(Color::Rgb(2, 20, 36)),
    ));
    lines.push(Line::from(spans));
    // Row 10
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 29, 40))
            .bg(Color::Rgb(1, 14, 34)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 17, 35))
            .bg(Color::Rgb(7, 42, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(17, 99, 74))
            .bg(Color::Rgb(8, 52, 52)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 72, 63))
            .bg(Color::Rgb(2, 31, 45)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 52, 62))
            .bg(Color::Rgb(16, 64, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 64, 68))
            .bg(Color::Rgb(12, 71, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 80, 79))
            .bg(Color::Rgb(10, 67, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 73, 76))
            .bg(Color::Rgb(19, 107, 95)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 130, 114))
            .bg(Color::Rgb(14, 109, 98)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(24, 131, 119))
            .bg(Color::Rgb(15, 118, 108)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 131, 111))
            .bg(Color::Rgb(35, 151, 106)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(48, 146, 105))
            .bg(Color::Rgb(46, 130, 103)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(45, 120, 96))
            .bg(Color::Rgb(40, 72, 85)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 102, 83))
            .bg(Color::Rgb(29, 59, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 82, 75))
            .bg(Color::Rgb(26, 53, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(68, 89, 96))
            .bg(Color::Rgb(72, 82, 96)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(78, 70, 90))
            .bg(Color::Rgb(53, 49, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 42, 50))
            .bg(Color::Rgb(34, 19, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(151, 127, 127))
            .bg(Color::Rgb(126, 163, 115)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(245, 191, 181))
            .bg(Color::Rgb(185, 223, 156)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(249, 216, 196))
            .bg(Color::Rgb(255, 203, 195)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(249, 216, 197))
            .bg(Color::Rgb(252, 219, 197)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(248, 219, 197))
            .bg(Color::Rgb(255, 223, 201)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(249, 212, 193))
            .bg(Color::Rgb(244, 195, 183)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(250, 217, 197))
            .bg(Color::Rgb(249, 206, 190)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(244, 216, 194))
            .bg(Color::Rgb(255, 228, 204)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(245, 213, 193))
            .bg(Color::Rgb(253, 217, 196)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(249, 214, 194))
            .bg(Color::Rgb(255, 203, 193)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(248, 195, 187))
            .bg(Color::Rgb(170, 224, 148)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(158, 123, 126))
            .bg(Color::Rgb(118, 136, 102)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(51, 58, 65))
            .bg(Color::Rgb(43, 20, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(80, 85, 96))
            .bg(Color::Rgb(73, 84, 93)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 48, 66))
            .bg(Color::Rgb(39, 58, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 88, 72))
            .bg(Color::Rgb(39, 94, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(54, 118, 92))
            .bg(Color::Rgb(32, 62, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 135, 100))
            .bg(Color::Rgb(43, 98, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 177, 129))
            .bg(Color::Rgb(46, 123, 92)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 163, 128))
            .bg(Color::Rgb(34, 141, 112)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 150, 121))
            .bg(Color::Rgb(16, 136, 117)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 146, 114))
            .bg(Color::Rgb(11, 95, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(23, 124, 100))
            .bg(Color::Rgb(8, 83, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 90, 81))
            .bg(Color::Rgb(7, 64, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 92, 80))
            .bg(Color::Rgb(7, 55, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 82, 73))
            .bg(Color::Rgb(5, 41, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 71, 64))
            .bg(Color::Rgb(5, 39, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 60, 55))
            .bg(Color::Rgb(9, 60, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 13, 34))
            .bg(Color::Rgb(6, 44, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 41, 44))
            .bg(Color::Rgb(3, 21, 36)),
    ));
    lines.push(Line::from(spans));
    // Row 11
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 23, 37))
            .bg(Color::Rgb(6, 34, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 11, 33))
            .bg(Color::Rgb(0, 5, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 81, 64))
            .bg(Color::Rgb(19, 116, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 66, 58))
            .bg(Color::Rgb(13, 90, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 44, 55))
            .bg(Color::Rgb(4, 34, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 71, 72))
            .bg(Color::Rgb(24, 94, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(34, 122, 105))
            .bg(Color::Rgb(23, 94, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 89, 84))
            .bg(Color::Rgb(19, 93, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 82, 81))
            .bg(Color::Rgb(12, 92, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 118, 106))
            .bg(Color::Rgb(13, 109, 101)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 110, 103))
            .bg(Color::Rgb(12, 128, 116)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(27, 161, 121))
            .bg(Color::Rgb(27, 158, 122)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 72, 81))
            .bg(Color::Rgb(43, 131, 91)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 113, 65))
            .bg(Color::Rgb(34, 92, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(57, 149, 69))
            .bg(Color::Rgb(38, 101, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 81, 67))
            .bg(Color::Rgb(38, 71, 78)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(61, 59, 87))
            .bg(Color::Rgb(84, 80, 95)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(58, 77, 76))
            .bg(Color::Rgb(34, 65, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 163, 86))
            .bg(Color::Rgb(77, 147, 110)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(59, 152, 98))
            .bg(Color::Rgb(226, 197, 185)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(172, 177, 154))
            .bg(Color::Rgb(255, 218, 203)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(252, 209, 195))
            .bg(Color::Rgb(241, 208, 189)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(255, 222, 204))
            .bg(Color::Rgb(220, 182, 168)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(249, 198, 187))
            .bg(Color::Rgb(244, 192, 181)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(250, 200, 188))
            .bg(Color::Rgb(243, 192, 180)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(255, 223, 206))
            .bg(Color::Rgb(223, 186, 171)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(249, 206, 192))
            .bg(Color::Rgb(243, 208, 190)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(162, 166, 144))
            .bg(Color::Rgb(255, 217, 203)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(52, 149, 93))
            .bg(Color::Rgb(217, 191, 177)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(47, 144, 79))
            .bg(Color::Rgb(60, 121, 91)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(79, 74, 86))
            .bg(Color::Rgb(72, 103, 87)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(44, 64, 67))
            .bg(Color::Rgb(61, 50, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(53, 111, 66))
            .bg(Color::Rgb(27, 57, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(89, 195, 83))
            .bg(Color::Rgb(57, 134, 85)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(47, 139, 75))
            .bg(Color::Rgb(44, 116, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 50, 73))
            .bg(Color::Rgb(34, 108, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 141, 109))
            .bg(Color::Rgb(37, 173, 139)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 129, 115))
            .bg(Color::Rgb(12, 134, 118)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 114, 102))
            .bg(Color::Rgb(12, 120, 105)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 112, 100))
            .bg(Color::Rgb(20, 120, 104)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 136, 114))
            .bg(Color::Rgb(24, 115, 100)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 128, 106))
            .bg(Color::Rgb(26, 109, 95)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 61, 64))
            .bg(Color::Rgb(12, 67, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 47, 55))
            .bg(Color::Rgb(7, 50, 54)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 24, 41))
            .bg(Color::Rgb(6, 45, 51)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 18, 38))
            .bg(Color::Rgb(5, 43, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 23, 38))
            .bg(Color::Rgb(1, 20, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 27, 39))
            .bg(Color::Rgb(8, 40, 44)),
    ));
    lines.push(Line::from(spans));
    // Row 12
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 20, 34))
            .bg(Color::Rgb(5, 28, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 20, 35))
            .bg(Color::Rgb(2, 17, 34)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 72, 61))
            .bg(Color::Rgb(13, 75, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 47, 49))
            .bg(Color::Rgb(7, 55, 54)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 30, 43))
            .bg(Color::Rgb(34, 110, 98)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 57, 62))
            .bg(Color::Rgb(38, 117, 104)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 62, 68))
            .bg(Color::Rgb(28, 108, 98)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 84, 81))
            .bg(Color::Rgb(36, 121, 108)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 109, 98))
            .bg(Color::Rgb(26, 109, 99)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(17, 100, 91))
            .bg(Color::Rgb(11, 94, 90)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(24, 145, 120))
            .bg(Color::Rgb(19, 139, 120)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 178, 134))
            .bg(Color::Rgb(33, 154, 115)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(45, 152, 100))
            .bg(Color::Rgb(43, 106, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(60, 131, 90))
            .bg(Color::Rgb(31, 64, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(59, 102, 90))
            .bg(Color::Rgb(61, 122, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 64, 72))
            .bg(Color::Rgb(58, 112, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 32, 43))
            .bg(Color::Rgb(55, 75, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 25, 68))
            .bg(Color::Rgb(67, 63, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(96, 136, 83))
            .bg(Color::Rgb(81, 187, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(120, 196, 92))
            .bg(Color::Rgb(66, 182, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(119, 195, 87))
            .bg(Color::Rgb(44, 161, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(71, 81, 74))
            .bg(Color::Rgb(85, 85, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(54, 29, 48))
            .bg(Color::Rgb(169, 140, 135)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 28, 38))
            .bg(Color::Rgb(244, 196, 183)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 25, 35))
            .bg(Color::Rgb(243, 194, 183)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(52, 27, 41))
            .bg(Color::Rgb(164, 137, 129)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(82, 51, 72))
            .bg(Color::Rgb(81, 60, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(87, 145, 79))
            .bg(Color::Rgb(50, 154, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(101, 157, 79))
            .bg(Color::Rgb(70, 181, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(64, 62, 69))
            .bg(Color::Rgb(70, 89, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 25, 50))
            .bg(Color::Rgb(62, 44, 78)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 43, 53))
            .bg(Color::Rgb(48, 99, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(48, 78, 83))
            .bg(Color::Rgb(64, 113, 73)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(70, 130, 99))
            .bg(Color::Rgb(56, 96, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(66, 135, 86))
            .bg(Color::Rgb(41, 66, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 102, 81))
            .bg(Color::Rgb(58, 129, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 152, 119))
            .bg(Color::Rgb(46, 163, 113)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 173, 141))
            .bg(Color::Rgb(20, 148, 127)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 129, 111))
            .bg(Color::Rgb(21, 124, 109)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 101, 90))
            .bg(Color::Rgb(12, 92, 87)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 122, 106))
            .bg(Color::Rgb(19, 92, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 53, 62))
            .bg(Color::Rgb(8, 51, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 59, 63))
            .bg(Color::Rgb(25, 103, 90)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 58, 60))
            .bg(Color::Rgb(42, 137, 111)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 28, 43))
            .bg(Color::Rgb(2, 27, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 26, 41))
            .bg(Color::Rgb(2, 21, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 27, 39))
            .bg(Color::Rgb(4, 27, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 23, 36))
            .bg(Color::Rgb(4, 24, 37)),
    ));
    lines.push(Line::from(spans));
    // Row 13
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 29, 38))
            .bg(Color::Rgb(0, 12, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 21, 35))
            .bg(Color::Rgb(3, 20, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 38, 43))
            .bg(Color::Rgb(11, 75, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 53, 55))
            .bg(Color::Rgb(9, 57, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 54, 58))
            .bg(Color::Rgb(15, 59, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 57, 62))
            .bg(Color::Rgb(17, 70, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 98, 89))
            .bg(Color::Rgb(9, 48, 57)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 105, 92))
            .bg(Color::Rgb(15, 67, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(17, 82, 78))
            .bg(Color::Rgb(29, 115, 101)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 108, 99))
            .bg(Color::Rgb(28, 126, 106)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 113, 104))
            .bg(Color::Rgb(23, 137, 115)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 131, 105))
            .bg(Color::Rgb(21, 157, 123)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 82, 90))
            .bg(Color::Rgb(33, 117, 97)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 22, 55))
            .bg(Color::Rgb(20, 42, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 30, 64))
            .bg(Color::Rgb(46, 97, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 80, 69))
            .bg(Color::Rgb(99, 173, 103)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(66, 121, 82))
            .bg(Color::Rgb(100, 164, 103)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 73, 78))
            .bg(Color::Rgb(39, 68, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 48, 76))
            .bg(Color::Rgb(24, 15, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 46, 71))
            .bg(Color::Rgb(37, 54, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 59, 80))
            .bg(Color::Rgb(54, 85, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(67, 135, 92))
            .bg(Color::Rgb(32, 63, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 51, 75))
            .bg(Color::Rgb(35, 49, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(48, 97, 70))
            .bg(Color::Rgb(19, 38, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(45, 90, 66))
            .bg(Color::Rgb(18, 35, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 66, 80))
            .bg(Color::Rgb(42, 55, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(78, 163, 113))
            .bg(Color::Rgb(34, 67, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 67, 82))
            .bg(Color::Rgb(39, 65, 79)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 47, 69))
            .bg(Color::Rgb(26, 31, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 54, 79))
            .bg(Color::Rgb(23, 22, 54)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 66, 81))
            .bg(Color::Rgb(40, 72, 78)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(53, 89, 83))
            .bg(Color::Rgb(110, 183, 108)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(34, 54, 80))
            .bg(Color::Rgb(111, 190, 109)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 32, 66))
            .bg(Color::Rgb(49, 96, 82)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(23, 29, 52))
            .bg(Color::Rgb(15, 20, 54)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 29, 61))
            .bg(Color::Rgb(25, 63, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 98, 90))
            .bg(Color::Rgb(36, 144, 110)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 86, 80))
            .bg(Color::Rgb(10, 113, 105)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 74, 79))
            .bg(Color::Rgb(17, 108, 96)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 91, 85))
            .bg(Color::Rgb(31, 135, 110)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 106, 88))
            .bg(Color::Rgb(32, 126, 103)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 117, 98))
            .bg(Color::Rgb(14, 65, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(23, 82, 75))
            .bg(Color::Rgb(23, 89, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 83, 74))
            .bg(Color::Rgb(9, 50, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 39, 46))
            .bg(Color::Rgb(6, 37, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 25, 38))
            .bg(Color::Rgb(4, 31, 42)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 34, 42))
            .bg(Color::Rgb(6, 38, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 20, 35))
            .bg(Color::Rgb(4, 27, 37)),
    ));
    lines.push(Line::from(spans));
    // Row 14
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 25, 36))
            .bg(Color::Rgb(5, 25, 36)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 32, 39))
            .bg(Color::Rgb(3, 21, 36)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 24, 35))
            .bg(Color::Rgb(4, 25, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 48, 52))
            .bg(Color::Rgb(2, 22, 36)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 89, 79))
            .bg(Color::Rgb(24, 84, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 62, 63))
            .bg(Color::Rgb(16, 66, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 135, 106))
            .bg(Color::Rgb(35, 119, 98)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(27, 95, 87))
            .bg(Color::Rgb(20, 87, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 64, 72))
            .bg(Color::Rgb(14, 71, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 61, 67))
            .bg(Color::Rgb(10, 75, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 94, 81))
            .bg(Color::Rgb(5, 91, 89)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 132, 111))
            .bg(Color::Rgb(30, 118, 92)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(65, 172, 105))
            .bg(Color::Rgb(23, 71, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(49, 105, 78))
            .bg(Color::Rgb(14, 38, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 31, 57))
            .bg(Color::Rgb(22, 36, 50)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 23, 51))
            .bg(Color::Rgb(25, 41, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 20, 39))
            .bg(Color::Rgb(27, 47, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 20, 45))
            .bg(Color::Rgb(17, 20, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 39, 40))
            .bg(Color::Rgb(37, 65, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(60, 120, 64))
            .bg(Color::Rgb(53, 93, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 22, 58))
            .bg(Color::Rgb(32, 42, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 47, 74))
            .bg(Color::Rgb(42, 71, 78)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 29, 63))
            .bg(Color::Rgb(26, 31, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(54, 107, 72))
            .bg(Color::Rgb(70, 139, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(57, 111, 75))
            .bg(Color::Rgb(70, 136, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 39, 72))
            .bg(Color::Rgb(33, 56, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 60, 76))
            .bg(Color::Rgb(75, 173, 111)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(23, 30, 67))
            .bg(Color::Rgb(40, 70, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(56, 106, 64))
            .bg(Color::Rgb(47, 80, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 63, 46))
            .bg(Color::Rgb(43, 78, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 8, 37))
            .bg(Color::Rgb(26, 29, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 21, 36))
            .bg(Color::Rgb(25, 33, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(17, 20, 43))
            .bg(Color::Rgb(26, 35, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 40, 64))
            .bg(Color::Rgb(30, 43, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(45, 83, 76))
            .bg(Color::Rgb(11, 19, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(80, 166, 100))
            .bg(Color::Rgb(18, 56, 51)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 106, 95))
            .bg(Color::Rgb(31, 87, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 120, 97))
            .bg(Color::Rgb(16, 95, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 97, 84))
            .bg(Color::Rgb(0, 51, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 93, 86))
            .bg(Color::Rgb(15, 76, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 42, 52))
            .bg(Color::Rgb(14, 66, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 112, 97))
            .bg(Color::Rgb(32, 126, 97)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 125, 98))
            .bg(Color::Rgb(24, 95, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 60, 59))
            .bg(Color::Rgb(13, 59, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 60, 56))
            .bg(Color::Rgb(5, 33, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 93, 68))
            .bg(Color::Rgb(9, 52, 51)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(9, 38, 42))
            .bg(Color::Rgb(5, 29, 39)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 23, 34))
            .bg(Color::Rgb(4, 22, 34)),
    ));
    lines.push(Line::from(spans));
    // Row 15
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 36, 41))
            .bg(Color::Rgb(4, 20, 34)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 22, 35))
            .bg(Color::Rgb(3, 25, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 17, 35))
            .bg(Color::Rgb(9, 43, 49)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 20, 36))
            .bg(Color::Rgb(6, 33, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 50, 52))
            .bg(Color::Rgb(17, 53, 57)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 76, 72))
            .bg(Color::Rgb(27, 84, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(51, 147, 113))
            .bg(Color::Rgb(54, 152, 118)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 51, 60))
            .bg(Color::Rgb(35, 105, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 71, 74))
            .bg(Color::Rgb(4, 40, 55)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(58, 184, 141))
            .bg(Color::Rgb(10, 65, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 99, 103))
            .bg(Color::Rgb(42, 132, 103)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(27, 44, 66))
            .bg(Color::Rgb(28, 68, 90)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 58, 85))
            .bg(Color::Rgb(53, 99, 101)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(54, 96, 91))
            .bg(Color::Rgb(109, 181, 107)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 63, 80))
            .bg(Color::Rgb(39, 57, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(34, 47, 65))
            .bg(Color::Rgb(44, 58, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 48, 65))
            .bg(Color::Rgb(33, 47, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 39, 57))
            .bg(Color::Rgb(24, 30, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(34, 51, 57))
            .bg(Color::Rgb(22, 36, 40)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(59, 118, 65))
            .bg(Color::Rgb(63, 125, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 49, 46))
            .bg(Color::Rgb(14, 17, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 23, 50))
            .bg(Color::Rgb(14, 21, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 14, 45))
            .bg(Color::Rgb(17, 17, 49)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(24, 71, 57))
            .bg(Color::Rgb(25, 73, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 92, 62))
            .bg(Color::Rgb(27, 83, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 15, 44))
            .bg(Color::Rgb(16, 14, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 20, 48))
            .bg(Color::Rgb(14, 17, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 61, 50))
            .bg(Color::Rgb(15, 21, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(60, 136, 66))
            .bg(Color::Rgb(66, 126, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 80, 66))
            .bg(Color::Rgb(13, 18, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 71, 81))
            .bg(Color::Rgb(16, 21, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(49, 91, 83))
            .bg(Color::Rgb(27, 36, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(51, 111, 66))
            .bg(Color::Rgb(45, 73, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 70, 72))
            .bg(Color::Rgb(39, 76, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(55, 91, 89))
            .bg(Color::Rgb(98, 163, 99)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 57, 84))
            .bg(Color::Rgb(62, 115, 103)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 54, 69))
            .bg(Color::Rgb(28, 37, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(27, 64, 85))
            .bg(Color::Rgb(31, 89, 91)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(47, 153, 132))
            .bg(Color::Rgb(38, 132, 104)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(28, 115, 91))
            .bg(Color::Rgb(8, 60, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 48, 59))
            .bg(Color::Rgb(18, 59, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 93, 81))
            .bg(Color::Rgb(37, 120, 102)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(34, 115, 91))
            .bg(Color::Rgb(50, 164, 122)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 72, 69))
            .bg(Color::Rgb(18, 67, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 34, 41))
            .bg(Color::Rgb(8, 42, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 45, 46))
            .bg(Color::Rgb(8, 55, 50)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 14, 32))
            .bg(Color::Rgb(3, 17, 34)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 19, 33))
            .bg(Color::Rgb(3, 17, 33)),
    ));
    lines.push(Line::from(spans));
    // Row 16
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 33, 38))
            .bg(Color::Rgb(8, 45, 43)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 22, 34))
            .bg(Color::Rgb(5, 27, 36)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 14, 32))
            .bg(Color::Rgb(2, 11, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 5, 27))
            .bg(Color::Rgb(1, 7, 29)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 45, 49))
            .bg(Color::Rgb(12, 49, 49)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 78, 71))
            .bg(Color::Rgb(16, 54, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 80, 75))
            .bg(Color::Rgb(28, 92, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(59, 168, 130))
            .bg(Color::Rgb(38, 130, 107)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(58, 183, 137))
            .bg(Color::Rgb(20, 70, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 85, 86))
            .bg(Color::Rgb(56, 159, 134)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 57, 80))
            .bg(Color::Rgb(36, 74, 96)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 57, 81))
            .bg(Color::Rgb(36, 48, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(27, 37, 61))
            .bg(Color::Rgb(27, 40, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 28, 52))
            .bg(Color::Rgb(29, 43, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 35, 62))
            .bg(Color::Rgb(30, 45, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 57, 81))
            .bg(Color::Rgb(35, 67, 87)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 61, 87))
            .bg(Color::Rgb(33, 54, 82)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 61, 85))
            .bg(Color::Rgb(35, 55, 82)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 47, 85))
            .bg(Color::Rgb(26, 34, 78)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(48, 97, 68))
            .bg(Color::Rgb(48, 96, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(52, 79, 64))
            .bg(Color::Rgb(51, 83, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 45, 80))
            .bg(Color::Rgb(28, 40, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(33, 44, 80))
            .bg(Color::Rgb(31, 38, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(54, 112, 83))
            .bg(Color::Rgb(37, 87, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(59, 125, 86))
            .bg(Color::Rgb(44, 110, 77)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(27, 35, 76))
            .bg(Color::Rgb(30, 36, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(47, 80, 80))
            .bg(Color::Rgb(26, 38, 68)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(55, 114, 65))
            .bg(Color::Rgb(63, 122, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(55, 94, 94))
            .bg(Color::Rgb(26, 47, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 70, 88))
            .bg(Color::Rgb(39, 77, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 66, 82))
            .bg(Color::Rgb(40, 95, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(53, 93, 90))
            .bg(Color::Rgb(40, 93, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 77, 90))
            .bg(Color::Rgb(37, 70, 73)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 47, 70))
            .bg(Color::Rgb(31, 42, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 26, 50))
            .bg(Color::Rgb(30, 42, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 24, 47))
            .bg(Color::Rgb(29, 41, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 50, 78))
            .bg(Color::Rgb(28, 45, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 73, 79))
            .bg(Color::Rgb(48, 88, 102)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(51, 116, 91))
            .bg(Color::Rgb(41, 116, 111)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(67, 194, 142))
            .bg(Color::Rgb(39, 139, 110)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 122, 97))
            .bg(Color::Rgb(35, 109, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 61, 62))
            .bg(Color::Rgb(25, 85, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 59, 58))
            .bg(Color::Rgb(15, 56, 58)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 23, 37))
            .bg(Color::Rgb(3, 18, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 29, 38))
            .bg(Color::Rgb(3, 26, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 43, 44))
            .bg(Color::Rgb(7, 46, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(8, 32, 39))
            .bg(Color::Rgb(7, 29, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 18, 32))
            .bg(Color::Rgb(4, 24, 34)),
    ));
    lines.push(Line::from(spans));
    // Row 17
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 28, 35))
            .bg(Color::Rgb(7, 33, 37)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 8, 28))
            .bg(Color::Rgb(1, 8, 29)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 14, 32))
            .bg(Color::Rgb(2, 14, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 9, 28))
            .bg(Color::Rgb(1, 7, 27)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 43, 49))
            .bg(Color::Rgb(15, 59, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(48, 86, 83))
            .bg(Color::Rgb(56, 136, 112)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 20, 35))
            .bg(Color::Rgb(47, 94, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 70, 62))
            .bg(Color::Rgb(23, 75, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(50, 138, 108))
            .bg(Color::Rgb(52, 179, 132)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(53, 96, 96))
            .bg(Color::Rgb(65, 153, 121)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(48, 111, 105))
            .bg(Color::Rgb(52, 83, 92)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 93, 95))
            .bg(Color::Rgb(39, 68, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 66, 77))
            .bg(Color::Rgb(23, 27, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 44, 54))
            .bg(Color::Rgb(12, 20, 41)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 37, 66))
            .bg(Color::Rgb(27, 32, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 60, 79))
            .bg(Color::Rgb(70, 119, 100)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 62, 74))
            .bg(Color::Rgb(83, 171, 112)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(47, 77, 84))
            .bg(Color::Rgb(70, 145, 104)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(44, 71, 82))
            .bg(Color::Rgb(73, 144, 105)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(68, 135, 75))
            .bg(Color::Rgb(76, 155, 84)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(50, 79, 71))
            .bg(Color::Rgb(69, 126, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(39, 45, 66))
            .bg(Color::Rgb(46, 54, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 50, 83))
            .bg(Color::Rgb(31, 44, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(84, 144, 107))
            .bg(Color::Rgb(72, 133, 97)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(82, 144, 105))
            .bg(Color::Rgb(74, 140, 98)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 49, 91))
            .bg(Color::Rgb(31, 49, 92)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(45, 100, 75))
            .bg(Color::Rgb(53, 102, 79)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 101, 71))
            .bg(Color::Rgb(39, 94, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(93, 172, 117))
            .bg(Color::Rgb(121, 200, 129)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(144, 238, 137))
            .bg(Color::Rgb(150, 234, 139)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(142, 233, 136))
            .bg(Color::Rgb(152, 236, 140)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(91, 175, 111))
            .bg(Color::Rgb(118, 203, 125)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 66, 84))
            .bg(Color::Rgb(27, 55, 78)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 35, 65))
            .bg(Color::Rgb(27, 47, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 49, 53))
            .bg(Color::Rgb(9, 9, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(43, 109, 98))
            .bg(Color::Rgb(23, 37, 58)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(48, 117, 106))
            .bg(Color::Rgb(45, 90, 98)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 84, 92))
            .bg(Color::Rgb(59, 132, 113)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(44, 92, 81))
            .bg(Color::Rgb(72, 174, 115)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(46, 107, 98))
            .bg(Color::Rgb(67, 186, 135)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 101, 83))
            .bg(Color::Rgb(31, 102, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(17, 59, 59))
            .bg(Color::Rgb(21, 68, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(7, 32, 42))
            .bg(Color::Rgb(33, 99, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 22, 34))
            .bg(Color::Rgb(4, 19, 33)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(5, 27, 36))
            .bg(Color::Rgb(5, 31, 38)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 25, 35))
            .bg(Color::Rgb(6, 36, 40)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 6, 29))
            .bg(Color::Rgb(0, 1, 26)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 20, 32))
            .bg(Color::Rgb(5, 20, 33)),
    ));
    lines.push(Line::from(spans));
    // Row 18
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 18, 32))
            .bg(Color::Rgb(4, 20, 33)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 6, 29))
            .bg(Color::Rgb(0, 6, 29)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 10, 31))
            .bg(Color::Rgb(2, 10, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 5, 26))
            .bg(Color::Rgb(0, 4, 25)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 60, 58))
            .bg(Color::Rgb(34, 71, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 69, 66))
            .bg(Color::Rgb(29, 67, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(4, 21, 34))
            .bg(Color::Rgb(0, 2, 26)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 107, 85))
            .bg(Color::Rgb(36, 107, 83)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(49, 75, 82))
            .bg(Color::Rgb(52, 115, 98)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(34, 39, 62))
            .bg(Color::Rgb(39, 40, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 50, 70))
            .bg(Color::Rgb(43, 85, 91)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 72, 86))
            .bg(Color::Rgb(46, 109, 101)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 46, 60))
            .bg(Color::Rgb(39, 113, 100)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(19, 52, 55))
            .bg(Color::Rgb(41, 128, 103)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 93, 84))
            .bg(Color::Rgb(41, 93, 96)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 85, 87))
            .bg(Color::Rgb(42, 80, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(48, 97, 103))
            .bg(Color::Rgb(53, 100, 111)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 63, 83))
            .bg(Color::Rgb(33, 45, 76)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(35, 63, 76))
            .bg(Color::Rgb(31, 44, 73)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 117, 70))
            .bg(Color::Rgb(49, 122, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 45, 67))
            .bg(Color::Rgb(32, 41, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 51, 71))
            .bg(Color::Rgb(36, 47, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 41, 77))
            .bg(Color::Rgb(32, 55, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(56, 121, 84))
            .bg(Color::Rgb(83, 143, 104)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(53, 114, 80))
            .bg(Color::Rgb(80, 140, 102)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 39, 76))
            .bg(Color::Rgb(31, 46, 86)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 51, 68))
            .bg(Color::Rgb(39, 84, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 77, 61))
            .bg(Color::Rgb(36, 103, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 42, 73))
            .bg(Color::Rgb(47, 81, 90)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(31, 64, 82))
            .bg(Color::Rgb(92, 165, 111)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 67, 82))
            .bg(Color::Rgb(95, 169, 115)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(42, 90, 95))
            .bg(Color::Rgb(45, 84, 85)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(50, 127, 110))
            .bg(Color::Rgb(39, 81, 93)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 101, 85))
            .bg(Color::Rgb(51, 126, 113)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(15, 32, 46))
            .bg(Color::Rgb(40, 112, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(18, 37, 53))
            .bg(Color::Rgb(23, 66, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 76, 84))
            .bg(Color::Rgb(20, 36, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 42, 71))
            .bg(Color::Rgb(46, 86, 97)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(47, 86, 75))
            .bg(Color::Rgb(35, 59, 81)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(75, 144, 93))
            .bg(Color::Rgb(28, 42, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(60, 137, 99))
            .bg(Color::Rgb(46, 121, 85)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(37, 113, 87))
            .bg(Color::Rgb(15, 46, 46)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(6, 22, 35))
            .bg(Color::Rgb(0, 6, 28)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 9, 30))
            .bg(Color::Rgb(4, 19, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 7, 31))
            .bg(Color::Rgb(3, 13, 32)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 11, 30))
            .bg(Color::Rgb(2, 12, 31)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 6, 29))
            .bg(Color::Rgb(0, 5, 29)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(3, 14, 31))
            .bg(Color::Rgb(3, 13, 30)),
    ));
    lines.push(Line::from(spans));
    // Row 19
    let mut spans = vec![Span::raw(pad.clone())];
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 10, 29))
            .bg(Color::Rgb(2, 11, 30)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 6, 29))
            .bg(Color::Rgb(0, 5, 28)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 8, 30))
            .bg(Color::Rgb(2, 9, 30)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 8, 30))
            .bg(Color::Rgb(1, 5, 29)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 7, 29))
            .bg(Color::Rgb(7, 23, 36)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(10, 29, 37))
            .bg(Color::Rgb(30, 64, 59)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(36, 97, 81))
            .bg(Color::Rgb(34, 90, 72)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(29, 44, 71))
            .bg(Color::Rgb(36, 102, 94)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(16, 18, 51))
            .bg(Color::Rgb(26, 28, 53)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(24, 30, 58))
            .bg(Color::Rgb(31, 39, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 31, 58))
            .bg(Color::Rgb(18, 21, 50)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 26, 55))
            .bg(Color::Rgb(24, 27, 56)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 16, 43))
            .bg(Color::Rgb(17, 27, 51)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 14, 36))
            .bg(Color::Rgb(17, 33, 47)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 13, 49))
            .bg(Color::Rgb(27, 52, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 45, 57))
            .bg(Color::Rgb(43, 92, 74)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(40, 82, 71))
            .bg(Color::Rgb(25, 45, 65)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 30, 60))
            .bg(Color::Rgb(29, 49, 73)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 31, 59))
            .bg(Color::Rgb(31, 57, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 40, 69))
            .bg(Color::Rgb(31, 86, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(26, 41, 56))
            .bg(Color::Rgb(54, 69, 75)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 38, 64))
            .bg(Color::Rgb(31, 44, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 22, 57))
            .bg(Color::Rgb(24, 25, 61)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 88, 68))
            .bg(Color::Rgb(33, 96, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(30, 86, 67))
            .bg(Color::Rgb(32, 90, 67)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 21, 55))
            .bg(Color::Rgb(25, 26, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 36, 62))
            .bg(Color::Rgb(26, 39, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(21, 35, 58))
            .bg(Color::Rgb(35, 35, 66)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 36, 61))
            .bg(Color::Rgb(31, 52, 70)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(22, 36, 61))
            .bg(Color::Rgb(36, 71, 80)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 30, 59))
            .bg(Color::Rgb(29, 54, 71)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(41, 80, 67))
            .bg(Color::Rgb(27, 53, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 44, 56))
            .bg(Color::Rgb(40, 82, 69)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 14, 49))
            .bg(Color::Rgb(26, 50, 62)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(11, 17, 35))
            .bg(Color::Rgb(11, 19, 35)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(14, 19, 47))
            .bg(Color::Rgb(15, 19, 44)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(12, 17, 40))
            .bg(Color::Rgb(29, 39, 64)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(13, 12, 46))
            .bg(Color::Rgb(14, 18, 48)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(63, 123, 83))
            .bg(Color::Rgb(77, 152, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(38, 73, 64))
            .bg(Color::Rgb(35, 63, 63)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(20, 15, 52))
            .bg(Color::Rgb(23, 20, 60)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(32, 71, 79))
            .bg(Color::Rgb(37, 95, 88)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(25, 75, 63))
            .bg(Color::Rgb(18, 55, 51)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 1, 24))
            .bg(Color::Rgb(0, 5, 27)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 8, 30))
            .bg(Color::Rgb(1, 8, 30)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(1, 9, 29))
            .bg(Color::Rgb(2, 10, 30)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(0, 5, 28))
            .bg(Color::Rgb(1, 6, 28)),
    ));
    spans.push(Span::styled(
        "▄",
        Style::default()
            .fg(Color::Rgb(2, 9, 29))
            .bg(Color::Rgb(3, 13, 30)),
    ));
    lines.push(Line::from(spans));
    lines
}
