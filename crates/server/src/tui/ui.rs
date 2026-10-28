use super::app::{App, Overlay, Pending, Pick, Tone, commands};
use super::text::{Piece, cols, hard_wrap, line, markdown, truncate, wrap};
use agentbench_core::{Entry, Message, Session, SessionStatus, Usage};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const OUTPUT_LINES: usize = 14;
const COMPOSER_MAX: usize = 8;
const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;

#[derive(Default)]
pub struct View {
    width: usize,
    expanded: bool,
    lines: Vec<Line<'static>>,
}

fn dim() -> Style {
    Style::default().fg(DIM)
}

fn piece(text: &str, style: Style) -> Vec<Piece> {
    vec![(text.to_string(), style)]
}

pub fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    if let Some((done, all)) = checklist(name, input) {
        return format!("{done} of {all} done");
    }
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let text = pick("command")
        .or_else(|| pick("path"))
        .or_else(|| pick("file_path"))
        .or_else(|| pick("pattern"))
        .or_else(|| pick("query"))
        .or_else(|| pick("url"))
        .or_else(|| pick("handle"))
        .or_else(|| pick("message"))
        .or_else(|| pick("topic"))
        .unwrap_or_else(|| if input.is_null() { String::new() } else { input.to_string() });
    text.lines().next().unwrap_or_default().to_string()
}

fn checklist(name: &str, input: &serde_json::Value) -> Option<(usize, usize)> {
    if !matches!(name, "todo_write" | "TodoWrite") {
        return None;
    }
    let items = input.get("items").or_else(|| input.get("todos"))?.as_array()?;
    let done = items
        .iter()
        .filter(|i| matches!(i.get("status").and_then(|s| s.as_str()), Some("done" | "completed")))
        .count();
    Some((done, items.len()))
}

fn indented(text: &str, style: Style, width: usize, max: Option<usize>) -> Vec<Line<'static>> {
    let pad = piece("    ", dim());
    let mut rows: Vec<Line<'static>> = Vec::new();
    let all: Vec<&str> = text.lines().collect();
    let shown = max.map_or(all.len(), |m| m.min(all.len()));
    for l in &all[..shown] {
        rows.extend(hard_wrap(l, style, width, &pad, &pad));
    }
    if shown < all.len() {
        rows.push(line(piece(&format!("    … {} more lines", all.len() - shown), dim())));
    }
    rows
}

fn tool_lines(
    name: &str,
    input: &serde_json::Value,
    output: &Option<String>,
    error: bool,
    expanded: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let (mark, tint) = match (output, error) {
        (None, _) => ("◌", Color::Yellow),
        (Some(_), true) => ("✗", Color::Red),
        (Some(_), false) => ("▸", DIM),
    };
    let summary = tool_summary(name, input);
    let head = vec![
        (format!("{mark} "), Style::default().fg(tint)),
        (name.to_string(), Style::default().fg(tint).add_modifier(Modifier::BOLD)),
        (" ".to_string(), Style::default()),
        (truncate(&summary, width.saturating_sub(cols(name) + 4)), Style::default().fg(tint)),
    ];
    let mut rows = vec![line(head)];
    if let Some(out) = output
        && (expanded || error)
        && !out.trim().is_empty()
    {
        let style = if error { Style::default().fg(Color::Red) } else { dim() };
        rows.extend(indented(out.trim_end(), style, width, Some(OUTPUT_LINES)));
    }
    rows
}

