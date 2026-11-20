use super::*;
use agentbench_core::{Onboarding, Provider, ProviderCheck, ProviderKind, Screen};

const LOGIN_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Step {
    Welcome,
    Setup(ProviderKind),
    Models,
    Finish,
}

impl Step {
    fn saved(self) -> &'static str {
        match self {
            Step::Welcome | Step::Setup(_) => "welcome",
            Step::Models => "models",
            Step::Finish => "finish",
        }
    }

    fn restored(saved: &str) -> Step {
        match saved {
            "models" => Step::Models,
            "finish" => Step::Finish,
            _ => Step::Welcome,
        }
    }
}

#[derive(Default)]
pub(super) struct Login {
    screen: Option<Screen>,
    running: bool,
    asked: Option<Instant>,
    size: (u16, u16),
}

pub(super) struct Onboard {
    data: Onboarding,
    step: Step,
    working: Vec<Provider>,
    edit: Option<Provider>,
    checks: HashMap<ProviderKind, Option<ProviderCheck>>,
    models: HashMap<ProviderKind, Option<Result<Vec<String>, String>>>,
    health: HashMap<ProviderKind, ProviderCheck>,
    health_asked: bool,
    login: Login,
    project: String,
}

enum Act {
    Skip,
    Close,
    Pick(ProviderKind),
    Save(Provider),
    Remove(ProviderKind),
    MakeDefault(ProviderKind),
    Go(Step),
    Test(Provider),
    SignIn(&'static str),
    Recheck,
    Finish(bool),
}

fn blurb(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::ClaudeSubscription => "Sign in with the Claude Code app you already pay for. No key to paste.",
        ProviderKind::CodexSubscription => "Sign in with your ChatGPT plan through the Codex app. No key to paste.",
        ProviderKind::AnthropicKey => "Pay as you go with a key from console.anthropic.com.",
        ProviderKind::OpenaiKey => "Pay as you go with a key from platform.openai.com.",
        ProviderKind::Endpoint => "YALR, Ollama, OpenRouter or any server that speaks the OpenAI API.",
        ProviderKind::Local => "A model running on this machine, such as Ollama or llama.cpp.",
    }
}

fn blank(kind: ProviderKind) -> Provider {
    Provider { kind, url: kind.default_url().to_string(), ..Default::default() }
}

impl Onboard {
    fn new(data: Onboarding) -> Onboard {
        let step = Step::restored(&data.step);
        let working = data.providers.clone();
        Onboard {
            data,
            step,
            working,
            edit: None,
            checks: HashMap::new(),
            models: HashMap::new(),
            health: HashMap::new(),
            health_asked: false,
            login: Login::default(),
            project: String::new(),
        }
    }

    pub(super) fn working_kinds(&self) -> Vec<ProviderKind> {
        self.working.iter().map(|p| p.kind).collect()
    }

    pub(super) fn step_name(&self) -> String {
        match self.step {
            Step::Setup(k) => format!("setup:{}", k.id()),
            other => other.saved().to_string(),
        }
    }

    fn provider(&self, kind: ProviderKind) -> Option<&Provider> {
        self.working.iter().find(|p| p.kind == kind)
    }

    fn cli(&self, kind: ProviderKind) -> &agentbench_core::CliLogin {
        match kind {
            ProviderKind::ClaudeSubscription => &self.data.claude,
            _ => &self.data.codex,
        }
    }
}

impl App {
    pub fn open_onboarding(&mut self) {
        if let Some(c) = &mut self.client {
            c.onboarding();
            c.provider_health();
        }
        self.onboard_opening = true;
    }

    pub(super) fn onboarding_reply(&mut self, data: Onboarding) {
        self.preferred_agent = data.default_agent.clone();
        if let Some(ob) = &mut self.onboard {
            ob.data = data;
            return;
        }
        let wanted = std::mem::take(&mut self.onboard_opening);
        if wanted || data.needed {
            if let Some(c) = &mut self.client {
                c.provider_health();
            }
            self.onboard = Some(Onboard::new(data));
        }
    }

    pub(super) fn providers_reply(&mut self, list: Vec<Provider>) {
        if let Some(c) = &mut self.client {
            c.onboarding();
            c.provider_health();
            c.overview();
        }
        if let Some(ob) = &mut self.onboard {
            ob.working = list;
            ob.edit = None;
        }
    }

