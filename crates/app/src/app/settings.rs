//! The settings dialog and its tabs: server and pairing, speech, appearance.

use super::*;

impl App {
    pub(super) fn open_settings(&mut self) {
        let (url, token) = self
            .client
            .as_ref()
            .filter(|_| self.remote)
            .map(|c| (c.server.url.clone(), c.server.token.clone()))
            .unwrap_or_default();
        let asking_agent = self.client.is_some() && !cfg!(target_os = "android");
        self.settings = Some(Settings {
            page: Page::General,
            speech: self.speech.clone(),
            place: if self.remote || self.local.is_none() { Where::Remote } else { Where::Local },
            url,
            token,
            external: self.external.clone(),
            thinking: self.thinking,
            alerts: self.alerts,
            scan: false,
            pair_token: self.client.as_ref().map(|c| c.server.token.clone()).unwrap_or_default(),
            pair_url: self.client.as_ref().map(|c| c.server.url.clone()).unwrap_or_default(),
            face: self.face.clone(),
            agent_error: None,
            asking_agent,
            model: foreman_core::ModelSettings {
                url: String::new(),
                model: String::new(),
                key: String::new(),
                has_key: false,
                steps: 0,
                compact_at: foreman_core::DEFAULT_COMPACT_AT,
                low: Default::default(),
                medium: Default::default(),
                high: Default::default(),
            },
            loaded: false,
            models: None,
            asking: false,
            title: foreman_core::TitleSettings {
                enabled: true,
                url: String::new(),
                model: String::new(),
                key: String::new(),
                has_key: false,
            },
            title_loaded: false,
            title_models: None,
            title_asking: false,
        });
        // The agent's settings are not shown on a phone, so they are not asked for.
        if !cfg!(target_os = "android")
            && let Some(c) = &mut self.client
        {
            c.model();
        }
    }

    /// Whether the server fields in the dialog name a different server from the one in use.
    pub(super) fn server_changed(&self, st: &Settings) -> bool {
        match st.place {
            Where::Local => self.remote || (self.client.is_none() && self.starting.is_none()),
            Where::Remote => {
                !self.remote
                    || self.client.as_ref().is_none_or(|c| {
                        c.server.url.trim() != st.url.trim()
                            || c.server.token.trim() != st.token.trim()
                    })
            }
        }
    }

