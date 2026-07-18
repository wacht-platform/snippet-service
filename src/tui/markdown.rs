use super::*;
use super::theme::*;

/// Center a line horizontally within `width` by left-padding to half the slack.
pub(super) fn center_line(line: Line<'static>, width: usize) -> Line<'static> {
    let pad = width.saturating_sub(line.width()) / 2;
    if pad == 0 {
        return line;
    }
    let mut spans = vec![Span::raw(" ".repeat(pad))];
    spans.extend(line.spans);
    Line::from(spans)
}

// --- Markdown-lite prose rendering (assistant text) ---

pub(super) fn render_prose(text: &str, width: usize) -> Vec<Line<'static>> {
    let base = Style::default().fg(self::text());
    let code_block = Style::default().fg(code());
    let heading = Style::default().fg(blue()).add_modifier(Modifier::BOLD);

    let lines: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    let mut in_code = false;
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            i += 1;
            continue;
        }
        // Markdown table: a `|`-delimited header row immediately followed by a
        // `|---|:--:|` separator row of the same column count. Render aligned with
        // wrapped cells instead of dumping raw pipes.
        if !in_code && raw.contains('|') && i + 1 < lines.len() && is_table_separator(lines[i + 1]) {
            let header = split_table_cells(raw);
            let sep = split_table_cells(lines[i + 1]);
            if !header.is_empty() && header.len() == sep.len() {
                let aligns: Vec<CellAlign> = sep.iter().map(|c| cell_align(c)).collect();
                let mut body = Vec::new();
                let mut j = i + 2;
                while j < lines.len() && lines[j].contains('|') && !lines[j].trim().is_empty() {
                    body.push(split_table_cells(lines[j]));
                    j += 1;
                }
                out.extend(render_md_table(&header, &aligns, &body, width));
                i = j;
                continue;
            }
        }
        // Fenced code, or an unfenced block indented 4+ spaces / a tab (Markdown
        // indented code): render verbatim — preserve indentation and skip inline
        // markdown so underscores/asterisks inside identifiers aren't mangled.
        let indented_code =
            !in_code && !trimmed.is_empty() && (raw.starts_with("    ") || raw.starts_with('\t'));
        if in_code || indented_code {
            for seg in wrap_code_line(raw, width.saturating_sub(2)) {
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(seg, code_block),
                ]));
            }
            i += 1;
            continue;
        }
        if let Some(h) = heading_text(trimmed) {
            for seg in wrap_one(h, width) {
                out.push(Line::from(Span::styled(seg, heading)));
            }
            i += 1;
            continue;
        }
        if is_thematic_break(trimmed) {
            out.push(Line::from(Span::styled(
                "─".repeat(width.max(1)),
                Style::default().fg(faint()),
            )));
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            let runs = parse_inline_md(rest, base);
            let mut bullet = wrap_runs(runs, width.saturating_sub(2));
            if let Some(first) = bullet.first_mut() {
                first.spans.insert(
                    0,
                    Span::styled("• ", Style::default().fg(blue())),
                );
            }
            for line in bullet.iter_mut().skip(1) {
                line.spans.insert(0, Span::raw("  "));
            }
            out.extend(bullet);
            i += 1;
            continue;
        }
        if raw.trim().is_empty() {
            out.push(Line::from(""));
            i += 1;
            continue;
        }
        let runs = parse_inline_md(trimmed, base);
        out.extend(wrap_runs(runs, width));
        i += 1;
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

#[derive(Clone, Copy)]
pub(super) enum CellAlign {
    Left,
    Right,
    Center,
}

/// Split a markdown table row into trimmed cells, dropping the optional leading
/// and trailing pipes (`| a | b |` and `a | b` both → ["a", "b"]).
pub(super) fn split_table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// A markdown table delimiter row: every cell is dashes with optional `:` ends.
pub(super) fn is_table_separator(line: &str) -> bool {
    let cells = split_table_cells(line);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim();
            c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':')
        })
}

pub(super) fn cell_align(sep: &str) -> CellAlign {
    let s = sep.trim();
    match (s.starts_with(':'), s.ends_with(':')) {
        (true, true) => CellAlign::Center,
        (false, true) => CellAlign::Right,
        _ => CellAlign::Left,
    }
}

