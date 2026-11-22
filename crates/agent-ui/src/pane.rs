use super::chat::{Attached, Turn};
use super::grants::{Decision, Grants, Request};
use super::settings::{self, Done};
use super::{Agent, Backend};
use crate::transcript::{self as shared, Action, Item};
use egui_bench::markdown;
use crate::key::Key;
use egui_bench::prelude::*;
use egui::{Margin, RichText, Ui};

const COLUMN: f32 = 860.0;
const ASK_H: f32 = 84.0;

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
        if let Some(plan) = latest_plan(&agent.chat.turns) {
            shared::plan_panel(ui, "agent-pane", &plan, ui.available_width() > 500.0);
        }
        let placeholder = agent.chat.backend.welcome().placeholder;
        agent.chat.take_dropped(ui.ctx());
        let asked = ask_box(ui, &mut agent.chat.draft, &mut agent.chat.files, &placeholder, busy);
        match asked {
            Some(Asked::Send(text, files)) => {
                let desk = agent.desk.clone();
                agent.chat.ask_with(&text, files, desk);
            }
            Some(Asked::Stop) => agent.chat.interrupt(),
            Some(Asked::Attach) => {
                #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
                agent.chat.pick_files(ui.ctx());
            }
            None => {}
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
                let mut answered = None;
                for (n, turn) in chat.turns.iter().enumerate() {
                    if let Some(a) = draw(ui, markdown, backend.as_ref(), turn, n, busy && n == last) {
                        answered = Some(a);
                    }
                    ui.add_space(10.0);
                }
                if let Some((id, answers)) = answered {
                    chat.answer(id, answers);
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
        if ui.add(Key::new("model").outline(VALUE)).clicked() {
            match on_model.as_mut() {
                Some(open) => open(),
                None => agent.settings = Some(agent.chat.config.clone()),
            }
        }
        let thinking = agent.chat.config.thinking;
        let (label, tip) = match thinking {
            None => ("think: auto", "thinking is left to the server, click to turn it on"),
            Some(true) => ("think: on", "thinking is on, click to turn it off"),
            Some(false) => ("think: off", "thinking is off, click to leave it to the server"),
        };
        let r = toggle(ui, label, thinking == Some(true));
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, label));
        if r.on_hover_text(tip).clicked() {
            agent.chat.config.thinking = next_thinking(thinking);
            let _ = agent.chat.config.save(agent.chat.backend.config_dir().as_deref());
        }
        let can_clear = !agent.chat.turns.is_empty() && !busy;
        if ui.add(Key::new("clear").outline(VALUE).enabled(can_clear)).clicked() {
            agent.chat.clear();
        }
        if busy && ui.add(Key::new("stop").outline(FAULT)).clicked() {
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

fn next_thinking(now: Option<bool>) -> Option<bool> {
    match now {
        None => Some(true),
        Some(true) => Some(false),
        Some(false) => None,
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

enum Asked {
    Send(String, Vec<Attached>),
    Stop,
    Attach,
}

fn ask_box(
    ui: &mut Ui,
    draft: &mut String,
    files: &mut Vec<Attached>,
    placeholder: &str,
    busy: bool,
) -> Option<Asked> {
    let mut asked = None;
    let height = ASK_H + if files.is_empty() { 0.0 } else { 36.0 };
    egui::Panel::bottom("ask")
        .exact_size(height)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(Margin::symmetric(0, 8)))
        .show(ui, |ui| {
            if !files.is_empty() {
                let pending: Vec<crate::composer::Pending> = files
                    .iter()
                    .map(|f| crate::composer::Pending {
                        name: &f.name,
                        size: f.data.len() as u64,
                        preview: None,
                    })
                    .collect();
                if let Some(i) = crate::composer::pending_row(ui, &pending) {
                    files.remove(i);
                }
                ui.add_space(4.0);
            }
            let edit = crate::composer::text_box(ui, "agent-pane", draft, placeholder, 2);
            let enter = crate::composer::entered(ui, &edit, draft);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can = !busy && (!draft.trim().is_empty() || !files.is_empty());
                    if (crate::composer::send_key(ui, "send", can, 100.0) || enter) && can {
                        let text = draft.trim().to_string();
                        draft.clear();
                        asked = Some(Asked::Send(text, std::mem::take(files)));
                    }
                    if busy && crate::composer::stop_key(ui) {
                        asked = Some(Asked::Stop);
                    }
                    if crate::composer::attach_key(ui) {
                        asked = Some(Asked::Attach);
                    }
                });
            });
        });
    asked
}

fn latest_plan(turns: &[Turn]) -> Option<Vec<(shared::Check, String)>> {
    turns.iter().rev().find_map(|t| match t {
        Turn::Did { name, args, .. } => {
            shared::checklist(name, &serde_json::from_str(args).unwrap_or_default())
        }
        _ => None,
    })
}