    pub(super) fn settings_modal(&mut self, ctx: &egui::Context) {
        let Some(mut st) = self.settings.take() else {
            return;
        };
        let (mut save, mut close, mut probe, mut probe_title) = (false, false, false, false);
        let can_close = self.client.is_some() || self.starting.is_some();
        let has_local = self.local.is_some();
        let installed = self.installed.clone();
        let scanning = self.font_scan.is_some();
        let main_list = self.agent_models.get("foreman").cloned();
        let m = egui::Modal::new(egui::Id::new("settings"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(modal_width(ctx, 540.0));
                modal_title(ui, "Settings");
                // The agent and the chat titles are set up on the server's own machine; a phone
                // only needs to know where the server is and how it should look and sound.
                let pages = if cfg!(target_os = "android") {
                    vec![(Page::General, "general"), (Page::Speech, "voice")]
                } else {
                    vec![
                        (Page::General, "general"),
                        (Page::Speech, "speech"),
                        (Page::Agent, "agent"),
                        (Page::Titles, "titles"),
                    ]
                };
                tabs(ui, &mut st.page, &pages);
                // One height for every page, so the dialog does not jump as you move between them.
                let body_h = (ctx.content_rect().height() - 260.0).clamp(240.0, 520.0);
                egui::ScrollArea::vertical()
                    .min_scrolled_height(body_h)
                    .max_height(body_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                ui.set_width(ui.available_width() - 12.0);
                if st.page == Page::General {
                    server_section(ui, &mut st, has_local);
                    ui.add_space(14.0);
                    // A phone is what scans the code, so it has no use for showing one.
                    if !cfg!(target_os = "android") {
                        pair_section(ui, &mut st);
                        ui.add_space(14.0);
                    }
                    appearance_section(ui, &mut st.face, &installed, scanning);
                    ui.add_space(14.0);
                    section(ui, "chats", "kept on this device", |ui| {
                        if crate::notify::available() {
                            row_help(
                                ui,
                                "alerts",
                                "Tell me when an agent needs an answer, a chat fails or an agent finishes, even with the app closed. Keeps a quiet notification up while watching.",
                                |ui| {
                                    if toggle(ui, if st.alerts { "on" } else { "off" }, st.alerts).clicked() {
                                        st.alerts = !st.alerts;
                                    }
                                },
                            );
                        }
                        row_help(ui, "show thinking", "Show the model's reasoning in the transcript.", |ui| {
                            if toggle(ui, if st.thinking { "on" } else { "off" }, st.thinking).clicked() {
                                st.thinking = !st.thinking;
                            }
                        });
                    });
                } else if st.page == Page::Speech {
                    speech_section(ui, &mut st.speech);
                } else if !can_close && st.place == Where::Remote {
                    hint(ui, "Connect to a server to set up its agents.");
                } else if let Some(e) = &st.agent_error {
                    section(ui, "foreman agent", "set on the server", |ui| status(ui, false, e));
                } else if !st.loaded {
                    section(ui, "foreman agent", "set on the server", |ui| {
                        hint(ui, if st.asking_agent { "asking the server" } else { "connect to a server to set up its agents" });
                    });
                } else if st.page == Page::Agent {
                let main_list = main_list.as_ref();
                section(ui, "model server", "any OpenAI compatible endpoint", |ui| {
                    row_help(ui, "address", "The base URL, ending in /v1 for most servers.", |ui| {
                        let r = field(ui, &mut st.model.url, "http://localhost:11434/v1");
                        if r.changed() {
                            st.models = None;
                        }
                        if r.lost_focus() && st.models.is_none() && !st.asking && !st.model.url.trim().is_empty() {
                            probe = true;
                        }
                    });
                    let explain = if st.model.has_key {
                        "Sent as a bearer token. A key is saved; leave this empty to keep it."
                    } else {
                        "Sent as a bearer token. Leave empty if the server needs none."
                    };
                    row_help(ui, "key", explain, |ui| {
                        let r = secret(ui, &mut st.model.key);
                        if r.changed() {
                            st.models = None;
                        }
                        if r.lost_focus() && st.models.is_none() && !st.asking && !st.model.url.trim().is_empty() {
                            probe = true;
                        }
                    });
                    row(ui, "model", |ui| match &st.models {
                        Some(Ok(list)) if !list.is_empty() => {
                            let w = (ui.available_width() - 96.0).max(120.0);
                            ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
                                ModelPicker::new("settings", list).show(ui, &mut st.model.model);
                            });
                            probe = ui
                                .add(Key::new("refresh").outline(VALUE).enabled(!st.asking))
                                .clicked();
                        }
                        _ => {
                            field_then(ui, &mut st.model.model, "model name", 130.0, |ui| {
                                probe = ui
                                    .add(Key::new(if st.asking { "asking" } else { "list models" }).outline(VALUE).enabled(!st.asking && !st.model.url.trim().is_empty()))
                                    .clicked();
                            });
                        }
                    });
                    row_help(
                        ui,
                        "max steps",
                        "Model calls in one turn before the agent pauses and waits for you to say continue.",
                        |ui| {
                            ui.add(egui::DragValue::new(&mut st.model.steps).range(1..=100_000).speed(10));
                        },
                    );
                    row_help(
                        ui,
                        "compact at",
                        "Summarise a chat and start it afresh once its context is this full, in percent. 0 never does. Only the foreman agent needs this; the others manage their own.",
                        |ui| {
                            ui.add(egui::DragValue::new(&mut st.model.compact_at).range(0..=100).suffix("%"));
                        },
                    );
                    match &st.models {
                        Some(Err(e)) => status(ui, false, e),
                        Some(Ok(l)) => status(ui, true, &format!("{} models on the server", l.len())),
                        None => {}
                    }
                });
                ui.add_space(8.0);
                hint(ui, "Saved on the server and used by every new turn of the foreman agent. Other agents keep their own settings.");
                ui.add_space(14.0);
                section(
                    ui,
                    "model levels",
                    "what switch_model and /model low|medium|high move a chat to",
                    |ui| {
                        let own = st.model.url.trim().to_string();
                        let levels = [
                            ("low", &mut st.model.low, "cheap and quick, for mechanical work"),
                            ("medium", &mut st.model.medium, "the usual"),
                            ("high", &mut st.model.high, "slow and strong, for the hard parts"),
                        ];
                        for (name, level, about) in levels {
                            row_help(ui, name, about, |ui| {
                                let list = level.url.trim().is_empty().then_some(main_list).flatten();
                                model_input(ui, &format!("level-{name}"), &mut level.model, "not set", list);
                            });
                            row_help(
                                ui,
                                "  server",
                                "Empty uses the foreman agent's server and key. A server of its own never gets the foreman key.",
                                |ui| {
                                    field(ui, &mut level.url, &own);
                                },
                            );
                            if !level.url.trim().is_empty() {
                                let explain = if level.has_key {
                                    "A key is saved; leave this empty to keep it."
                                } else {
                                    "Only sent to this server. Leave empty if it needs none."
                                };
                                row_help(ui, "  key", explain, |ui| {
                                    secret(ui, &mut level.key);
                                });
                            }
                        }
                    },
                );
                ui.add_space(8.0);
                hint(ui, "The agent can call switch_model to go up a level for hard work and down for easy work. A level with no model is left out. In a chat, /model high does the same by hand, and /model with nothing goes back to the default.");
                } else if st.title_loaded {
                    section(
                        ui,
                        "chat titles",
                        "a small model names each chat once the agent has answered",
                        |ui| {
                            row_help(ui, "titles", "Turn this off to keep the first words of each chat as its title.", |ui| {
                                ui.checkbox(&mut st.title.enabled, "name chats with a model");
                            });
                            row_help(
                                ui,
                                "address",
                                "Empty uses the foreman agent's server above. Any OpenAI compatible endpoint works, so a small one beside the big one is ideal.",
                                |ui| {
                                    let r = field(ui, &mut st.title.url, "same as the foreman agent");
                                    if r.changed() {
                                        st.title_models = None;
                                    }
                                    if r.lost_focus() && st.title_models.is_none() && !st.title_asking {
                                        probe_title = true;
                                    }
                                },
                            );
                            let explain = if st.title.has_key {
                                "Sent as a bearer token. A key is saved; leave this empty to keep it."
                            } else {
                                "Only sent to the address above. Empty uses the foreman agent's key when the address is empty too."
                            };
                            row_help(ui, "key", explain, |ui| {
                                let r = secret(ui, &mut st.title.key);
                                if r.changed() {
                                    st.title_models = None;
                                }
                            });
                            row(ui, "model", |ui| match &st.title_models {
                                Some(Ok(list)) if !list.is_empty() => {
                                    let w = (ui.available_width() - 96.0).max(120.0);
                                    ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
                                        ModelPicker::new("settings-title", list).show(ui, &mut st.title.model);
                                    });
                                    probe_title = ui
                                        .add(Key::new("refresh").outline(VALUE).enabled(!st.title_asking))
                                        .clicked();
                                }
                                _ => {
                                    field_then(ui, &mut st.title.model, "same as the foreman agent", 130.0, |ui| {
                                        probe_title = ui
                                            .add(Key::new(if st.title_asking { "asking" } else { "list models" }).outline(VALUE).enabled(!st.title_asking))
                                            .clicked();
                                    });
                                }
                            });
                            match &st.title_models {
                                Some(Err(e)) => status(ui, false, e),
                                Some(Ok(l)) => status(ui, true, &format!("{} models on the server", l.len())),
                                None => {}
                            }
                        },
                    );
                    ui.add_space(8.0);
                    hint(ui, "/title in a chat asks for a new title, and /title with words sets it yourself.");
                } else {
                    hint(ui, "asking the server");
                }
                });
                footer(ui, |ui| {
                    let server_ok = st.place == Where::Local
                        || (!st.url.trim().is_empty() && !st.token.trim().is_empty());
                    let agent_ok = !st.loaded
                        || (!st.model.url.trim().is_empty() && !st.model.model.trim().is_empty());
                    save = ui
                        .add(Key::new("save").solid(READOUT).enabled(server_ok && agent_ok).min_width(100.0))
                        .clicked();
                    if can_close {
                        close = ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked();
                    }
                });
            });
        if m.should_close() && can_close {
            close = true;
        }
        save |= std::mem::take(&mut self.save_requested);
        if std::mem::take(&mut st.scan) {
            let ctx = ctx.clone();
            self.scanner = Some(crate::scan::Scanner::start(move || ctx.request_repaint()));
        }
        if probe {
            st.asking = true;
            if let Some(c) = &mut self.client {
                c.models(&st.model);
            }
        }
        if probe_title {
            st.title_asking = true;
            if let Some(c) = &mut self.client {
                c.title_models(&st.title);
            }
        }
        // Only this device uses it, so it is kept as it is typed rather than on save.
        self.thinking = st.thinking;
        if self.alerts != st.alerts {
            self.alerts = st.alerts;
            self.sync_alerts();
        }
        if self.external != st.external.trim() {
            self.external = st.external.trim().to_string();
            ctx.request_repaint();
        }
        if save {
            self.speech = st.speech.clone();
            if st.face != self.face {
                self.face = st.face.clone();
                self.load_fonts(ctx);
            }
            if st.loaded
                && let Some(c) = &mut self.client
            {
                if st.title_loaded {
                    c.set_title(&st.title);
                }
                c.set_model(&st.model);
            }
            if self.server_changed(&st) {
                let server =
                    Server { url: st.url.trim().to_string(), token: st.token.trim().to_string() };
                match st.place {
                    Where::Local => {
                        self.remote = false;
                        self.start_local(ctx);
                    }
                    Where::Remote => {
                        self.remote = true;
                        self.connect_to(ctx, server);
                    }
                }
                return;
            }
            if st.loaded {
                self.settings = Some(st);
            }
            return;
        }
        if !close {
            self.settings = Some(st);
        }
    }
}