fn question_lines(
    questions: &[agentbench_core::Question],
    answers: Option<&[String]>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for (i, q) in questions.iter().enumerate() {
        let lead = piece("? ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD));
        rows.extend(wrap(&piece(&q.text, Style::default()), width, &lead, &piece("  ", dim())));
        if let Some(a) = answers.and_then(|a| a.get(i)) {
            rows.push(line(vec![
                ("  → ".into(), dim()),
                (a.clone(), Style::default().fg(Color::Green)),
            ]));
        }
    }
    rows
}

fn entry_lines(m: &Message, expanded: bool, width: usize) -> Vec<Line<'static>> {
    match &m.entry {
        Entry::User { text, attachments, queued } => {
            let lead = piece("› ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD));
            let mut body = piece(text, Style::default().add_modifier(Modifier::BOLD));
            if !attachments.is_empty() {
                body.push((format!("  [{} files]", attachments.len()), dim()));
            }
            if *queued {
                body.push(("  (queued)".into(), dim()));
            }
            let mut rows = Vec::new();
            for para in text_paragraphs(&body) {
                rows.extend(wrap(&para, width, &lead, &piece("  ", dim())));
            }
            rows
        }
        Entry::Assistant { text } => markdown(text, width),
        Entry::Thinking { text } if expanded => {
            let style = dim().add_modifier(Modifier::ITALIC);
            let mut rows = vec![line(piece("thinking", dim()))];
            rows.extend(indented(text.trim(), style, width, None));
            rows
        }
        Entry::Thinking { text } => {
            let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
            vec![line(vec![
                ("thinking ".into(), dim()),
                (truncate(first, width.saturating_sub(10)), dim().add_modifier(Modifier::ITALIC)),
            ])]
        }
        Entry::Tool { name, input, output, error, .. } => {
            tool_lines(name, input, output, *error, expanded, width)
        }
        Entry::Question { question, answer, .. } => {
            let answers = answer.as_ref().map(|a| [a.clone()]);
            question_lines(std::slice::from_ref(question), answers.as_ref().map(|a| &a[..]), width)
        }
        Entry::Questions { questions, answers, .. } => {
            question_lines(questions, answers.as_deref(), width)
        }
        Entry::Error { text } => {
            let lead =
                piece("error ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
            wrap(
                &piece(text, Style::default().fg(Color::Red)),
                width,
                &lead,
                &piece("      ", dim()),
            )
        }
        Entry::Notice { text } => wrap(&piece(text, dim()), width, &[], &[]),
        Entry::Diagnostics { text, .. } => tool_lines(
            "diagnostics",
            &serde_json::json!({ "message": "language server" }),
            &Some(agentbench_core::diagnostics_findings(text)),
            false,
            expanded,
            width,
        ),
        Entry::Unknown => Vec::new(),
        Entry::Summary { text } if text.is_empty() => {
            vec![line(piece("── cleared: the agent starts afresh here ──", dim()))]
        }
        Entry::Summary { text } => {
            let mut rows =
                vec![line(piece("── compacted: the agent carries on from this summary ──", dim()))];
            rows.extend(markdown(text, width));
            rows
        }
    }
}

fn text_paragraphs(body: &[Piece]) -> Vec<Vec<Piece>> {
    let mut paras: Vec<Vec<Piece>> = vec![Vec::new()];
    for (text, style) in body {
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            paras.last_mut().expect("one").push((first.to_string(), *style));
        }
        for rest in parts {
            paras.push(vec![(rest.to_string(), *style)]);
        }
    }
    paras
}

pub fn transcript(messages: &[Message], expanded: bool, width: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut prev: Option<&Entry> = None;
    for m in messages {
        let body = entry_lines(m, expanded, width);
        if body.is_empty() {
            continue;
        }
        let tight = matches!(
            (prev, &m.entry),
            (Some(Entry::Tool { .. }), Entry::Tool { .. })
                | (Some(Entry::Thinking { .. }), Entry::Tool { .. })
                | (Some(Entry::Tool { .. }), Entry::Thinking { .. })
                | (Some(Entry::Thinking { .. }), Entry::Thinking { .. })
        );
        if !rows.is_empty() && !tight {
            rows.push(Line::default());
        }
        rows.extend(body);
        prev = Some(&m.entry);
    }
    rows
}

fn usage_text(u: &Usage) -> String {
    let mut parts = Vec::new();
    if !u.model.is_empty() {
        parts.push(u.model.clone());
    }
    if let Some(f) = u.fill() {
        parts.push(format!("{:.0}% of {}k", f * 100.0, u.window / 1000));
    }
    if u.cost_usd > 0.0 {
        parts.push(format!("${:.2}", u.cost_usd));
    }
    parts.join("  ")
}

fn status_line(app: &App, width: usize) -> Line<'static> {
    let (tone, text) = match &app.flash {
        Some((Tone::Error, t, _)) => (Color::Red, t.clone()),
        Some((Tone::Info, t, _)) => (Color::Yellow, t.clone()),
        None => (DIM, String::new()),
    };
    if !text.is_empty() {
        return line(piece(&truncate(&text, width), Style::default().fg(tone)));
    }
    let mut left: Vec<Piece> = Vec::new();
    match app.session.as_ref().map(|s| s.status) {
        Some(SessionStatus::Running) => left.push((
            format!("{} working", SPINNER[app.frame % SPINNER.len()]),
            Style::default().fg(Color::Yellow),
        )),
        Some(SessionStatus::Waiting) => {
            left.push(("? waiting for your answer".into(), Style::default().fg(Color::Magenta)))
        }
        Some(SessionStatus::Failed) => {
            left.push(("failed".into(), Style::default().fg(Color::Red)))
        }
        _ => {}
    }
    if !app.online {
        left.push(("  offline".into(), Style::default().fg(Color::Red)));
    }
    let right = match &app.session {
        Some(s) => format!("{}  {}", s.agent, usage_text(&s.usage)),
        None => {
            let model = app.model.as_deref().map(|m| format!("  {m}")).unwrap_or_default();
            format!("{}{model}", app.agent)
        }
    };
    let used: usize = left.iter().map(|(t, _)| cols(t)).sum();
    let right = truncate(right.trim(), width.saturating_sub(used + 2));
    let gap = width.saturating_sub(used + cols(&right));
    left.push((" ".repeat(gap), Style::default()));
    left.push((right, dim()));
    line(left)
}

