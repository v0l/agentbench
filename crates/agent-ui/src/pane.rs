use super::chat::Turn;
use super::grants::{Decision, Grants, Request};
use super::settings::{self, Done};
use super::{Agent, Backend};
use egui_bench::markdown;
use egui_bench::prelude::*;
use egui::{Margin, RichText, Ui};

const COLUMN: f32 = 860.0;
const ASK_H: f32 = 84.0;
const SEND_W: f32 = 72.0;

pub fn show<H>(ui: &mut Ui, agent: &mut Agent<H>) {
    show_with(ui, agent, &mut Extras::default());
}

pub type Draw<'a> = Box<dyn FnMut(&mut Ui) + 'a>;
pub type Click<'a> = Box<dyn FnMut() + 'a>;

#[derive(Default)]
pub struct Extras<'a> {
    pub status: Option<Draw<'a>>,
    pub below: Option<Draw<'a>>,
    pub on_model: Option<Click<'a>>,
}

pub fn show_with<H>(ui: &mut Ui, agent: &mut Agent<H>, extras: &mut Extras) {
    if agent.settings.is_some() {
        settings_view(ui, agent);
        return;
    }
    let busy = agent.chat.busy();
    let page = column(ui.max_rect(), COLUMN);
    ui.scope_builder(egui::UiBuilder::new().max_rect(page), |ui| {
        ui.add_space(10.0);
        head(ui, agent, busy, extras.on_model.as_mut());
        if let Some(status) = extras.status.as_mut() {
            status(ui);
        }
        ui.add_space(6.0);
        if let Some(why) = agent.chat.config.fault() {
            card(
                ui,
                Some(FAULT),
                |ui| {
                    Line::new().legend("agent").set(why).tint(FAULT).show(ui);
                },
                |ui| {
                    hint(
                        ui,
                        "Press Model to set the server address, the model to ask for and a key.",
                    )
                },
            );
            ui.add_space(6.0);
        }
        access(ui, &agent.chat.grants);
        let placeholder = agent.chat.backend.welcome().placeholder;
        let asked = ask_box(ui, &mut agent.chat.draft, &placeholder, busy);
        if let Some(text) = asked {
            let desk = agent.desk.clone();
            agent.chat.ask(&text, desk);
        }
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(
            ui,
            |ui| {
                ui.set_width(ui.available_width());
                if agent.chat.turns.is_empty() {
                    welcome(ui, agent);
                }
                let Agent { chat, markdown, .. } = agent;
                let backend = chat.backend.clone();
                let last = chat.turns.len().saturating_sub(1);
                for (n, turn) in chat.turns.iter().enumerate() {
                    draw(ui, markdown, backend.as_ref(), turn, n, busy && n == last);
                    ui.add_space(10.0);
                }
                if let Some(below) = extras.below.as_mut() {
                    below(ui);
                }
                if busy {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.add_space(6.0);
                        Line::new().legend("working").size(11.0).show(ui);
                    });
                }
                ui.add_space(8.0);
            },
        );
    });
    agent.markdown.sweep();
}

fn column(area: egui::Rect, width: f32) -> egui::Rect {
    let width = (area.width() - 32.0).min(width).max(120.0);
    let left = area.center().x - width / 2.0;
    egui::Rect::from_min_max(egui::pos2(left, area.top()), egui::pos2(left + width, area.bottom()))
}

fn access(ui: &mut Ui, grants: &Grants) {
    for request in grants.pending() {
        request_card(ui, grants, &request);
        ui.add_space(6.0);
    }
    let shared = grants.list();
    if shared.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        Line::new().legend("shared").size(11.0).show(ui);
        for g in &shared {
            let how = if g.write { "read and write" } else { "read only" };
            let label = format!("{}  {how}", tidy(&g.path));
            let tint = if g.write { READOUT } else { LEGEND };
            let r = chip(ui, &label, tint, g.write).on_hover_text("click to stop sharing");
            if r.clicked() {
                grants.revoke(&g.path);
            }
        }
    });
    ui.add_space(6.0);
}

