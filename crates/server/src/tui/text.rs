use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub type Piece = (String, Style);

pub fn cols(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn pieces_cols(pieces: &[Piece]) -> usize {
    pieces.iter().map(|(t, _)| cols(t)).sum()
}

pub fn line(pieces: Vec<Piece>) -> Line<'static> {
    Line::from(pieces.into_iter().map(|(t, s)| Span::styled(t, s)).collect::<Vec<_>>())
}

fn push(line: &mut Vec<Piece>, text: &str, style: Style) {
    match line.last_mut() {
        Some((t, s)) if *s == style => t.push_str(text),
        _ => line.push((text.to_string(), style)),
    }
}

pub fn truncate(text: &str, width: usize) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if cols(&flat) <= width {
        return flat;
    }
    let mut out = String::new();
    let mut used = 0;
    for c in flat.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

enum Token<'a> {
    Space(&'a str),
    Word(&'a str),
}

fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut space = None;
    for (i, c) in text.char_indices() {
        let is_space = c.is_whitespace();
        match space {
            Some(s) if s == is_space => {}
            Some(s) => {
                out.push(if s {
                    Token::Space(&text[start..i])
                } else {
                    Token::Word(&text[start..i])
                });
                start = i;
                space = Some(is_space);
            }
            None => space = Some(is_space),
        }
    }
    if let Some(s) = space {
        out.push(if s { Token::Space(&text[start..]) } else { Token::Word(&text[start..]) });
    }
    out
}

struct Flow {
    width: usize,
    rest: Vec<Piece>,
    rows: Vec<Vec<Piece>>,
    cur: Vec<Piece>,
    used: usize,
    floor: usize,
}

impl Flow {
    fn new(width: usize, first: &[Piece], rest: &[Piece]) -> Flow {
        let floor = pieces_cols(first);
        let width = width.max(floor.max(pieces_cols(rest)) + 1);
        Flow {
            width,
            rest: rest.to_vec(),
            rows: Vec::new(),
            cur: first.to_vec(),
            used: floor,
            floor,
        }
    }

    fn empty(&self) -> bool {
        self.used == self.floor
    }

    fn break_line(&mut self) {
        let next = self.rest.clone();
        self.floor = pieces_cols(&next);
        self.used = self.floor;
        self.rows.push(std::mem::replace(&mut self.cur, next));
    }

    fn char(&mut self, c: char, style: Style) {
        let w = c.width().unwrap_or(0);
        if self.used + w > self.width && !self.empty() {
            self.break_line();
        }
        let mut buf = [0u8; 4];
        push(&mut self.cur, c.encode_utf8(&mut buf), style);
        self.used += w;
    }

    fn finish(mut self) -> Vec<Vec<Piece>> {
        self.rows.push(self.cur);
        self.rows
    }
}

pub fn wrap(pieces: &[Piece], width: usize, first: &[Piece], rest: &[Piece]) -> Vec<Line<'static>> {
    let mut flow = Flow::new(width, first, rest);
    let mut gap: Option<(usize, Style)> = None;
    for (text, style) in pieces {
        for token in tokens(text) {
            match token {
                Token::Space(s) => {
                    if !flow.empty() {
                        gap = Some((cols(s), *style));
                    }
                }
                Token::Word(w) => {
                    let wide = cols(w);
                    let gap_w = gap.map(|g| g.0).unwrap_or(0);
                    if flow.used + gap_w + wide <= flow.width {
                        if let Some((n, s)) = gap.take() {
                            push(&mut flow.cur, &" ".repeat(n), s);
                            flow.used += n;
                        }
                        push(&mut flow.cur, w, *style);
                        flow.used += wide;
                        continue;
                    }
                    gap = None;
                    if !flow.empty() {
                        flow.break_line();
                    }
                    for c in w.chars() {
                        flow.char(c, *style);
                    }
                }
            }
        }
    }
    flow.finish().into_iter().map(line).collect()
}

pub fn hard_wrap(
    text: &str,
    style: Style,
    width: usize,
    first: &[Piece],
    rest: &[Piece],
) -> Vec<Line<'static>> {
    let mut flow = Flow::new(width, first, rest);
    for c in text.replace('\t', "    ").chars().filter(|c| !c.is_control()) {
        flow.char(c, style);
    }
    flow.finish().into_iter().map(line).collect()
}

pub fn inline(text: &str, base: Style) -> Vec<Piece> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Piece> = Vec::new();
    let mut buf = String::new();
    let flush = |out: &mut Vec<Piece>, buf: &mut String| {
        if !buf.is_empty() {
            push(out, buf, base);
            buf.clear();
        }
    };
    let find = |from: usize, pat: &[char]| -> Option<usize> {
        (from..chars.len().saturating_sub(pat.len() - 1)).find(|&j| chars[j..j + pat.len()] == *pat)
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`'
            && let Some(j) = find(i + 1, &['`'])
            && j > i + 1
        {
            flush(&mut out, &mut buf);
            let code: String = chars[i + 1..j].iter().collect();
            push(&mut out, &code, base.fg(Color::Yellow));
            i = j + 1;
            continue;
        }
        if c == '*'
            && chars.get(i + 1) == Some(&'*')
            && let Some(j) = find(i + 2, &['*', '*'])
            && j > i + 2
        {
            flush(&mut out, &mut buf);
            let bold: String = chars[i + 2..j].iter().collect();
            push(&mut out, &bold, base.add_modifier(Modifier::BOLD));
            i = j + 2;
            continue;
        }
        if c == '['
            && let Some(j) = find(i + 1, &[']', '('])
            && let Some(k) = find(j + 2, &[')'])
        {
            flush(&mut out, &mut buf);
            let label: String = chars[i + 1..j].iter().collect();
            push(&mut out, &label, base.add_modifier(Modifier::UNDERLINED));
            i = k + 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut out, &mut buf);
    out
}