pub(super) fn server_section(ui: &mut Ui, st: &mut Settings, has_local: bool) {
    section(ui, "server", "where foreman runs its agents", |ui| {
        if has_local {
            segments(
                ui,
                &mut st.place,
                &[(Where::Local, "this machine", None), (Where::Remote, "remote", None)],
            );
            ui.add_space(8.0);
        }
        match st.place {
            Where::Local => note(
                ui,
                "Foreman starts its server in the background on this machine and keeps it running after the window closes, so agents carry on working.",
                LEGEND,
            ),
            Where::Remote => {
                if crate::scan::available() {
                    let key = ui.add(Key::new("scan a code").solid(READOUT).min_width(ui.available_width()));
                    key.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "scan a code")
                    });
                    if key.clicked() {
                        st.scan = true;
                    }
                    hint(ui, "Scan the code in foreman's settings on your computer, under pair a phone.");
                    ui.add_space(6.0);
                }
                row_help(
                    ui,
                    "address",
                    "Where the server listens, through whatever tunnel or DNAT you put in front of it.",
                    |ui| {
                        let r = field(ui, &mut st.url, "https://foreman.example.com");
                        // A pairing link pasted here fills in the address and the token.
                        if r.changed()
                            && let Some(p) = foreman_core::pairing::Pairing::parse(&st.url)
                        {
                            st.url = p.url;
                            st.token = p.token;
                        }
                    },
                );
                row_help(ui, "token", "Printed by `foreman token` on that machine.", |ui| {
                    secret(ui, &mut st.token);
                });
            }
        }
    });
}