fn request_card(ui: &mut Ui, grants: &Grants, request: &Request) {
    let what = if request.write { "wants to read and change" } else { "wants to read" };
    card(
        ui,
        Some(READOUT),
        |ui| {
            Line::new().legend("access").set(what).show(ui);
        },
        |ui| {
            Line::new().value(request.path.display().to_string()).wrapped(ui);
            if !request.reason.is_empty() {
                Line::new().note(request.reason.clone()).size(12.0).wrapped(ui);
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if request.write
                    && ui
                        .add(Key::new("allow read and write").solid(READOUT))
                        .on_hover_text("the agent may change files here")
                        .clicked()
                {
                    grants.decide(request.id, Decision::Write);
                }
                if ui
                    .add(Key::new("allow read only").outline(VALUE))
                    .on_hover_text("the agent may read here and not change anything")
                    .clicked()
                {
                    grants.decide(request.id, Decision::Read);
                }
                if ui.add(Key::new("deny").outline(FAULT)).clicked() {
                    grants.decide(request.id, Decision::Deny);
                }
            });
        },
    );
}

fn welcome<H>(ui: &mut Ui, agent: &Agent<H>) {
    let welcome = agent.chat.backend.welcome();
    ui.add_space(40.0);
    Line::new().legend(&welcome.kicker).size(12.0).show(ui);
    ui.add_space(6.0);
    ui.label(RichText::new(welcome.title).font(legend_font(22.0)).color(VALUE));
    ui.add_space(8.0);
    Line::new().note(welcome.text).size(13.0).wrapped(ui);
    ui.add_space(10.0);
    Line::new()
        .legend("workspace")
        .value(agent.chat.workspace_path().display().to_string())
        .size(11.0)
        .elided(ui);
}

/// Below this width the header's three parts do not fit in a row, and are stacked instead.
const HEAD_ROW: f32 = 420.0;

fn head<H>(ui: &mut Ui, agent: &mut Agent<H>, busy: bool, mut on_model: Option<&mut Click<'_>>) {
    let model = agent.chat.config.model.trim().to_string();
    let model = if model.is_empty() { "none".to_string() } else { model };
    let workspace = tidy(&agent.chat.workspace_path());
    let mut buttons = |ui: &mut Ui| {
        if ui.small_button("Model").clicked() {
            match on_model.as_mut() {
                Some(open) => open(),
                None => agent.settings = Some(agent.chat.config.clone()),
            }
        }
        if ui
            .add_enabled(!agent.chat.turns.is_empty() && !busy, egui::Button::new("Clear").small())
            .clicked()
        {
            agent.chat.clear();
        }
        if busy && ui.small_button("Stop").clicked() {
            agent.chat.interrupt();
        }
    };
    if ui.available_width() >= HEAD_ROW {
        ui.horizontal(|ui| {
            Line::new().legend("model").set(model).size(11.0).show(ui);
            ui.add_space(12.0);
            Line::new().legend("workspace").value(workspace).size(11.0).elided(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), buttons);
        });
    } else {
        ui.horizontal_wrapped(|ui| buttons(ui));
        Line::new().legend("model").set(model).size(11.0).wrapped(ui);
        Line::new().legend("workspace").value(workspace).size(11.0).wrapped(ui);
    }
}

pub(super) fn tidy(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(&h).ok().map(|p| p.to_path_buf())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn settings_view<H>(ui: &mut Ui, agent: &mut Agent<H>) {
    let app = agent.chat.backend.app();
    let dir = agent.chat.backend.config_dir();
    let Some(draft) = agent.settings.as_mut() else { return };
    let mut done = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let page = column(ui.max_rect(), 560.0);
        ui.scope_builder(egui::UiBuilder::new().max_rect(page), |ui| {
            done = settings::show(ui, draft, app, dir.as_deref());
        });
    });
    match done {
        Some(Done::Save(config)) => {
            let saved = config.save(agent.chat.backend.config_dir().as_deref());
            agent.chat.config = config;
            agent.settings = None;
            if let Err(e) = saved {
                agent.chat.turns.push(Turn::Fault(format!("could not save the settings: {e}")));
            }
        }
        Some(Done::Cancel) => agent.settings = None,
        None => {}
    }
}