fn title_line(app: &App, width: usize) -> Line<'static> {
    let title = app.session.as_ref().map(|s| s.title.as_str()).unwrap_or("new chat");
    let rest = format!(" {}", title);
    let head = "agentbench";
    let mut spans = vec![
        (head.to_string(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        (truncate(&rest, width.saturating_sub(cols(head))), dim()),
    ];
    spans.retain(|(t, _)| !t.is_empty());
    line(spans)
}

fn option_rows(pending: &Pending, pick: &Pick, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let index = pick.index.min(pending.questions.len() - 1);
    let q = &pending.questions[index];
    let head = if pending.questions.len() > 1 {
        format!("question {} of {}", index + 1, pending.questions.len())
    } else {
        "question".to_string()
    };
    rows.push(line(piece(&head, Style::default().fg(Color::Magenta))));
    for (i, o) in q.options.iter().enumerate() {
        let on = i == pick.at;
        let box_ = if q.multiple {
            if pick.checked.get(i).copied().unwrap_or(false) { "[x] " } else { "[ ] " }
        } else {
            ""
        };
        let marker = if on { "❯ " } else { "  " };
        let style = if on {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let label = truncate(&format!("{box_}{}", o), width.saturating_sub(8));
        rows.push(line(vec![
            (marker.into(), style),
            (format!("{} ", i + 1), dim()),
            (label, style),
        ]));
    }
    let on = pick.at == q.options.len();
    let style = if on { Style::default().fg(ACCENT).add_modifier(Modifier::BOLD) } else { dim() };
    rows.push(line(vec![
        ((if on { "❯ " } else { "  " }).into(), style),
        ("o other: type your own answer".into(), style),
    ]));
    let hint = if q.multiple {
        "↑↓ move  space tick  enter send  1-9 tick"
    } else {
        "↑↓ move  enter pick  1-9 pick now"
    };
    rows.push(line(piece(hint, dim())));
    rows
}

fn menu_rows(app: &App, width: usize) -> Vec<Line<'static>> {
    let items = app.slash_menu();
    let at = app.menu_at.min(items.len().saturating_sub(1));
    items
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let on = i == at;
            let name = format!("/{} {}", c.name, c.args);
            let style = if on { Style::default().fg(ACCENT) } else { Style::default() };
            let pad = " ".repeat(28usize.saturating_sub(cols(&name)).max(1));
            let about = truncate(c.about, width.saturating_sub(cols(&name) + pad.len() + 2));
            line(vec![
                ((if on { "❯ " } else { "  " }).into(), style),
                (name, style),
                (pad, Style::default()),
                (about, dim()),
            ])
        })
        .collect()
}