fn draw(
    ui: &mut Ui,
    md: &mut markdown::Cache,
    backend: &dyn Backend,
    turn: &Turn,
    n: usize,
    live: bool,
) -> Option<(u64, Vec<String>)> {
    let mut show = shared::Show {
        markdown: md,
        files: &mut shared::NoFiles,
        now: 0,
        thinking: true,
        reading: None,
        read_aloud: false,
        save: false,
    };
    let id = n as i64;
    match turn {
        Turn::You { text, files } => {
            let shown: Vec<shared::File> = files
                .iter()
                .enumerate()
                .map(|(i, f)| shared::File {
                    id: i as i64,
                    name: &f.name,
                    mime: &f.mime,
                    size: f.data.len() as u64,
                })
                .collect();
            shared::entry(ui, &mut show, id, &Item::You { text, files: &shown, from: &[], at: None, queued: false });
        }
        Turn::Said(text) => {
            shared::entry(ui, &mut show, id, &Item::Said { text });
        }
        Turn::Speed(text) => {
            shared::entry(ui, &mut show, id, &Item::Speed { text });
        }
        Turn::Thinking(text) => {
            shared::entry(ui, &mut show, id, &Item::Thinking { text });
        }
        Turn::Did { name, args, answer } => {
            let input: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
            let (output, error) = match answer {
                None => (None, false),
                Some(Ok(t)) => (Some(shared::pretty(t)), false),
                Some(Err(t)) => (Some(t.clone()), true),
            };
            let summary = summary(backend, name, args);
            let item = Item::Tool {
                name,
                input: &input,
                output: output.as_deref(),
                error,
                summary: Some(&summary),
            };
            shared::entry(ui, &mut show, id, &item);
        }
        Turn::Asked { id: qid, questions, answers } => {
            let key = qid.to_string();
            let qs: Vec<shared::Question> = questions
                .iter()
                .map(|q| shared::Question {
                    text: &q.text,
                    options: &q.options,
                    multiple: q.multiple,
                    secret: false,
                })
                .collect();
            let item = Item::Questions { id: &key, questions: &qs, answers: answers.as_deref() };
            if let Some(Action::Answer(a)) = shared::entry(ui, &mut show, id, &item) {
                return Some((*qid, a));
            }
        }
        Turn::Fault(e) => {
            shared::entry(ui, &mut show, id, &Item::Error { text: e });
        }
    }
    if live {
        ui.ctx().request_repaint();
    }
    None
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

    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use serde_json::json;

    fn agent() -> Agent<()> {
        let ctx = egui::Context::default();
        let mut a = Agent::new(&ctx, std::sync::Arc::new(Quiet), true);
        a.chat.config.model = "qwen3".into();
        a
    }

    fn transcript(a: &mut Agent<()>) {
        let t = &mut a.chat.turns;
        t.push(Turn::You {
            text: "The current credit system isnt very good. Look at how credits are applied.".into(),
            files: vec![Attached {
                name: "billing.csv".into(),
                mime: "text/csv".into(),
                data: std::sync::Arc::new(vec![0; 2048]),
            }],
        });
        t.push(Turn::Thinking("I should find where credits are deducted first.".into()));
        t.push(Turn::Did {
            name: "todo_write".into(),
            args: json!({"items": [
                {"text": "find the credit code", "status": "done"},
                {"text": "add a ledger row", "status": "in_progress"},
                {"text": "show it on the billing page"},
            ]})
            .to_string(),
            answer: Some(Ok("saved".into())),
        });
        t.push(Turn::Did {
            name: "search_files".into(),
            args: json!({"pattern": "apply_credit"}).to_string(),
            answer: Some(Ok("src/billing.rs:88: fn apply_credit(\nsrc/api.rs:210: apply_credit(".into())),
        });
        t.push(Turn::Did {
            name: "bash".into(),
            args: json!({"command": "cargo test -p billing"}).to_string(),
            answer: Some(Err("error[E0425]: cannot find value `ledger`\n[exit 101 in 4.2s]".into())),
        });
        t.push(Turn::Said(
            "Credits are added to the balance with no **ledger row**, which is why users cannot see them.".into(),
        ));
        t.push(Turn::Asked {
            id: 1,
            questions: vec![super::super::chat::Asking {
                text: "Show the ledger on the billing page?".into(),
                options: vec!["yes".into(), "no".into()],
                multiple: false,
            }],
            answers: None,
        });
        t.push(Turn::Fault("the model server went away".into()));
    }

    fn pane(a: Agent<()>) -> Harness<'static, Agent<()>> {
        let mut setup = egui_kittest::wgpu::default_wgpu_setup();
        if let egui_wgpu::WgpuSetup::CreateNew(new) = &mut setup {
            new.instance_descriptor.flags = egui_wgpu::wgpu::InstanceFlags::empty();
        }
        let mut h = Harness::builder().with_size([900.0, 1100.0]).wgpu_setup(setup).build_ui_state(
            |ui, a: &mut Agent<()>| {
                egui_bench::theme::install(ui.ctx());
                show(ui, a);
            },
            a,
        );
        h.run_steps(4);
        h
    }

    fn painted(h: &Harness<'_, Agent<()>>) -> String {
        fn walk(s: &egui::Shape, out: &mut String) {
            match s {
                egui::Shape::Text(t) => {
                    out.push_str(t.galley.text());
                    out.push('\n');
                }
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        let mut out = String::new();
        for c in &h.output().shapes {
            walk(&c.shape, &mut out);
        }
        out.to_lowercase()
    }

    #[test]
    fn the_pane_draws_every_turn_kind_with_the_shared_widgets() {
        let mut a = agent();
        transcript(&mut a);
        let mut h = pane(a);
        let text = painted(&h);
        for words in [
            "the current credit system",
            "billing.csv 2 kb",
            "i should find where credits are deducted",
            "plan",
            "1 of 3 done",
            "apply_credit",
            "$ cargo test -p billing",
            "failed",
            "ledger row",
            "show the ledger on the billing page?",
            "the model server went away",
        ] {
            assert!(text.contains(words), "{words} is drawn");
        }
        if let Ok(dir) = std::env::var("AGENT_UI_SHOTS") {
            let img = h.render().expect("rendered");
            img.save(std::path::Path::new(&dir).join("agent-ui-pane.png")).unwrap();
        }
        h.get_by_label("yes").click();
        h.run_steps(3);
        assert!(
            matches!(h.state().chat.turns.get(6), Some(Turn::Asked { answers: Some(a), .. }) if a == &["yes".to_string()]),
            "a click answers the question"
        );
    }

    #[test]
    fn a_reply_shows_its_speed_and_the_attach_key_and_chip_are_findable() {
        let mut a = agent();
        transcript(&mut a);
        a.chat.turns.push(Turn::Speed("prefill 850.5 tok/s (120 tok), decode 32.2 tok/s (40 tok)".into()));
        let h = pane(a);
        h.get_by_label("attach");
        h.get_by_label("attached billing.csv 2 KB");
        assert!(painted(&h).contains("decode 32.2 tok/s"));
    }

    #[test]
    fn model_and_clear_are_keys_and_clear_empties_the_chat() {
        let mut a = agent();
        transcript(&mut a);
        let mut h = pane(a);
        h.get_by_label("model");
        h.get_by_label("clear").click();
        h.run_steps(3);
        assert!(h.state().chat.turns.is_empty());
        h.get_by_label("model").click();
        h.run_steps(3);
        assert!(h.state().settings.is_some());
    }

    #[test]
    fn the_think_toggle_cycles_auto_on_off_auto() {
        let mut h = pane(agent());
        h.get_by_label("think: auto");
        for (label, want) in [("think: auto", Some(true)), ("think: on", Some(false)), ("think: off", None)] {
            h.get_by_label(label).click();
            h.run_steps(3);
            assert_eq!(h.state().chat.config.thinking, want);
        }
        h.get_by_label("think: auto");
    }

    #[test]
    fn the_head_renders_at_narrow_and_wide_widths() {
        let Ok(dir) = std::env::var("AGENT_UI_SHOTS") else { return };
        for width in [900.0, 390.0] {
            let mut a = agent();
            transcript(&mut a);
            let mut setup = egui_kittest::wgpu::default_wgpu_setup();
            if let egui_wgpu::WgpuSetup::CreateNew(new) = &mut setup {
                new.instance_descriptor.flags = egui_wgpu::wgpu::InstanceFlags::empty();
            }
            let mut h = Harness::builder().with_size([width, 500.0]).wgpu_setup(setup).build_ui_state(
                |ui, a: &mut Agent<()>| {
                    egui_bench::theme::install(ui.ctx());
                    show(ui, a);
                },
                a,
            );
            h.run_steps(4);
            let img = h.render().expect("rendered");
            img.save(std::path::Path::new(&dir).join(format!("agent-ui-head-{width}.png"))).unwrap();
            h.state_mut().settings = Some(h.state().chat.config.clone());
            h.run_steps(4);
            let img = h.render().expect("rendered");
            img.save(std::path::Path::new(&dir).join(format!("agent-ui-settings-{width}.png"))).unwrap();
        }
    }

    #[test]
    fn the_send_key_asks_and_takes_the_files_with_it() {
        let mut a = agent();
        a.chat.config.model.clear();
        a.chat.draft = "hello".into();
        a.chat.files.push(Attached { name: "a.txt".into(), mime: "text/plain".into(), data: std::sync::Arc::new(b"hi".to_vec()) });
        let mut h = pane(a);
        assert!(painted(&h).contains("a.txt 2 b"));
        h.get_by_label("send").click();
        h.run_steps(3);
        let a = h.state();
        assert!(a.chat.files.is_empty());
        assert!(matches!(&a.chat.turns[0], Turn::You { text, files } if text == "hello" && files.len() == 1));
    }
}