fn ask_box(ui: &mut Ui, draft: &mut String, placeholder: &str, busy: bool) -> Option<String> {
    let mut asked = None;
    egui::Panel::bottom("ask")
        .exact_size(ASK_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(Margin::symmetric(0, 8)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let box_w = (ui.available_width() - SEND_W - 8.0).max(80.0);
                let entry = ui.add_sized(
                    egui::vec2(box_w, ASK_H - 16.0),
                    egui::TextEdit::multiline(draft).hint_text(placeholder).desired_rows(3),
                );
                let send = ui
                    .add_enabled(
                        !busy,
                        egui::Button::new("Send").min_size(egui::vec2(SEND_W, 30.0)),
                    )
                    .clicked();
                let entered = entry.has_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                if !busy && (send || entered) {
                    let text = draft.trim().to_string();
                    draft.clear();
                    if !text.is_empty() {
                        asked = Some(text);
                    }
                }
            });
        });
    asked
}

fn draw(
    ui: &mut Ui,
    md: &mut markdown::Cache,
    backend: &dyn Backend,
    turn: &Turn,
    n: usize,
    live: bool,
) {
    match turn {
        Turn::You(text) => {
            egui::Frame::NONE
                .fill(PANEL)
                .stroke(egui::Stroke::new(1.0, ETCH))
                .corner_radius(2)
                .inner_margin(Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    Line::new().legend("you").size(10.5).show(ui);
                    ui.add_space(2.0);
                    ui.add(egui::Label::new(RichText::new(text).size(13.0).color(VALUE)).wrap());
                });
        }
        Turn::Said(text) => {
            Line::new().legend("agent").size(10.5).show(ui);
            ui.add_space(2.0);
            markdown::show(ui, md, text, n);
            if live {
                ui.ctx().request_repaint();
            }
        }
        Turn::Did { name, args, answer } => call(ui, backend, name, args, answer, n),
        Turn::Fault(e) => {
            egui::Frame::NONE
                .stroke(egui::Stroke::new(1.0, FAULT))
                .corner_radius(2)
                .inner_margin(Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    Line::new().value(e.clone()).tint(FAULT).wrapped(ui);
                });
        }
    }
}

fn call(
    ui: &mut Ui,
    backend: &dyn Backend,
    name: &str,
    args: &str,
    answer: &Option<Result<String, String>>,
    n: usize,
) {
    let tint = match answer {
        Some(Err(_)) => FAULT,
        None => READOUT,
        Some(Ok(_)) => LEGEND,
    };
    let mark = match answer {
        None => "running",
        Some(Ok(_)) => "done",
        Some(Err(_)) => "failed",
    };
    let summary = summary(backend, name, args);
    let body = match answer {
        Some(Ok(t)) | Some(Err(t)) => Some(t.as_str()),
        None => None,
    };
    let failed = matches!(answer, Some(Err(_)));
    egui::Frame::NONE
        .fill(WELL)
        .stroke(egui::Stroke::new(1.0, if failed { FAULT } else { ETCH }))
        .corner_radius(2)
        .inner_margin(Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let id = ui.make_persistent_id(("call", n));
            let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                id,
                false,
            );
            let row = ui.horizontal(|ui| {
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(14.0, 18.0), egui::Sense::click());
                let open = state.is_open();
                let c = rect.center();
                let points = match open {
                    true => vec![
                        c + egui::vec2(-4.0, -2.0),
                        c + egui::vec2(4.0, -2.0),
                        c + egui::vec2(0.0, 3.0),
                    ],
                    false => vec![
                        c + egui::vec2(-2.0, -4.0),
                        c + egui::vec2(3.0, 0.0),
                        c + egui::vec2(-2.0, 4.0),
                    ],
                };
                ui.painter().add(egui::Shape::convex_polygon(points, LEGEND, egui::Stroke::NONE));
                if resp.clicked() && body.is_some() {
                    state.toggle(ui);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    Line::new().legend(mark).size(9.5).tint(tint).show(ui);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        Line::new()
                            .legend(name)
                            .size(10.5)
                            .tint(tint)
                            .gap(8.0)
                            .note(summary.clone())
                            .size(11.5)
                            .tint(LEGEND)
                            .elided(ui);
                    });
                });
            });
            if row.response.interact(egui::Sense::click()).clicked() && body.is_some() {
                state.toggle(ui);
            }
            if let (Some(text), true) = (body, state.is_open()) {
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt(("callbody", n))
                    .max_height(280.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(pretty(text)).font(mono(11.5)).color(if failed {
                                    FAULT
                                } else {
                                    VALUE
                                }),
                            )
                            .wrap(),
                        );
                    });
            }
            state.store(ui.ctx());
        });
}