fn overlay_lines(app: &App, width: usize) -> Vec<Line<'static>> {
    match &app.overlay {
        Overlay::None => Vec::new(),
        Overlay::Help => {
            let mut rows = vec![line(piece("keys", Style::default().add_modifier(Modifier::BOLD)))];
            for (k, about) in [
                ("enter", "send"),
                ("alt+enter, ctrl+j, \\ enter", "new line"),
                ("up / down", "recall earlier messages"),
                ("esc, ctrl+c", "stop the agent"),
                ("ctrl+c twice, ctrl+d", "leave; the agent keeps working"),
                ("pgup / pgdn, wheel", "scroll"),
                ("ctrl+end", "back to the bottom"),
                ("ctrl+o", "show tool output and thinking"),
            ] {
                rows.push(line(vec![
                    (format!("  {k:<28}"), Style::default().fg(ACCENT)),
                    (about.into(), dim()),
                ]));
            }
            rows.push(Line::default());
            rows.push(line(piece("commands", Style::default().add_modifier(Modifier::BOLD))));
            for c in commands() {
                let name = format!("/{} {}", c.name, c.args);
                rows.push(line(vec![
                    (format!("  {name:<28}"), Style::default().fg(ACCENT)),
                    (truncate(c.about, width.saturating_sub(32)), dim()),
                ]));
            }
            rows.push(Line::default());
            rows.push(line(piece("any key to close", dim())));
            rows
        }
        Overlay::Sessions { items, at } => {
            let mut rows = vec![line(piece(
                "earlier chats  ↑↓ move  enter open  esc close",
                Style::default().add_modifier(Modifier::BOLD),
            ))];
            if items.is_empty() {
                rows.push(line(piece("  none yet", dim())));
            }
            for (i, s) in items.iter().enumerate() {
                let on = i == *at;
                let style = if on { Style::default().fg(ACCENT) } else { Style::default() };
                rows.push(line(vec![
                    ((if on { "❯ " } else { "  " }).into(), style),
                    (truncate(&s.title, width.saturating_sub(18)), style),
                    (format!("  {}", s.agent), dim()),
                    (format!("  {}", s.status.as_str()), dim()),
                ]));
            }
            rows
        }
    }
}

fn session_ready(s: &Option<Session>) -> bool {
    s.is_some()
}

pub fn draw(f: &mut Frame, app: &mut App, view: &mut View) {
    let area = f.area();
    let width = area.width as usize;
    app.editor.width = width.saturating_sub(2).max(2);

    let (composer_rows, cursor) = app.editor.layout(width.saturating_sub(2).max(2));
    let shown_from = composer_rows.len().saturating_sub(COMPOSER_MAX);
    let composer_h = (composer_rows.len() - shown_from) as u16;

    let choosing = app.choosing();
    let side: Vec<Line<'static>> = if app.overlay != Overlay::None {
        overlay_lines(app, width)
    } else if let Some((pending, pick)) = &choosing {
        option_rows(pending, pick, width)
    } else {
        menu_rows(app, width)
    };
    let side_h = (side.len() as u16).min(area.height.saturating_sub(composer_h + 4));

    let [title, body, side_area, rule, input, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(side_h),
        Constraint::Length(1),
        Constraint::Length(composer_h),
        Constraint::Length(1),
    ])
    .areas(area);

    f.render_widget(Paragraph::new(title_line(app, width)), title);

    if app.lines_stale || view.width != width || view.expanded != app.expanded {
        view.lines = transcript(&app.messages, app.expanded, width);
        view.width = width;
        view.expanded = app.expanded;
        app.lines_stale = false;
    }
    let total = view.lines.len();
    let height = body.height as usize;
    let max_scroll = total.saturating_sub(height);
    app.scroll = app.scroll.min(max_scroll);
    let end = total - app.scroll;
    let start = end.saturating_sub(height);
    let mut visible: Vec<Line<'static>> = view.lines[start..end].to_vec();
    if session_ready(&app.session) || total > 0 {
        while visible.len() < height {
            visible.insert(0, Line::default());
        }
    } else {
        visible = welcome(app, height);
    }
    f.render_widget(Paragraph::new(visible), body);
    if app.scroll > 0 {
        let tag = format!(" ↓ {} more lines ", app.scroll);
        let w = cols(&tag) as u16;
        let r = Rect::new(
            body.right().saturating_sub(w),
            body.bottom().saturating_sub(1),
            w.min(body.width),
            1,
        );
        f.render_widget(
            Paragraph::new(Span::styled(tag, Style::default().fg(Color::Black).bg(DIM))),
            r,
        );
    }

    if side_h > 0 {
        f.render_widget(Clear, side_area);
        f.render_widget(Paragraph::new(side), side_area);
    }

    let busy = app.busy();
    let rule_style = if busy { Style::default().fg(Color::Yellow) } else { dim() };
    f.render_widget(Paragraph::new(Span::styled("─".repeat(width), rule_style)), rule);

    let hint = match &choosing {
        Some(_) => String::new(),
        None => composer_hint(app),
    };
    let rows: Vec<Line<'static>> = composer_rows[shown_from..]
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let lead = if shown_from + i == 0 { "› " } else { "  " };
            let mut spans = vec![Span::styled(lead, Style::default().fg(ACCENT))];
            if r.is_empty() && composer_rows.len() == 1 && app.editor.is_empty() {
                spans.push(Span::styled(hint.clone(), dim()));
            } else {
                spans.push(Span::raw(r.clone()));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(rows), input);
    f.render_widget(Paragraph::new(status_line(app, width)), status);

    if choosing.is_none() && app.overlay == Overlay::None {
        let (row, col) = cursor;
        if row >= shown_from {
            let y = input.y + (row - shown_from) as u16;
            let x = (input.x + 2 + col as u16).min(input.right().saturating_sub(1));
            f.set_cursor_position((x, y));
        }
    }
}

