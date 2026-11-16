use crate::form::{field, field_then, secret};
use crate::widgets::{code_block, option_row, press, rail_block, read_link, size, spinner};
use egui::{Margin, Sense, Ui, Vec2};
use egui_bench::key::Key;
use egui_bench::markdown;
use egui_bench::prelude::*;
use serde_json::Value;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct File<'a> {
    pub id: i64,
    pub name: &'a str,
    pub mime: &'a str,
    pub size: u64,
}

impl File<'_> {
    pub fn is_image(&self) -> bool {
        self.mime.split('/').next().is_some_and(|t| t.trim().eq_ignore_ascii_case("image"))
    }

    pub fn is_text(&self) -> bool {
        self.mime.starts_with("text/") || self.mime == "application/json"
    }

    pub fn is_markdown(&self) -> bool {
        self.mime == "text/markdown"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Question<'a> {
    pub text: &'a str,
    pub options: &'a [String],
    pub multiple: bool,
    pub secret: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Check {
    Todo,
    Doing,
    Done,
}

pub enum Item<'a> {
    You { text: &'a str, files: &'a [File<'a>], from: &'a [String], at: Option<i64>, queued: bool },
    Said { text: &'a str },
    Thinking { text: &'a str },
    Tool {
        name: &'a str,
        input: &'a Value,
        output: Option<&'a str>,
        error: bool,
        summary: Option<&'a str>,
    },
    Questions { id: &'a str, questions: &'a [Question<'a>], answers: Option<&'a [String]> },
    Error { text: &'a str },
    Notice { text: &'a str },
    Warning { text: &'a str },
    File { caption: &'a str, file: File<'a> },
    Summary { title: &'a str, text: &'a str },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Answer(Vec<String>),
    Read,
    Save,
    Open,
}

pub enum Thumb {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

pub enum Body {
    Loading,
    Ready(Arc<str>),
    Failed,
}

pub trait Files {
    fn thumb(&mut self, ctx: &egui::Context, file: &File) -> Option<Thumb>;

    fn text(&mut self, ctx: &egui::Context, file: &File) -> Option<Body>;
}

pub struct NoFiles;

impl Files for NoFiles {
    fn thumb(&mut self, _: &egui::Context, _: &File) -> Option<Thumb> {
        None
    }

    fn text(&mut self, _: &egui::Context, _: &File) -> Option<Body> {
        None
    }
}

pub struct Show<'a> {
    pub markdown: &'a mut markdown::Cache,
    pub files: &'a mut dyn Files,
    pub now: i64,
    pub thinking: bool,
    pub reading: Option<i64>,
    pub read_aloud: bool,
    pub save: bool,
}

pub fn entry(ui: &mut Ui, show: &mut Show, id: i64, item: &Item) -> Option<Action> {
    match item {
        Item::You { text, files, from, at, queued } => {
            you(ui, show, id, text, files, from, *at, *queued);
            None
        }
        Item::Said { text } => {
            let mut read = false;
            egui::Frame::NONE
                .inner_margin(Margin { left: 13, right: 8, top: 2, bottom: 2 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    markdown::show(ui, show.markdown, text, id as usize);
                    if show.read_aloud && text.trim().len() > 1 {
                        read = read_link(ui, show.reading == Some(id)).clicked();
                    }
                });
            read.then_some(Action::Read)
        }
        Item::Thinking { text } => {
            if show.thinking {
                thinking(ui, text);
            }
            None
        }
        Item::Tool { name, input, output, error, summary } => {
            tool_row(ui, id, name, input, *output, *error, *summary);
            None
        }
        Item::Questions { id: qid, questions: qs, answers } => {
            questions(ui, show.markdown, qid, qs, *answers).map(Action::Answer)
        }
        Item::Error { text } => {
            fault(ui, text);
            None
        }
        Item::Notice { text } => {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                Line::new().note(text.to_string()).size(11.0).wrapped(ui);
            });
            None
        }
        Item::Warning { text } => {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                Line::new()
                    .legend("warning")
                    .tint(WARN)
                    .size(10.0)
                    .gap(6.0)
                    .note(text.to_string())
                    .tint(WARN)
                    .size(11.0)
                    .wrapped(ui);
            });
            None
        }
        Item::File { caption, file } => sent_file(ui, show, id, caption, file),
        Item::Summary { title, text } => {
            heading(ui, title);
            if !text.is_empty() {
                rail_block(ui, OK, PANEL, |ui| {
                    markdown::show(ui, show.markdown, text, id as usize);
                });
                ui.add_space(4.0);
            }
            None
        }
    }
}

pub fn thinking(ui: &mut Ui, text: &str) {
    rail_block(ui, egui::Color32::TRANSPARENT, egui::Color32::TRANSPARENT, |ui| {
        ui.add(
            egui::Label::new(egui::RichText::new(text.trim()).italics().size(12.0).color(LEGEND))
                .wrap(),
        );
    });
}

pub fn fault(ui: &mut Ui, text: &str) {
    rail_block(ui, FAULT, FAULT.gamma_multiply(0.08), |ui| {
        Line::new().legend("error").tint(FAULT).size(10.0).show(ui);
        ui.add(egui::Label::new(egui::RichText::new(text).font(mono(11.5)).color(FAULT)).wrap());
    });
}

pub fn first_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
pub fn you(
    ui: &mut Ui,
    show: &mut Show,
    id: i64,
    text: &str,
    files: &[File],
    from: &[String],
    at: Option<i64>,
    queued: bool,
) {
    let who = if from.is_empty() { "you".to_string() } else { format!("from {}", from.join(", ")) };
    let foldable = !queued && !from.is_empty();
    let open_id = egui::Id::unique(("you-open", id));
    let mut open = !foldable || ui.ctx().data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
    let when = at.map(|t| crate::widgets::ago(t, show.now)).unwrap_or_default();
    ui.add_space(6.0);
    let (rail, fill) = if queued {
        (LEGEND, egui::Color32::TRANSPARENT)
    } else if foldable && !open {
        (ETCH, egui::Color32::TRANSPARENT)
    } else {
        (READOUT, PANEL)
    };
    rail_block(ui, rail, fill, |ui| {
        if queued {
            Line::new()
                .legend("you")
                .size(10.0)
                .gap(8.0)
                .note("queued: the agent has not read this yet")
                .size(10.0)
                .show(ui);
        } else if foldable {
            let head = ui
                .horizontal(|ui| {
                    let mark = if open { "▾" } else { "▸" };
                    ui.label(egui::RichText::new(mark).font(mono(11.0)).color(LEGEND));
                    let mut line = Line::new().legend(&who).size(10.0).gap(8.0).note(when.clone()).size(10.0);
                    if !open {
                        line = line.gap(8.0).value(first_line(text)).size(11.0).tint(VALUE);
                    }
                    line.elided(ui);
                })
                .response
                .interact(Sense::click());
            head.widget_info(|| {
                egui::WidgetInfo::selected(egui::Role::Button, true, open, who.clone())
            });
            if head.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if head.clicked() {
                open = !open;
                ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
            }
        } else {
            Line::new().legend(&who).size(10.0).gap(8.0).note(when.clone()).size(10.0).show(ui);
        }
        if !open {
            return;
        }
        ui.add_space(2.0);
        if !text.is_empty() {
            markdown::show(ui, show.markdown, text, id as usize);
        }
        if !files.is_empty() {
            ui.add_space(4.0);
            attached(ui, show.files, files);
        }
    });
    ui.add_space(4.0);
}

fn sent_file(ui: &mut Ui, show: &mut Show, id: i64, caption: &str, file: &File) -> Option<Action> {
    let mut action = None;
    ui.add_space(6.0);
    rail_block(ui, OK, PANEL, |ui| {
        Line::new()
            .legend("file")
            .size(10.0)
            .gap(8.0)
            .value(file.name.to_string())
            .size(11.0)
            .gap(8.0)
            .note(size(file.size))
            .size(10.0)
            .show(ui);
        if !caption.is_empty() {
            ui.add_space(2.0);
            markdown::show(ui, show.markdown, caption, id as usize);
        }
        ui.add_space(4.0);
        if file.is_image() {
            attached(ui, show.files, std::slice::from_ref(file));
        } else if file.is_text() {
            text_preview(ui, show, id, file);
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if show.save {
                if press(ui, "save", true, Key::new("save").outline(VALUE)) {
                    action = Some(Action::Save);
                }
                if press(ui, "open", true, Key::new("open").outline(VALUE)) {
                    action = Some(Action::Open);
                }
            }
        });
    });
    ui.add_space(4.0);
    action
}

const PREVIEW_LINES: usize = 40;
const PREVIEW_CHARS: usize = 6000;

pub fn excerpt(text: &str) -> (String, usize) {
    let mut out = String::new();
    let mut lines = text.lines();
    let mut shown = 0;
    for line in lines.by_ref().take(PREVIEW_LINES) {
        if out.len() + line.len() > PREVIEW_CHARS {
            let room = PREVIEW_CHARS.saturating_sub(out.len());
            out.push_str(&line[..line.floor_char_boundary(room)]);
            return (out, text.lines().count().saturating_sub(shown + 1));
        }
        out.push_str(line);
        out.push('\n');
        shown += 1;
    }
    (out.trim_end().to_string(), lines.count())
}

fn text_preview(ui: &mut Ui, show: &mut Show, id: i64, file: &File) {
    let Some(body) = show.files.text(ui.ctx(), file) else { return };
    match body {
        Body::Ready(text) => {
            let (shown, more) = excerpt(&text);
            if file.is_markdown() {
                markdown::show(ui, show.markdown, &shown, id as usize);
            } else {
                ui.add(
                    egui::Label::new(egui::RichText::new(shown).font(mono(11.5)).color(VALUE))
                        .wrap(),
                );
            }
            if more > 0 {
                Line::new().note(format!("{more} more lines, save to read them")).size(10.0).show(ui);
            }
        }
        Body::Loading => {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
            spinner(ui, r.center(), 8.0, LEGEND);
        }
        Body::Failed => {}
    }
}

pub fn attached(ui: &mut Ui, fetch: &mut dyn Files, files: &[File]) {
    let ctx = ui.ctx().clone();
    ui.horizontal_wrapped(|ui| {
        for a in files {
            let label = format!("{} {}", a.name, size(a.size));
            if a.is_image()
                && let Some(thumb) = fetch.thumb(&ctx, a)
            {
                match thumb {
                    Thumb::Ready(tex) => {
                        let s = tex.size_vec2();
                        let h = s.y.min(160.0);
                        ui.add(egui::Image::new((tex.id(), Vec2::new(s.x * h / s.y.max(1.0), h))))
                            .on_hover_text(&label);
                        continue;
                    }
                    Thumb::Loading => {
                        let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                        spinner(ui, r.center(), 8.0, LEGEND);
                    }
                    Thumb::Failed => {}
                }
            }
            chip(ui, &label, LEGEND, false);
        }
    });
}

#[derive(Clone, Default)]
struct Slot {
    picked: Vec<usize>,
    typed: String,
}

impl Slot {
    fn reply(&self, q: &Question) -> String {
        let mut parts: Vec<&str> =
            self.picked.iter().filter_map(|&i| q.options.get(i)).map(String::as_str).collect();
        let typed = self.typed.trim();
        if !typed.is_empty() {
            parts.push(typed);
        }
        parts.join(", ")
    }
}

fn salt(id: &str, n: usize) -> usize {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (id, n).hash(&mut h);
    h.finish() as usize
}

pub fn questions(
    ui: &mut Ui,
    md: &mut markdown::Cache,
    id: &str,
    qs: &[Question],
    done: Option<&[String]>,
) -> Option<Vec<String>> {
    let rail = if done.is_some() { ETCH } else { READOUT };
    let quick = qs.len() == 1 && !qs[0].multiple && !qs[0].options.is_empty();
    let legend = if qs.len() == 1 { "question" } else { "questions" };
    let key = egui::Id::unique(("question-slots", id));
    let mut sent: Option<Vec<String>> = None;
    card(
        ui,
        Some(rail),
        |ui| {
            Line::new().legend(legend).show(ui);
        },
        |ui| {
            let mut slots = ui.ctx().data(|d| d.get_temp::<Vec<Slot>>(key)).unwrap_or_default();
            slots.resize(qs.len(), Slot::default());
            let mut enter = false;
            for (n, q) in qs.iter().enumerate() {
                if n > 0 {
                    ui.add_space(10.0);
                }
                if qs.len() > 1 {
                    Line::new().legend(&format!("{} of {}", n + 1, qs.len())).size(10.0).show(ui);
                }
                markdown::show(ui, md, q.text, salt(id, n));
                if let Some(a) = done {
                    let said = a.get(n).map(String::as_str).unwrap_or_default();
                    Line::new().legend("you said").gap(8.0).set(said).size(12.0).wrapped(ui);
                    continue;
                }
                let slot = &mut slots[n];
                for (i, o) in q.options.iter().enumerate() {
                    let on = slot.picked.contains(&i);
                    if !option_row(ui, o, q.multiple, on, (id, n, i)).clicked() {
                        continue;
                    }
                    if quick {
                        sent = Some(vec![o.clone()]);
                    } else if q.multiple {
                        if on {
                            slot.picked.retain(|p| *p != i);
                        } else {
                            slot.picked.push(i);
                            slot.picked.sort_unstable();
                        }
                    } else {
                        slot.picked = if on { vec![] } else { vec![i] };
                        slot.typed.clear();
                    }
                }
                let hint = if q.options.is_empty() { "type an answer" } else { "or type an answer" };
                let r = if q.secret {
                    secret(ui, &mut slot.typed)
                } else if quick {
                    let ready = !slot.typed.trim().is_empty();
                    let mut go = false;
                    let r = field_then(ui, &mut slot.typed, hint, 90.0, |ui| {
                        go = ui.add(Key::new("answer").solid(READOUT).enabled(ready)).clicked();
                    });
                    if go && ready {
                        sent = Some(vec![slot.typed.trim().to_string()]);
                    }
                    r
                } else {
                    field(ui, &mut slot.typed, hint)
                };
                if r.changed() && !q.multiple && !slot.typed.trim().is_empty() {
                    slot.picked.clear();
                }
                enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            }
            if done.is_none() && !quick {
                let replies: Vec<String> = qs.iter().zip(&slots).map(|(q, s)| s.reply(q)).collect();
                let ready = replies.iter().all(|r| !r.is_empty());
                ui.add_space(8.0);
                let label = if qs.len() > 1 { "send answers" } else { "answer" };
                let key = ui.add(Key::new(label).solid(READOUT).enabled(ready));
                key.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, ready, label));
                if (key.clicked() || enter) && ready {
                    sent = Some(replies);
                }
            } else if enter && quick {
                let t = slots[0].typed.trim();
                if !t.is_empty() {
                    sent = Some(vec![t.to_string()]);
                }
            }
            ui.ctx().data_mut(|d| {
                if sent.is_some() {
                    d.remove::<Vec<Slot>>(key);
                } else {
                    d.insert_temp(key, slots);
                }
            });
        },
    );
    sent
}