    pub(super) fn provider_test_reply(&mut self, kind: ProviderKind, check: ProviderCheck) {
        if let Some(ob) = &mut self.onboard {
            ob.checks.insert(kind, Some(check));
        }
    }

    pub(super) fn provider_models_reply(
        &mut self,
        kind: ProviderKind,
        list: Result<Vec<String>, String>,
    ) {
        let Some(ob) = &mut self.onboard else {
            return;
        };
        if let (Ok(l), Some(p)) = (&list, ob.working.iter_mut().find(|p| p.kind == kind)) {
            let empty = p.low.is_empty() && p.medium.is_empty() && p.high.is_empty();
            if empty && kind == ProviderKind::ClaudeSubscription && l.len() >= 3 {
                p.low = l[0].clone();
                p.medium = l[1].clone();
                p.high = l[2].clone();
                p.supervisor = l[2].clone();
            } else if empty && let Some(first) = l.first() {
                p.medium = first.clone();
            }
        }
        ob.models.insert(kind, Some(list));
    }

    pub(super) fn provider_health_reply(&mut self, list: Vec<agentbench_core::ProviderHealth>) {
        if let Some(ob) = &mut self.onboard {
            ob.health = list
                .into_iter()
                .map(|h| (h.kind, ProviderCheck { ok: h.ok, message: h.message }))
                .collect();
        }
    }

    pub(super) fn login_reply(&mut self, screen: Option<Screen>) {
        let mut finished = false;
        if let Some(ob) = &mut self.onboard {
            finished = ob.login.running && screen.as_ref().is_some_and(|s| !s.alive);
            ob.login.running = screen.as_ref().is_some_and(|s| s.alive);
            if screen.is_some() {
                ob.login.screen = screen;
            }
        }
        if finished && let Some(c) = &mut self.client {
            c.onboarding();
            c.provider_health();
        }
    }

    fn enter_step(&mut self, step: Step) {
        let Some(ob) = &mut self.onboard else {
            return;
        };
        ob.step = step;
        if let Some(c) = &mut self.client {
            c.set_onboarding("open", Some(step.saved()));
            if step == Step::Models {
                for p in &ob.working {
                    if ob.models.contains_key(&p.kind) {
                        continue;
                    }
                    if p.kind != ProviderKind::CodexSubscription {
                        c.provider_models(p);
                    }
                    ob.models.insert(p.kind, None);
                }
            }
        }
    }

    fn close_login(&mut self) {
        if let Some(ob) = &mut self.onboard {
            ob.login = Login::default();
        }
        if let Some(c) = &mut self.client {
            c.login_close();
        }
    }