fn composer_hint(app: &App) -> String {
    if app.pending().is_some() {
        "type your answer".into()
    } else if app.busy() {
        "steer the agent while it works".into()
    } else if app.session.is_none() {
        "say something to start a chat, / for commands".into()
    } else {
        "tell the agent what to do next".into()
    }
}

fn welcome(app: &App, height: usize) -> Vec<Line<'static>> {
    let mut rows = vec![Line::default(); height.saturating_sub(5) / 2];
    rows.push(line(piece(
        "agentbench chat",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    rows.push(line(piece(&format!("talking to {} in {}", app.agent, app.project.path), dim())));
    rows.push(Line::default());
    rows.push(line(piece("/ for commands, /resume for an earlier chat, /help for keys", dim())));
    rows
}

#[cfg(test)]
mod tests {
    use super::super::app::tests::*;
    use super::*;
    use agentbench_core::Question;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        let mut view = View::default();
        term.draw(|f| draw(f, app, &mut view)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn joined(rows: &[String]) -> String {
        rows.join("\n")
    }

    #[test]
    fn a_new_chat_shows_the_welcome_and_the_composer() {
        let mut a = app();
        let out = render(&mut a, 60, 14);
        assert!(joined(&out).contains("agentbench chat"));
        assert!(joined(&out).contains("say something to start a chat"));
        assert!(out[13].contains("native"));
    }

    #[test]
    fn the_transcript_shows_messages_oldest_first_at_the_bottom() {
        let mut a = app();
        a.open(
            session(SessionStatus::Idle),
            vec![
                msg(1, Entry::user("what is 2+2")),
                msg(2, Entry::Assistant { text: "It is **four**.".into() }),
            ],
        );
        let out = render(&mut a, 50, 12);
        let text = joined(&out);
        let q = text.find("› what is 2+2").expect("user line");
        let r = text.find("It is four.").expect("assistant line");
        assert!(q < r);
        assert!(!text.contains("**"));
    }

    #[test]
    fn scrolled_up_shows_older_lines_and_a_marker() {
        let mut a = app();
        let msgs =
            (1..=30).map(|i| msg(i, Entry::Assistant { text: format!("line {i}") })).collect();
        a.open(session(SessionStatus::Idle), msgs);
        let bottom = joined(&render(&mut a, 40, 12));
        assert!(bottom.contains("line 30"));
        a.scroll = 20;
        let up = joined(&render(&mut a, 40, 12));
        assert!(!up.contains("line 30"));
        assert!(up.contains("more lines"));
        a.scroll = 10_000;
        let top = joined(&render(&mut a, 40, 12));
        assert!(top.contains("line 1"));
        assert!(a.scroll < 10_000);
    }

    #[test]
    fn tool_output_is_hidden_until_expanded_but_errors_always_show() {
        let tool = |output: &str, error| Entry::Tool {
            call_id: "c".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls -la"}),
            output: Some(output.into()),
            error,
        };
        let mut a = app();
        a.open(
            session(SessionStatus::Idle),
            vec![msg(1, tool("file-one", false)), msg(2, tool("no such file", true))],
        );
        let text = joined(&render(&mut a, 50, 14));
        assert!(text.contains("bash ls -la"));
        assert!(!text.contains("file-one"));
        assert!(text.contains("no such file"));
        a.expanded = true;
        assert!(joined(&render(&mut a, 50, 14)).contains("file-one"));
    }

    #[test]
    fn diagnostics_after_an_edit_show_as_a_row_that_opens_to_the_findings() {
        let note = "<language-server-diagnostics>\nChecked by the language server after your edit; this is the harness speaking, not the user. Fix what is yours:\nsrc/lib.rs: 1 error (rust)\n</language-server-diagnostics>";
        let mut a = app();
        a.open(
            session(SessionStatus::Idle),
            vec![msg(1, Entry::Diagnostics { text: note.into(), broken: vec![] })],
        );
        let text = joined(&render(&mut a, 60, 14));
        assert!(text.contains("diagnostics language server"), "{text}");
        assert!(!text.contains("src/lib.rs"));
        a.expanded = true;
        let text = joined(&render(&mut a, 60, 14));
        assert!(
            text.contains("src/lib.rs: 1 error") && !text.contains("harness speaking"),
            "{text}"
        );
    }

    #[test]
    fn a_running_tool_is_marked_and_long_output_is_cut() {
        let long = (1..=40).map(|i| format!("row{i}")).collect::<Vec<_>>().join("\n");
        let rows =
            tool_lines("bash", &serde_json::json!({"command": "x"}), &Some(long), false, true, 40);
        let text: String = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect::<String>() + "\n")
            .collect();
        assert!(text.contains("row14"));
        assert!(!text.contains("row15"));
        assert!(text.contains("26 more lines"));
        let running =
            tool_lines("bash", &serde_json::json!({"command": "x"}), &None, false, false, 40);
        assert!(running[0].spans[0].content.starts_with('◌'));
    }

    #[test]
    fn a_waiting_question_lists_its_options_with_the_cursor_row() {
        let mut a = app();
        a.open(
            session(SessionStatus::Waiting),
            vec![msg(
                1,
                Entry::Question {
                    id: "q".into(),
                    question: Question {
                        text: "pick one".into(),
                        options: vec!["red".into(), "blue".into()],
                        multiple: false,
                    },
                    answer: None,
                },
            )],
        );
        let out = render(&mut a, 50, 16);
        let text = joined(&out);
        assert!(text.contains("❯ 1 red"));
        assert!(text.contains("  2 blue"));
        assert!(text.contains("o other"));
        assert!(text.contains("waiting for your answer"));
    }

    #[test]
    fn the_slash_menu_appears_above_the_composer() {
        let mut a = app();
        type_text(&mut a, "/th");
        let text = joined(&render(&mut a, 70, 14));
        assert!(text.contains("/thinking"));
        assert!(!text.contains("/quit"));
    }

    #[test]
    fn help_and_the_chat_picker_replace_the_menu() {
        let mut a = app();
        a.overlay = Overlay::Help;
        let text = joined(&render(&mut a, 90, 40));
        assert!(text.contains("alt+enter"));
        assert!(text.contains("/resume"));
        let mut s = session(SessionStatus::Idle);
        s.title = "planning the trip".into();
        a.overlay = Overlay::Sessions { items: vec![s], at: 0 };
        assert!(joined(&render(&mut a, 90, 20)).contains("planning the trip"));
    }

    #[test]
    fn a_long_composer_grows_then_scrolls_to_the_cursor() {
        let mut a = app();
        for i in 0..12 {
            type_text(&mut a, &format!("row {i}"));
            a.key(with(crossterm::event::KeyCode::Enter, crossterm::event::KeyModifiers::ALT), 10);
        }
        let text = joined(&render(&mut a, 40, 24));
        assert!(!text.contains("row 0"));
        assert!(text.contains("row 11"));
    }

    #[test]
    fn the_status_line_shows_progress_the_model_and_cost() {
        let mut a = app();
        let mut s = session(SessionStatus::Running);
        s.usage.model = "qwen".into();
        s.usage.context = 5_000;
        s.usage.window = 20_000;
        s.usage.cost_usd = 0.5;
        a.open(s, vec![]);
        let out = render(&mut a, 70, 10);
        let last = out.last().unwrap();
        assert!(last.contains("working"), "{last}");
        assert!(last.contains("qwen  25% of 20k  $0.50"), "{last}");
    }

    #[test]
    fn rendering_never_panics_on_tiny_terminals() {
        let mut a = app();
        a.open(
            session(SessionStatus::Waiting),
            vec![msg(
                1,
                Entry::Question {
                    id: "q".into(),
                    question: Question {
                        text: "x".into(),
                        options: vec!["a".into(); 20],
                        multiple: true,
                    },
                    answer: None,
                },
            )],
        );
        type_text(&mut a, "hello");
        for (w, h) in [(1, 1), (5, 3), (10, 6), (20, 4)] {
            render(&mut a, w, h);
        }
    }

    #[test]
    fn queued_messages_and_attachments_are_tagged() {
        let mut a = app();
        a.open(
            session(SessionStatus::Running),
            vec![msg(1, Entry::User { text: "steer".into(), attachments: vec![], queued: true })],
        );
        assert!(joined(&render(&mut a, 40, 10)).contains("(queued)"));
    }
}