pub fn tool_row(
    ui: &mut Ui,
    id: i64,
    name: &str,
    input: &Value,
    output: Option<&str>,
    error: bool,
    summary: Option<&str>,
) {
    let (tint, state) = match (output, error) {
        (None, _) => (TRACE, "running"),
        (Some(_), true) => (FAULT, "failed"),
        (Some(_), false) => (LEGEND, ""),
    };
    let open_id = egui::Id::unique(("tool-open", id));
    let mut open = ui.ctx().data(|d| d.get_temp::<bool>(open_id)).unwrap_or(error);
    let body = tool_body(name, input);
    let command = shell_command(input);
    let rail = if error {
        FAULT
    } else if output.is_none() {
        TRACE
    } else {
        ETCH
    };
    rail_block(ui, rail, egui::Color32::TRANSPARENT, |ui| {
        let head = ui
            .horizontal(|ui| {
                let mark = if output.is_none() {
                    "◌"
                } else if open {
                    "▾"
                } else {
                    "▸"
                };
                ui.label(egui::RichText::new(mark).font(mono(11.0)).color(tint));
                let mut line = Line::new().legend(name).size(10.0).tint(tint);
                if command.is_none() {
                    let text = summary.map_or_else(|| tool_summary(name, input), str::to_string);
                    line = line.gap(8.0).value(text).size(11.0).tint(VALUE);
                }
                if !state.is_empty() {
                    line = line.gap(10.0).legend(state).size(9.5).tint(tint);
                }
                line.elided(ui);
            })
            .response
            .interact(Sense::click());
        head.widget_info(|| {
            egui::WidgetInfo::selected(egui::Role::Button, true, open, name.to_string())
        });
        if head.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if head.clicked() {
            open = !open;
        }
        if let Some(c) = command {
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!("$ {c}")).font(mono(11.0)).color(VALUE),
                    )
                    .wrap(),
                );
            });
        }
        if !open {
            if let Some(out) = output.filter(|o| !o.trim().is_empty()) {
                let first = out.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
                let more = out.lines().count().saturating_sub(1);
                let tail = if more > 0 { format!("   +{more} lines") } else { String::new() };
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(format!("{}{tail}", first.trim()))
                                .font(mono(10.5))
                                .color(LEGEND.gamma_multiply(0.8)),
                        )
                        .truncate(),
                    );
                });
            }
            return;
        }
        if let Some(b) = &body {
            ui.add_space(2.0);
            code_block(ui, b, VALUE, ("tool-in", id));
        }
        if let Some(out) = output {
            ui.add_space(4.0);
            let text = if out.trim().is_empty() { "no output" } else { out };
            code_block(ui, text, if error { FAULT } else { LEGEND }, ("tool-out", id));
        }
    });
    ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
}