    pub(super) fn onboarding_modal(&mut self, ctx: &egui::Context) {
        let Some(mut ob) = self.onboard.take() else {
            return;
        };
        let font = mono(12.0);
        let (cell_w, cell_h) = ctx.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let width = modal_width(ctx, 660.0);
        if let Some(c) = &mut self.client
            && matches!(ob.step, Step::Setup(k) if k.subscription())
            && (ob.login.running || ob.login.screen.is_some())
        {
            let now = Instant::now();
            let size = (
                (((ctx.content_rect().height() * 0.28) / cell_h) as u16).max(5),
                (((width - 40.0) / cell_w) as u16).max(20),
            );
            if ob.login.running && ctx.memory(|m| m.focused().is_none()) {
                let data = terminal_bytes(&ctx.input(|i| i.events.clone()));
                let resize = if size != ob.login.size { size } else { (0, 0) };
                if !data.is_empty() || resize != (0, 0) {
                    c.login_send(&data, resize.0, resize.1);
                    ob.login.size = size;
                    ob.login.asked = None;
                }
            }
            if ob.login.asked.is_none_or(|a| now.duration_since(a) >= LOGIN_EVERY) {
                c.login_screen();
                ob.login.asked = Some(now);
            }
            ctx.request_repaint_after(LOGIN_EVERY);
        }
        let mut act: Option<Act> = None;
        let busy_tests: Vec<ProviderKind> =
            ob.checks.iter().filter(|(_, c)| c.is_none()).map(|(k, _)| *k).collect();
        let m = egui::Modal::new(egui::Id::new("onboarding"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(width);
                let title = match ob.step {
                    Step::Welcome => "Set up providers",
                    Step::Setup(k) => k.label(),
                    Step::Models => "Choose models",
                    Step::Finish => "All set",
                };
                modal_title(ui, title);
                let body_h = (ctx.content_rect().height() - 220.0).clamp(240.0, 560.0);
                egui::ScrollArea::vertical()
                    .id_salt(("onboarding", ob.step.saved()))
                    .max_height(body_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width() - 10.0);
                        match ob.step {
                            Step::Welcome => welcome(ui, &mut ob, &mut act),
                            Step::Setup(kind) if kind.subscription() => {
                                subscription(ui, &mut ob, kind, &mut act, &font, cell_h)
                            }
                            Step::Setup(kind) => endpoint(ui, &mut ob, kind, &busy_tests, &mut act),
                            Step::Models => models(ui, &mut ob),
                            Step::Finish => finish(ui, &mut ob),
                        }
                    });
                footer(ui, |ui| match ob.step {
                    Step::Welcome => {
                        let ready = !ob.working.is_empty();
                        if act_key(ui, Key::new("next").solid(READOUT).enabled(ready).min_width(110.0), "next").clicked() {
                            act = Some(Act::Go(Step::Models));
                        }
                        if act_key(ui, Key::new("skip for now").outline(VALUE).min_width(130.0), "skip for now").clicked() {
                            act = Some(Act::Skip);
                        }
                    }
                    Step::Setup(_) => {
                        if act_key(ui, Key::new("back").outline(VALUE).min_width(100.0), "back").clicked() {
                            act = Some(Act::Go(Step::Welcome));
                        }
                    }
                    Step::Models => {
                        if act_key(ui, Key::new("next").solid(READOUT).min_width(110.0), "next").clicked() {
                            act = Some(Act::Go(Step::Finish));
                        }
                        if act_key(ui, Key::new("back").outline(VALUE).min_width(100.0), "back").clicked() {
                            act = Some(Act::Go(Step::Welcome));
                        }
                    }
                    Step::Finish => {
                        if act_key(ui, Key::new("start a session").solid(READOUT).min_width(160.0), "start a session").clicked() {
                            act = Some(Act::Finish(true));
                        }
                        if act_key(ui, Key::new("finish").outline(VALUE).min_width(100.0), "finish").clicked() {
                            act = Some(Act::Finish(false));
                        }
                        if act_key(ui, Key::new("back").outline(VALUE).min_width(100.0), "back").clicked() {
                            act = Some(Act::Go(Step::Models));
                        }
                    }
                });
            });
        if m.should_close() && act.is_none() {
            act = Some(Act::Close);
        }
        self.onboard = Some(ob);
        let Some(act) = act else {
            return;
        };
        self.run_onboarding_act(act);
    }

    fn run_onboarding_act(&mut self, act: Act) {
        match act {
            Act::Close => {
                self.close_login();
                self.onboard = None;
            }
            Act::Skip => {
                self.close_login();
                if let Some(c) = &mut self.client {
                    c.set_onboarding("skipped", Some(""));
                }
                self.onboard = None;
            }
            Act::Pick(kind) => {
                if let Some(ob) = &mut self.onboard {
                    ob.edit = Some(ob.provider(kind).cloned().unwrap_or_else(|| blank(kind)));
                    ob.checks.remove(&kind);
                    ob.login = Login::default();
                }
                self.enter_step(Step::Setup(kind));
                if kind.subscription()
                    && let Some(c) = &mut self.client
                {
                    c.onboarding();
                }
            }
            Act::Save(p) => {
                let Some(ob) = &mut self.onboard else {
                    return;
                };
                let first = ob.working.is_empty();
                let mut list = ob.working.clone();
                let mut p = p;
                match list.iter_mut().find(|x| x.kind == p.kind) {
                    Some(old) => {
                        p.default = old.default;
                        p.low = old.low.clone();
                        p.medium = old.medium.clone();
                        p.high = old.high.clone();
                        p.supervisor = old.supervisor.clone();
                        *old = p;
                    }
                    None => {
                        p.default = first;
                        list.push(p);
                    }
                }
                ob.models.clear();
                if let Some(c) = &mut self.client {
                    c.put_providers(&list);
                }
                self.close_login();
                self.enter_step(Step::Welcome);
            }
            Act::Remove(kind) => {
                if let Some(ob) = &mut self.onboard {
                    let mut list = ob.working.clone();
                    list.retain(|p| p.kind != kind);
                    ob.checks.remove(&kind);
                    ob.health.remove(&kind);
                    if let Some(c) = &mut self.client {
                        c.put_providers(&list);
                    }
                }
            }
            Act::MakeDefault(kind) => {
                if let Some(ob) = &mut self.onboard {
                    let mut list = ob.working.clone();
                    for p in &mut list {
                        p.default = p.kind == kind;
                    }
                    if let Some(c) = &mut self.client {
                        c.put_providers(&list);
                    }
                }
            }
            Act::Go(step) => {
                if step == Step::Finish
                    && let Some(ob) = &mut self.onboard
                    && let Some(c) = &mut self.client
                {
                    c.put_providers(&ob.working.clone());
                }
                if step == Step::Welcome {
                    self.close_login();
                }
                self.enter_step(step);
            }
            Act::Test(p) => {
                if let Some(ob) = &mut self.onboard {
                    ob.checks.insert(p.kind, None);
                }
                if let Some(c) = &mut self.client {
                    c.test_provider(&p);
                }
            }
            Act::SignIn(cli) => {
                if let Some(ob) = &mut self.onboard {
                    ob.login = Login { running: true, ..Login::default() };
                }
                if let Some(c) = &mut self.client {
                    c.login_start(cli);
                    c.login_screen();
                }
            }
            Act::Recheck => {
                if let Some(c) = &mut self.client {
                    c.onboarding();
                    c.provider_health();
                }
            }
            Act::Finish(session) => {
                let Some(ob) = self.onboard.take() else {
                    return;
                };
                self.close_login();
                if let Some(c) = &mut self.client {
                    c.put_providers(&ob.working);
                    c.set_onboarding("done", Some(""));
                    if !ob.project.trim().is_empty() {
                        c.add_project(&NewProject {
                            path: ob.project.trim().to_string(),
                            name: None,
                            create: false,
                        });
                    }
                }
                if session {
                    self.open_start(None);
                }
            }
        }
    }
}