/// A code for a phone to scan, which holds the server's address and token.
pub(super) fn pair_section(ui: &mut Ui, st: &mut Settings) {
    section(ui, "pair a phone", "scan to connect", |ui| {
        row_help(
            ui,
            "external address",
            "Where a phone reaches this server from outside this machine: a public address, a tunnel, or the machine's address on your network. Kept on this device.",
            |ui| {
                field(ui, &mut st.external, "https://foreman.example.com");
            },
        );
        match pairing_of(st) {
            Some(p) => {
                ui.add_space(8.0);
                ui.vertical_centered(|ui| {
                    let side = 220.0_f32.min(ui.available_width());
                    ui.allocate_ui(Vec2::splat(side), |ui| {
                        // A white card with room round the code, as scanners need.
                        if let Ok(code) = egui_qr::QrCodeWidget::from_data(p.link().as_bytes()) {
                            ui.add(code.quiet_zone(3.0));
                        }
                    });
                });
                ui.add_space(6.0);
                hint(
                    ui,
                    "Scan it with the phone's camera and open it in foreman. The code holds the server's token, so keep it to yourself.",
                );
            }
            None if st.pair_token.is_empty() => {
                hint(ui, "Connect to a server first: the code carries its token.");
            }
            None => hint(ui, "Give the address a phone reaches this server at to get a code."),
        }
    });
}