fn piece(text: &str, style: Style) -> Vec<Piece> {
    vec![(text.to_string(), style)]
}

fn list_marker(line: &str) -> Option<(String, &str)> {
    let indent = line.len() - line.trim_start().len();
    let rest = &line[indent..];
    for bullet in ["- ", "* ", "+ "] {
        if let Some(body) = rest.strip_prefix(bullet) {
            return Some((format!("{}• ", &line[..indent]), body));
        }
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if (1..=3).contains(&digits) && rest[digits..].starts_with(". ") {
        return Some((format!("{}{}. ", &line[..indent], &rest[..digits]), &rest[digits + 2..]));
    }
    None
}

pub fn markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let plain = Style::default();
    let dim = Style::default().fg(Color::DarkGray);
    let code = Style::default().fg(Color::Yellow);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut fenced = false;
    for raw in text.trim_end().lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") {
            if !fenced {
                let lang = trimmed.trim_start_matches('`').trim();
                if !lang.is_empty() {
                    out.push(line(piece(&format!("  {lang}"), dim)));
                }
            }
            fenced = !fenced;
            continue;
        }
        if fenced {
            out.extend(hard_wrap(raw, code, width, &piece("  ", dim), &piece("  ", dim)));
            continue;
        }
        if raw.trim().is_empty() {
            if !out.last().is_some_and(|l| l.spans.is_empty()) {
                out.push(Line::default());
            }
            continue;
        }
        if let Some(title) = trimmed.strip_prefix('#') {
            let title = title.trim_start_matches('#').trim();
            let style = plain.add_modifier(Modifier::BOLD).fg(Color::Cyan);
            out.extend(wrap(&inline(title, style), width, &[], &[]));
            continue;
        }
        if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_') {
            out.push(line(piece(&"─".repeat(width.min(40)), dim)));
            continue;
        }
        if trimmed.starts_with('|') {
            out.extend(hard_wrap(raw, plain, width, &[], &[]));
            continue;
        }
        if let Some(quote) = trimmed.strip_prefix('>') {
            let bar = piece("▎ ", dim);
            let style = plain.add_modifier(Modifier::ITALIC);
            out.extend(wrap(&inline(quote.trim_start(), style), width, &bar, &bar));
            continue;
        }
        if let Some((marker, body)) = list_marker(raw) {
            let first = piece(&marker, dim);
            let rest = piece(&" ".repeat(cols(&marker)), plain);
            out.extend(wrap(&inline(body, plain), width, &first, &rest));
            continue;
        }
        out.extend(wrap(&inline(raw.trim(), plain), width, &[], &[]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    fn plain(text: &str) -> Vec<Piece> {
        piece(text, Style::default())
    }

    #[test]
    fn words_wrap_at_the_width_and_never_split() {
        let rows = wrap(&plain("the quick brown fox jumps"), 10, &[], &[]);
        assert_eq!(texts(&rows), ["the quick", "brown fox", "jumps"]);
    }

    #[test]
    fn a_word_longer_than_a_row_is_broken() {
        let rows = wrap(&plain("abcdefghijkl"), 5, &[], &[]);
        assert_eq!(texts(&rows), ["abcde", "fghij", "kl"]);
    }

    #[test]
    fn prefixes_lead_the_first_row_and_the_rest() {
        let rows = wrap(&plain("alpha beta gamma"), 9, &plain("> "), &plain("  "));
        assert_eq!(texts(&rows), ["> alpha", "  beta", "  gamma"]);
    }

    #[test]
    fn wrapped_rows_are_never_wider_than_the_width() {
        let text = "日本語のテキスト and some English words 日本 mixed together here";
        for w in 6..30 {
            for row in texts(&wrap(&plain(text), w, &plain("• "), &plain("  "))) {
                assert!(cols(&row) <= w, "{row:?} is wider than {w}");
            }
        }
    }

    #[test]
    fn hard_wrap_keeps_spaces() {
        let rows = hard_wrap("a  b  c", Style::default(), 4, &[], &[]);
        assert_eq!(texts(&rows), ["a  b", "  c"]);
    }

    #[test]
    fn inline_code_bold_and_links_are_styled_and_unmarked() {
        let p = inline("use `cargo` and **bold** [docs](http://x)", Style::default());
        let joined: String = p.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "use cargo and bold docs");
        assert!(p.iter().any(|(t, s)| t == "cargo" && s.fg == Some(Color::Yellow)));
        assert!(p.iter().any(|(t, s)| t == "bold" && s.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn an_unclosed_marker_stays_as_typed() {
        let p = inline("a `b and **c", Style::default());
        let joined: String = p.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "a `b and **c");
    }

    #[test]
    fn markdown_handles_fences_lists_headings_and_blank_runs() {
        let md = "# Title\n\n\n- one\n- two with a long tail that wraps\n1. first\n\n```rust\nlet x = 1;\n```\nend";
        let rows = texts(&markdown(md, 20));
        assert_eq!(
            rows,
            [
                "Title",
                "",
                "• one",
                "• two with a long",
                "  tail that wraps",
                "1. first",
                "",
                "  rust",
                "  let x = 1;",
                "end"
            ]
        );
    }

    #[test]
    fn truncate_ends_with_an_ellipsis_inside_the_width() {
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(truncate("short", 8), "short");
        assert_eq!(truncate("a\nb", 8), "a b");
        assert!(cols(&truncate("日本語日本語", 7)) <= 7);
    }
}