fn act_key(ui: &mut Ui, key: Key, label: &str) -> egui::Response {
    let enabled = true;
    let r = ui.add(key);
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    r
}

fn health_chip(ui: &mut Ui, check: Option<&ProviderCheck>) {
    match check {
        Some(c) if c.ok => chip(ui, "working", OK, true),
        Some(_) => chip(ui, "not working", FAULT, true),
        None => chip(ui, "not checked", LEGEND, false),
    };
}

fn welcome(ui: &mut Ui, ob: &mut Onboard, act: &mut Option<Act>) {
    note(
        ui,
        "Pick every way you want to reach a model. You can set up several and choose which one new chats use.",
        LEGEND,
    );
    ui.add_space(8.0);
    let per_row = if ui.available_width() < 520.0 { 1 } else { 2 };
    for row in ProviderKind::ALL.chunks(per_row) {
        ui.columns(per_row, |cols| {
            for (i, kind) in row.iter().enumerate() {
                provider_card(&mut cols[i], ob, *kind, act);
            }
        });
        ui.add_space(6.0);
    }
    if ob.working.is_empty() {
        hint(ui, "Nothing is set up yet. Choose at least one to continue.");
    }
}

fn provider_card(ui: &mut Ui, ob: &Onboard, kind: ProviderKind, act: &mut Option<Act>) {
    let have = ob.provider(kind);
    let default = have.is_some_and(|p| p.default);
    let health = ob.health.get(&kind);
    let rail = match (have, health) {
        (Some(_), Some(h)) if !h.ok => Some(FAULT),
        (Some(_), _) if default => Some(READOUT),
        (Some(_), Some(h)) if h.ok => Some(OK),
        _ => None,
    };
    card(
        ui,
        rail,
        |ui| {
            Line::new().legend(kind.label()).elided(ui);
        },
        |ui| {
            ui.set_min_height(104.0);
            hint(ui, blurb(kind));
            match (have, health) {
                (Some(_), Some(h)) => {
                    health_chip(ui, Some(h));
                    note(ui, &h.message, if h.ok { LEGEND } else { FAULT });
                }
                (Some(_), None) => health_chip(ui, None),
                (None, _) => {}
            }
            ui.horizontal_wrapped(|ui| {
                let name = if have.is_some() { "edit" } else { "set up" };
                let label = format!("{name} {}", kind.label());
                if act_key(ui, Key::new(name).solid(READOUT).min_width(80.0), &label).clicked() {
                    *act = Some(Act::Pick(kind));
                }
                if have.is_some() {
                    if default {
                        chip(ui, "default", READOUT, true);
                    } else {
                        let label = format!("make {} the default", kind.label());
                        if act_key(ui, Key::new("make default").outline(VALUE), &label).clicked() {
                            *act = Some(Act::MakeDefault(kind));
                        }
                    }
                    let label = format!("remove {}", kind.label());
                    if act_key(ui, Key::new("remove").outline(FAULT), &label).clicked() {
                        *act = Some(Act::Remove(kind));
                    }
                }
            });
        },
    );
}