/// What the pairing code says: the external address, or the connected server's when that one is
/// not just this machine, with the connected server's token.
pub(super) fn pairing_of(st: &Settings) -> Option<foreman_core::pairing::Pairing> {
    let given = st.external.trim();
    let url = if !given.is_empty() {
        given.to_string()
    } else if !is_loopback(&st.pair_url) {
        st.pair_url.trim().to_string()
    } else {
        return None;
    };
    (!st.pair_token.trim().is_empty() && !url.is_empty())
        .then(|| foreman_core::pairing::Pairing { url, token: st.pair_token.trim().to_string() })
}

pub(super) fn is_loopback(url: &str) -> bool {
    let host = foreman_core::host_of(url);
    let host = host.rsplit_once(':').map_or(host.as_str(), |(h, _)| h);
    host.is_empty() || matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1" | "0.0.0.0")
}

pub(super) fn speech_section(ui: &mut Ui, sp: &mut crate::voice::Settings) {
    use crate::voice::{LOCAL_MODELS, Mode, Voice};
    section(ui, "voice input", "kept on this device", |ui| {
        let mut options =
            vec![(Mode::Foreman, "foreman server", None), (Mode::Server, "other server", None)];
        if cfg!(local_speech) {
            options.insert(0, (Mode::Local, "this device", None));
        }
        segments(ui, &mut sp.mode, &options);
        ui.add_space(8.0);
        match sp.mode {
            Mode::Local => {
                row_help(
                    ui,
                    "model",
                    "Whisper, run by foreman on this device. Downloaded the first time you speak.",
                    |ui| {
                        choice_fit(
                            ui,
                            "speech-model",
                            &mut sp.model,
                            LOCAL_MODELS.iter().map(|m| {
                                let have = if Voice::installed(m.0) { ", downloaded" } else { "" };
                                (m.0.to_string(), format!("{}, {} MB{have}", m.2, m.3))
                            }),
                        );
                    },
                );
            }
            Mode::Foreman => {
                row_help(
                    ui,
                    "model",
                    "The model the foreman server reads with. It downloads it the first time, and runs it on its GPU when it was built with CUDA.",
                    |ui| {
                        choice_fit(
                            ui,
                            "speech-model-server",
                            &mut sp.model,
                            LOCAL_MODELS
                                .iter()
                                .map(|m| (m.0.to_string(), format!("{}, {} MB", m.2, m.3))),
                        );
                    },
                );
                hint(
                    ui,
                    "Speech is sent to the foreman server you are connected to, with the same token. Nothing leaves your network unless that server does.",
                );
            }
            Mode::Server => {
                row_help(
                    ui,
                    "address",
                    "Any server with an OpenAI style /v1/audio/transcriptions: OpenAI, Groq, faster-whisper-server, whisper.cpp.",
                    |ui| {
                        field(ui, &mut sp.url, "http://localhost:8000/v1");
                    },
                );
                row_help(
                    ui,
                    "key",
                    "Sent as a bearer token. Leave empty if the server needs none.",
                    |ui| {
                        secret(ui, &mut sp.key);
                    },
                );
                row_help(ui, "model", "Sent as the model name. Empty sends whisper-1.", |ui| {
                    field(ui, &mut sp.server_model, "whisper-1");
                });
            }
        }
        if let Some(p) = sp.problem() {
            status(ui, false, p);
        }
    });
    ui.add_space(8.0);
    hint(
        ui,
        "The speak key beside send records from the default microphone until you press it again, then adds what you said to the message. Nothing is sent until you send it.",
    );
}