pub fn shell_command(input: &Value) -> Option<&str> {
    input.get("command").and_then(|v| v.as_str()).filter(|c| !c.trim().is_empty())
}

pub fn checklist(name: &str, input: &Value) -> Option<Vec<(Check, String)>> {
    if !matches!(name, "todo_write" | "TodoWrite") {
        return None;
    }
    let items = input.get("items").or_else(|| input.get("todos"))?.as_array()?;
    let list: Vec<(Check, String)> = items
        .iter()
        .filter_map(|i| {
            let text = i.get("text").or_else(|| i.get("content"))?.as_str()?.trim();
            let status = match i.get("status").and_then(|s| s.as_str()) {
                Some("done" | "completed") => Check::Done,
                Some("in_progress" | "doing") => Check::Doing,
                _ => Check::Todo,
            };
            Some((status, text.to_string()))
        })
        .collect();
    (!list.is_empty()).then_some(list)
}

pub fn tool_body(name: &str, input: &Value) -> Option<String> {
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str());
    if shell_command(input).is_some() {
        return None;
    }
    match name {
        "todo_write" | "TodoWrite" => {
            let lines: Vec<String> = checklist(name, input)?
                .into_iter()
                .map(|(c, t)| {
                    let mark = match c {
                        Check::Done => "[x]",
                        Check::Doing => "[~]",
                        Check::Todo => "[ ]",
                    };
                    format!("{mark} {t}")
                })
                .collect();
            Some(lines.join("\n"))
        }
        "edit_file" | "edit" => {
            let old = pick("old").or(pick("oldText"))?;
            let new = pick("new").or(pick("newText")).unwrap_or_default();
            let mut out = String::new();
            for l in old.lines() {
                out.push_str(&format!("- {l}\n"));
            }
            for l in new.lines() {
                out.push_str(&format!("+ {l}\n"));
            }
            Some(out)
        }
        "write_file" | "write" => {
            pick("content").map(|c| c.lines().take(40).collect::<Vec<_>>().join("\n"))
        }
        _ if input.is_object() && input.as_object().is_some_and(|o| o.len() > 1) => {
            serde_json::to_string_pretty(input).ok()
        }
        _ => None,
    }
}