fn subscription(
    ui: &mut Ui,
    ob: &mut Onboard,
    kind: ProviderKind,
    act: &mut Option<Act>,
    font: &egui::FontId,
    cell_h: f32,
) {
    let cli = ob.cli(kind).clone();
    let (name, command) = match kind {
        ProviderKind::ClaudeSubscription => ("Claude Code", "claude"),
        _ => ("Codex", "codex"),
    };
    section(ui, "this machine", &format!("{name} keeps the login, agentbench never stores it"), |ui| {
        Line::new().legend("app").column(ui, 90.0).value(if cli.installed { "installed" } else { "not installed" }).show(ui);
        if cli.installed && !cli.path.is_empty() {
            hint(ui, &cli.path);
        }
        status(
            ui,
            cli.logged_in,
            &if cli.logged_in {
                cli.detail.clone()
            } else if cli.detail.is_empty() {
                "not signed in".to_string()
            } else {
                format!("not signed in: {}", cli.detail)
            },
        );
        if !cli.installed && !cli.logged_in {
            note(ui, &format!("{name} is not installed on this machine. Install it, then check again."), LEGEND);
            code_block(ui, &cli.install, VALUE, ("install", command));
            if act_key(ui, Key::new("copy command").outline(VALUE), "copy install command").clicked() {
                ui.ctx().copy_text(cli.install.clone());
            }
        }
        ui.horizontal_wrapped(|ui| {
            if cli.installed && !cli.logged_in {
                let label = format!("sign in with {name}");
                if act_key(ui, Key::new("sign in").solid(READOUT).min_width(100.0).enabled(!ob.login.running), &label).clicked() {
                    *act = Some(Act::SignIn(command));
                }
            }
            if act_key(ui, Key::new("check again").outline(VALUE), "check again").clicked() {
                *act = Some(Act::Recheck);
            }
        });
    });
    if let Some(s) = &ob.login.screen {
        ui.add_space(8.0);
        section(ui, "sign in", "complete the steps here and in your browser", |ui| {
            egui::Frame::NONE.fill(CHASSIS).inner_margin(6).show(ui, |ui| {
                ui.set_min_height(cell_h * 6.0);
                ui.add(
                    egui::Label::new(egui::RichText::new(s.rows.join("\n").trim_end()).font(font.clone()))
                        .wrap_mode(egui::TextWrapMode::Extend),
                );
            });
            if !s.alive {
                hint(ui, "The login has finished. Check again to see if it worked.");
            } else {
                hint(ui, "Type here if the app asks for a code. Open the link it prints in a browser.");
            }
        });
    }
    if cli.logged_in {
        ui.add_space(8.0);
        let label = format!("use {}", kind.label());
        let key = Key::new("use this login").solid(READOUT).min_width(160.0);
        if act_key(ui, key, &label).clicked() {
            *act = Some(Act::Save(Provider { kind, ..Default::default() }));
        }
    }
}