/// `choice`, but a long label is cut to the width of the box instead of widening it past the dialog.
pub(super) fn choice_fit(
    ui: &mut Ui,
    id: &str,
    picked: &mut String,
    options: impl IntoIterator<Item = (String, String)>,
) {
    let options: Vec<(String, String)> = options.into_iter().collect();
    let shown = options.iter().find(|(v, _)| v == picked).map(|(_, l)| l.clone());
    let width = ui.available_width();
    egui::ComboBox::from_id_salt(id)
        .selected_text(shown.unwrap_or_default())
        .width(width)
        .truncate()
        .show_ui(ui, |ui| {
            ui.set_max_width(width);
            for (v, label) in options {
                let on = *picked == v;
                if ui.add(egui::Button::selectable(on, label).truncate()).clicked() {
                    *picked = v;
                }
            }
        });
}

pub(super) fn appearance_section(ui: &mut Ui, face: &mut Typeface, installed: &Installed, scanning: bool) {
    section(ui, "appearance", "kept on this device", |ui| {
        if installed.can_choose() {
            row_help(
                ui,
                "font",
                "Any font installed here. Empty uses the system's sans-serif.",
                |ui| {
                    font_input(ui, "font-family", &mut face.family, &installed.all);
                },
            );
            row_help(
                ui,
                "code font",
                "Used for code, tool output and figures. Empty uses the system's monospace.",
                |ui| {
                    font_input(ui, "font-mono", &mut face.mono, &installed.mono);
                },
            );
        } else if scanning {
            hint(ui, "looking for installed fonts");
        } else if !cfg!(target_os = "android") {
            hint(ui, "This platform does not list its fonts, so the text keeps the built in ones.");
        }
        row_help(ui, "text size", "Scales every piece of text in the window.", |ui| {
            ui.add(
                egui::DragValue::new(&mut face.size)
                    .range(fonts::MIN..=fonts::MAX)
                    .speed(0.1)
                    .fixed_decimals(1)
                    .suffix(" pt"),
            );
        });
    });
}

pub(super) fn font_input(ui: &mut Ui, id: &str, value: &mut String, list: &[String]) {
    let w = ui.available_width();
    ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
        ModelPicker::new(id, list).noun("font").allow_empty("system default").show(ui, value);
    });
}

/// The model input: a dropdown over the server's list when there is one, else a plain field.
pub(super) fn model_input(
    ui: &mut Ui,
    id: &str,
    value: &mut String,
    hint_text: &str,
    list: Option<&Result<Vec<String>, String>>,
) {
    match list {
        Some(Ok(l)) if !l.is_empty() => {
            let w = ui.available_width();
            ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
                ModelPicker::new(id, l).allow_empty(hint_text).show(ui, value);
            });
        }
        _ => {
            field(ui, value, hint_text);
        }
    }
}