/// Pad a rendered cell line to `w` columns per its alignment.
pub(super) fn pad_table_line(mut line: Line<'static>, w: usize, align: CellAlign) -> Line<'static> {
    let pad = w.saturating_sub(line.width());
    if pad == 0 {
        return line;
    }
    match align {
        CellAlign::Left => line.spans.push(Span::raw(" ".repeat(pad))),
        CellAlign::Right => line.spans.insert(0, Span::raw(" ".repeat(pad))),
        CellAlign::Center => {
            let l = pad / 2;
            line.spans.insert(0, Span::raw(" ".repeat(l)));
            line.spans.push(Span::raw(" ".repeat(pad - l)));
        }
    }
    line
}

/// Render a markdown table: column widths from content (shrunk to fit `width`),
/// header bold + a rule, cells inline-md-styled and wrapped, separated by ` │ `.
pub(super) fn render_md_table(
    header: &[String],
    aligns: &[CellAlign],
    body: &[Vec<String>],
    width: usize,
) -> Vec<Line<'static>> {
    // The header/delimiter row is authoritative for column count (GFM). A ragged
    // body row with extra cells folds its overflow back into the last column so a
    // single malformed row can't explode the whole table into empty columns.
    let ncols = header.len().max(1);
    let body: Vec<Vec<String>> = body
        .iter()
        .map(|row| {
            if row.len() > ncols {
                let mut r: Vec<String> = row[..ncols - 1].to_vec();
                r.push(row[ncols - 1..].join(" "));
                r
            } else {
                row.clone()
            }
        })
        .collect();
    let body = &body[..];
    let header_style = Style::default().fg(self::text()).add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(self::text());
    let faint = Style::default().fg(faint());

    let runs_for = |s: &str, header_row: bool| {
        parse_inline_md(s, if header_row { header_style } else { body_style })
    };
    let runs_w = |runs: &[(String, Style)]| runs.iter().map(|(t, _)| t.chars().count()).sum::<usize>();

    let mut natural = vec![1usize; ncols];
    for (c, h) in header.iter().enumerate().take(ncols) {
        natural[c] = natural[c].max(runs_w(&runs_for(h, true)));
    }
    for row in body {
        for (c, cell) in row.iter().enumerate().take(ncols) {
            natural[c] = natural[c].max(runs_w(&runs_for(cell, false)));
        }
    }

    // Column separator is " │ " (3 cols). Fit natural widths into the budget,
    // shrinking the widest columns first when they don't fit.
    let sep_total = 3 * ncols.saturating_sub(1);
    let avail = width.saturating_sub(sep_total).max(ncols * 3);
    // Water-fill: start from natural widths and only ever shave the single widest
    // column. Narrow columns keep their full width until every column is as wide,
    // so a giant column wrapping doesn't force short ones (e.g. `oauth-relay`) to
    // wrap a character early.
    let widths: Vec<usize> = {
        let mut w = natural.clone();
        let mut total: usize = w.iter().sum();
        while total > avail {
            let idx = (0..ncols).max_by_key(|&i| w[i]).unwrap_or(0);
            if w[idx] <= 3 {
                break;
            }
            w[idx] -= 1;
            total -= 1;
        }
        w
    };

    let render_row = |cells: &[String], header_row: bool| -> Vec<Line<'static>> {
        let wrapped: Vec<Vec<Line<'static>>> = (0..ncols)
            .map(|c| {
                let text = cells.get(c).map(String::as_str).unwrap_or("");
                let mut wl = wrap_runs(runs_for(text, header_row), widths[c].max(1));
                if wl.is_empty() {
                    wl.push(Line::from(""));
                }
                let align = aligns.get(c).copied().unwrap_or(CellAlign::Left);
                wl.into_iter().map(|l| pad_table_line(l, widths[c], align)).collect()
            })
            .collect();
        let height = wrapped.iter().map(|w| w.len()).max().unwrap_or(1);
        (0..height)
            .map(|k| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                for c in 0..ncols {
                    if c > 0 {
                        spans.push(Span::styled(" │ ", faint));
                    }
                    match wrapped[c].get(k) {
                        Some(l) => spans.extend(l.spans.clone()),
                        None => spans.push(Span::raw(" ".repeat(widths[c]))),
                    }
                }
                Line::from(spans)
            })
            .collect()
    };

    let mut out = render_row(header, true);
    let mut rule: Vec<Span<'static>> = Vec::new();
    for c in 0..ncols {
        if c > 0 {
            rule.push(Span::styled("─┼─", faint));
        }
        rule.push(Span::styled("─".repeat(widths[c]), faint));
    }
    out.push(Line::from(rule));
    for row in body {
        out.extend(render_row(row, false));
    }
    out
}

/// A thematic break (`---`, `***`, `___`): three or more of a single marker
/// char, spaces allowed between. Rendered as a horizontal rule.
pub(super) fn is_thematic_break(line: &str) -> bool {
    let t = line.trim();
    let marker = match t.chars().next() {
        Some(c @ ('-' | '*' | '_')) => c,
        _ => return false,
    };
    let mut count = 0;
    for ch in t.chars() {
        if ch == marker {
            count += 1;
        } else if ch != ' ' {
            return false;
        }
    }
    count >= 3
}