pub fn tool_summary(name: &str, input: &Value) -> String {
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    if let Some(items) = checklist(name, input) {
        let done = items.iter().filter(|(c, _)| *c == Check::Done).count();
        return format!("{done} of {} done", items.len());
    }
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
    let line = text.lines().next().unwrap_or_default().to_string();
    line.chars().take(160).collect()
}

pub fn pretty(text: &str) -> String {
    let t = text.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return text.to_string();
    }
    match serde_json::from_str::<Value>(text) {
        Ok(v) if v.is_object() || v.is_array() => {
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string())
        }
        _ => text.to_string(),
    }
}

pub fn plan_panel(
    ui: &mut Ui,
    salt: impl Hash + std::fmt::Debug + Copy,
    plan: &[(Check, String)],
    open_by_default: bool,
) {
    let done = plan.iter().filter(|(c, _)| *c == Check::Done).count();
    let open_id = egui::Id::unique(("plan-open", salt));
    let mut open = ui.ctx().data(|d| d.get_temp::<bool>(open_id)).unwrap_or(open_by_default);
    let doing = plan.iter().find(|(c, _)| *c == Check::Doing).map(|(_, t)| t.as_str());
    rail_block(ui, ETCH, egui::Color32::TRANSPARENT, |ui| {
        let head = ui
            .horizontal(|ui| {
                ui.label(
                    egui::RichText::new(if open { "▾" } else { "▸" }).font(mono(11.0)).color(LEGEND),
                );
                let mut line = Line::new()
                    .legend("plan")
                    .size(10.0)
                    .tint(LEGEND)
                    .gap(8.0)
                    .value(format!("{done} of {} done", plan.len()))
                    .size(11.0)
                    .tint(VALUE);
                if let (false, Some(t)) = (open, doing) {
                    line = line.gap(10.0).value(t).size(11.0).tint(READOUT);
                }
                line.elided(ui);
            })
            .response
            .interact(Sense::click());
        head.widget_info(|| {
            egui::WidgetInfo::selected(egui::Role::Button, true, open, "plan")
        });
        if head.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if head.clicked() {
            open = !open;
        }
        if open {
            egui::ScrollArea::vertical()
                .id_salt(("plan-scroll", salt))
                .max_height(150.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (status, text) in plan {
                        let (mark, tint) = match status {
                            Check::Done => ("[x]", LEGEND),
                            Check::Doing => ("[~]", READOUT),
                            Check::Todo => ("[ ]", VALUE),
                        };
                        ui.horizontal_top(|ui| {
                            ui.add_space(16.0);
                            ui.label(egui::RichText::new(mark).font(mono(11.0)).color(tint));
                            let mut t = egui::RichText::new(text).font(mono(11.0)).color(tint);
                            if *status == Check::Done {
                                t = t.strikethrough();
                            }
                            ui.add(egui::Label::new(t).wrap());
                        });
                    }
                });
        }
    });
    ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests;