fn endpoint(
    ui: &mut Ui,
    ob: &mut Onboard,
    kind: ProviderKind,
    busy: &[ProviderKind],
    act: &mut Option<Act>,
) {
    let check = ob.checks.get(&kind).cloned();
    let Some(p) = ob.edit.as_mut() else {
        return;
    };
    let what = match kind {
        ProviderKind::AnthropicKey => "stored on the agentbench server, never in a project file",
        ProviderKind::OpenaiKey => "stored on the agentbench server, never in a project file",
        _ => "the base URL, ending in /v1 for most servers",
    };
    section(ui, "connection", what, |ui| {
        if !matches!(kind, ProviderKind::AnthropicKey | ProviderKind::OpenaiKey) || !p.url.is_empty() {
            row_help(ui, "address", "The base URL of the API.", |ui| {
                field(ui, &mut p.url, kind.default_url());
            });
        }
        let explain = if p.has_key {
            "A key is saved. Leave this empty to keep it."
        } else {
            "Left empty for servers that need none."
        };
        row_help(ui, "api key", explain, |ui| {
            secret(ui, &mut p.key);
        });
        let ready = (!p.url.trim().is_empty() || !kind.default_url().is_empty())
            && (!kind.needs_key() || p.has_key || !p.key.trim().is_empty());
        let testing = busy.contains(&kind);
        ui.horizontal(|ui| {
            let label = if testing { "testing" } else { "test" };
            if act_key(ui, Key::new(label).solid(READOUT).min_width(100.0).enabled(ready && !testing), "test connection").clicked() {
                *act = Some(Act::Test(p.clone()));
            }
        });
        match &check {
            Some(Some(c)) => status(ui, c.ok, &c.message),
            Some(None) => hint(ui, "asking the server for one tiny reply"),
            None => hint(ui, "The test sends one short message and shows the server's own answer."),
        }
        let passed = matches!(&check, Some(Some(c)) if c.ok);
        if act_key(ui, Key::new("save").solid(if passed { OK } else { READOUT }).min_width(100.0).enabled(ready), "save provider").clicked() {
            *act = Some(Act::Save(p.clone()));
        }
        if !passed {
            hint(ui, "You can save before it passes. The health check on the first page will keep showing the error.");
        }
    });
}

fn level_rows(ui: &mut Ui, p: &mut Provider, list: Option<&Result<Vec<String>, String>>) {
    let kind = p.kind;
    for (name, value, help) in [
        ("low", &mut p.low, "cheap and quick, for mechanical work"),
        ("medium", &mut p.medium, "the usual"),
        ("high", &mut p.high, "slow and strong, for the hard parts"),
        ("supervisor", &mut p.supervisor, "the standing chat that runs your other agents"),
    ] {
        row_help(ui, name, help, |ui| {
            model_input(ui, &format!("ob-{}-{name}", kind.id()), value, "not set", list);
        });
    }
}

fn models(ui: &mut Ui, ob: &mut Onboard) {
    note(
        ui,
        "Pick a model for each level. Chats start on the medium one, and an agent can move up or down a level when the work needs it.",
        LEGEND,
    );
    ui.add_space(6.0);
    let kinds: Vec<ProviderKind> = ob.working.iter().map(|p| p.kind).collect();
    for kind in kinds {
        let list = ob.models.get(&kind).and_then(|l| l.clone());
        let Some(p) = ob.working.iter_mut().find(|p| p.kind == kind) else {
            continue;
        };
        section(ui, kind.label(), if p.default { "default" } else { "" }, |ui| {
            match (&list, ob.models.get(&kind)) {
                (Some(Err(e)), _) => status(ui, false, e),
                (None, Some(None)) => hint(ui, "asking for the model list"),
                (Some(Ok(l)), _) => status(ui, true, &format!("{} models available", l.len())),
                _ => {}
            }
            level_rows(ui, p, list.as_ref());
        });
        ui.add_space(6.0);
    }
}

fn finish(ui: &mut Ui, ob: &mut Onboard) {
    section(ui, "your providers", "checked just now", |ui| {
        for p in &ob.working {
            let health = ob.health.get(&p.kind);
            ui.horizontal(|ui| {
                health_chip(ui, health);
                Line::new().legend(p.kind.label()).show(ui);
                if p.default {
                    chip(ui, "default", READOUT, true);
                }
            });
            if let Some(h) = health.filter(|h| !h.ok) {
                note(ui, &h.message, FAULT);
            } else if !p.medium.is_empty() {
                hint(ui, &format!("chats start on {}", p.medium));
            }
        }
        if ob.working.is_empty() {
            hint(ui, "Nothing is set up. You can come back from settings.");
        }
    });
    ui.add_space(8.0);
    section(ui, "first project", "optional, a folder with a git repo", |ui| {
        row_help(ui, "folder", "Any folder on the server. Leave it empty to add one later.", |ui| {
            field(ui, &mut ob.project, "~/git/my-project");
        });
    });
}