pub(super) fn heading_text(line: &str) -> Option<&str> {
    for prefix in ["#### ", "### ", "## ", "# "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    None
}

/// Split a single logical line into styled runs, honouring `**bold**`,
/// `` `code` ``, and `*italic*` / `_italic_`.
pub(super) fn parse_inline_md(text: &str, base: Style) -> Vec<(String, Style)> {
    let bold = base.add_modifier(Modifier::BOLD);
    let italic = base.add_modifier(Modifier::ITALIC);
    let code = Style::default().fg(code());

    let chars: Vec<char> = text.chars().collect();
    let mut runs: Vec<(String, Style)> = Vec::new();
    let mut buf = String::new();
    let mut i = 0;

    let find = |from: usize, pat: char| (from..chars.len()).find(|&j| chars[j] == pat);
    let find2 =
        |from: usize| (from + 1..chars.len()).find(|&j| chars[j] == '*' && chars[j - 1] == '*');

    while i < chars.len() {
        let c = chars[i];
        if c == '`'
            && let Some(end) = find(i + 1, '`')
        {
            flush(&mut runs, &mut buf, base);
            runs.push((chars[i + 1..end].iter().collect(), code));
            i = end + 1;
            continue;
        }
        if c == '*'
            && i + 1 < chars.len()
            && chars[i + 1] == '*'
            && let Some(end) = find2(i + 2)
        {
            flush(&mut runs, &mut buf, base);
            runs.push((chars[i + 2..end - 1].iter().collect(), bold));
            i = end + 1;
            continue;
        }
        if (c == '*' || c == '_')
            && i + 1 < chars.len()
            && chars[i + 1] != c
            && let Some(end) = find(i + 1, c)
            && end > i + 1
        {
            flush(&mut runs, &mut buf, base);
            runs.push((chars[i + 1..end].iter().collect(), italic));
            i = end + 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut runs, &mut buf, base);
    runs
}

pub(super) fn flush(runs: &mut Vec<(String, Style)>, buf: &mut String, style: Style) {
    if !buf.is_empty() {
        runs.push((std::mem::take(buf), style));
    }
}

/// Greedy word-wrap across styled runs, preserving each run's style.
pub(super) fn wrap_runs(runs: Vec<(String, Style)>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(10);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for (text, style) in runs {
        for word in text.split(' ') {
            if word.is_empty() {
                continue;
            }
            let wlen = word.chars().count();
            if cur_w > 0 && cur_w + 1 + wlen > width {
                lines.push(Line::from(std::mem::take(&mut cur)));
                cur_w = 0;
            }
            if cur_w > 0 {
                cur.push(Span::raw(" "));
                cur_w += 1;
            }
            if wlen > width {
                if cur_w > 0 {
                    lines.push(Line::from(std::mem::take(&mut cur)));
                    cur_w = 0;
                }
                for chunk in hard_chunks(word, width) {
                    let clen = chunk.chars().count();
                    if clen == width {
                        lines.push(Line::from(vec![Span::styled(chunk, style)]));
                    } else {
                        cur.push(Span::styled(chunk, style));
                        cur_w = clen;
                    }
                }
            } else {
                cur.push(Span::styled(word.to_string(), style));
                cur_w += wlen;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
}

/// Plain word-wrap (no styling) for a possibly multi-line string.
/// Wrap a code line to `width` while preserving ALL whitespace — leading indent
/// and internal alignment — unlike `wrap_one`, which collapses space runs. Tabs
/// expand to 4 spaces; continuation fragments are re-indented to the line's own
/// leading whitespace so nested structure stays readable after a wrap.
pub(super) fn wrap_code_line(raw: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let expanded = raw.replace('\t', "    ");
    let chars: Vec<char> = expanded.chars().collect();
    if chars.len() <= width {
        return vec![expanded];
    }
    let indent: String = expanded.chars().take_while(|c| *c == ' ').collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < chars.len() {
        let lead = if first { String::new() } else { indent.clone() };
        let budget = width.saturating_sub(lead.chars().count()).max(1);
        let end = (start + budget).min(chars.len());
        let mut seg = lead;
        seg.extend(&chars[start..end]);
        out.push(seg);
        start = end;
        first = false;
    }
    out
}

pub(super) fn wrap_one(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        if raw.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cw = 0usize;
        for word in raw.split(' ') {
            if word.is_empty() {
                continue;
            }
            let wl = word.chars().count();
            if cw > 0 && cw + 1 + wl > width {
                out.push(std::mem::take(&mut cur));
                cw = 0;
            }
            if wl > width {
                if cw > 0 {
                    out.push(std::mem::take(&mut cur));
                    cw = 0;
                }
                for chunk in hard_chunks(word, width) {
                    let cl = chunk.chars().count();
                    if cl == width {
                        out.push(chunk);
                    } else {
                        cur = chunk;
                        cw = cl;
                    }
                }
                continue;
            }
            if cw > 0 {
                cur.push(' ');
                cw += 1;
            }
            cur.push_str(word);
            cw += wl;
        }
        out.push(cur);
    }
    out
}

pub(super) fn hard_chunks(word: &str, width: usize) -> Vec<String> {
    word.chars()
        .collect::<Vec<_>>()
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}