pub fn summary(backend: &dyn Backend, name: &str, args: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else { return short(args) };
    if let Some(text) = backend.summary(name, &v) {
        return short(&text);
    }
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    match name {
        "read_file" | "write_file" | "edit_file" | "list_files" | "search_files" => {
            short(&field("path").or_else(|| field("pattern")).unwrap_or_default())
        }
        "bash" => short(&field("command").unwrap_or_default()),
        "bash_output" | "bash_kill" => field("handle").unwrap_or_else(|| "list".into()),
        "request_access" => short(&field("path").unwrap_or_default()),
        "load_skill" => field("name").unwrap_or_else(|| "list".into()),
        "web_search" | "find_skills" => short(&field("query").unwrap_or_default()),
        "web_fetch" => short(&field("url").unwrap_or_default()),
        "todo_write" => super::todo::summary(&v).unwrap_or_default(),
        "code_outline" | "code_diagnostics" => short(&field("path").unwrap_or_default()),
        "code_find" => short(&field("name").unwrap_or_default()),
        _ => short(args),
    }
}

fn pretty(text: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) if v.is_object() || v.is_array() => {
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string())
        }
        _ => text.to_string(),
    }
}

fn short(args: &str) -> String {
    let one: String = args.split_whitespace().collect::<Vec<_>>().join(" ");
    let one = one.trim_matches(|c| c == '{' || c == '}');
    match one.chars().count() > 90 {
        true => format!("{}…", one.chars().take(90).collect::<String>()),
        false => one.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_arguments_are_cut_and_short_ones_are_not() {
        assert_eq!(short("{\"tool\": \"gcad\"}"), "\"tool\": \"gcad\"");
        let long = format!("{{\"path\":\"{}\"}}", "x".repeat(200));
        assert!(short(&long).ends_with('…'));
        assert!(short(&long).chars().count() <= 91);
    }

    struct Quiet;

    #[super::super::async_trait]
    impl Backend for Quiet {
        fn app(&self) -> &'static str {
            "egui-bench-test"
        }

        fn brief(&self, _: &std::path::Path, _: &[super::super::skillset::Skill]) -> String {
            String::new()
        }

        fn welcome(&self) -> super::super::Welcome {
            super::super::Welcome {
                kicker: String::new(),
                title: String::new(),
                text: String::new(),
                placeholder: String::new(),
            }
        }

        fn summary(&self, name: &str, args: &serde_json::Value) -> Option<String> {
            (name == "launch").then(|| args["tool"].as_str().unwrap_or_default().to_string())
        }
    }

    #[test]
    fn a_call_is_summarised_by_what_it_does() {
        let s = |n: &str, a: &str| summary(&Quiet, n, a);
        assert_eq!(s("write_file", r#"{"path":"a/b.gcad","content":"x"}"#), "a/b.gcad");
        assert_eq!(s("bash", r#"{"command":"ls -la"}"#), "ls -la");
        assert_eq!(s("load_skill", r#"{"name":"agentee"}"#), "agentee");
        assert_eq!(s("load_skill", "{}"), "list");
        assert_eq!(s("launch", r#"{"tool":"gcad"}"#), "gcad");
        assert_eq!(s("list_things", "{}"), "");
        assert_eq!(s("web_fetch", r#"{"url":"https://docs.rs"}"#), "https://docs.rs");
        assert_eq!(s("code_find", r#"{"name":"Shape::area"}"#), "Shape::area");
        assert_eq!(s("bash_output", "{}"), "list");
        assert_eq!(
            s("todo_write", r#"{"items":[{"text":"a","status":"done"},{"text":"b"}]}"#),
            "1 of 2 done"
        );
    }

    #[test]
    fn the_column_is_centred_and_capped() {
        let wide = column(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(1400.0, 900.0)),
            860.0,
        );
        assert_eq!(wide.width(), 860.0);
        assert_eq!(wide.center().x, 700.0);
        let narrow = column(
            egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(400.0, 900.0)),
            860.0,
        );
        assert_eq!(narrow.width(), 368.0);
        assert_eq!(narrow.left(), 16.0);
    }

    #[test]
    fn a_json_answer_is_laid_out_and_text_is_left_alone() {
        assert!(pretty("{\"a\":1}").contains("\n"));
        assert_eq!(pretty("$ gcad check\nok"), "$ gcad check\nok");
    }
}
