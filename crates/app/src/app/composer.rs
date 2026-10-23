//! The composer under a chat: its text box, slash menu, attachments, speech key and status line.

use super::*;

impl App {
    pub(super) fn slash_menu(&mut self, ui: &mut Ui, field: egui::Rect) {
        let typed = self.draft.trim_start();
        if !typed.starts_with('/') || typed.contains(char::is_whitespace) {
            return;
        }
        let typed = typed.trim_start_matches('/').to_string();
        let matches: Vec<&foreman_core::Slash> =
            foreman_core::SLASH.iter().filter(|c| c.name.starts_with(&typed)).collect();
        if matches.is_empty() {
            return;
        }
        let height = matches.len() as f32 * 24.0 + 10.0;
        let at = egui::pos2(field.left(), field.top() - height - 4.0);
        let mut picked = None;
        egui::Area::new(ui.id().with("slash-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(at)
            .show(ui.ctx(), |ui| {
                egui::Frame::NONE
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0, ETCH))
                    .corner_radius(2)
                    .inner_margin(Margin::symmetric(8, 5))
                    .show(ui, |ui| {
                        ui.set_width(field.width() - 16.0);
                        for c in matches {
                            let r = ui
                                .horizontal(|ui| {
                                    ui.set_min_height(20.0);
                                    Line::new()
                                        .set(format!("/{}", c.name))
                                        .size(12.0)
                                        .gap(6.0)
                                        .value(c.args)
                                        .size(11.0)
                                        .tint(LEGEND)
                                        .gap(12.0)
                                        .note(c.about)
                                        .size(11.0)
                                        .elided(ui);
                                })
                                .response
                                .interact(Sense::click());
                            if r.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            if r.clicked() {
                                picked = Some(c);
                            }
                        }
                    });
            });
        if let Some(c) = picked {
            self.draft = format!("/{}{}", c.name, if c.args.is_empty() { "" } else { " " });
        }
    }

    /// Files dropped on the window go to the composer when it is on screen.
    pub(super) fn take_dropped(&mut self, ctx: &egui::Context) {
        if self.tab != Tab::Chat || self.settings.is_some() {
            return;
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            let name = f
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| f.name.clone());
            let tx = self.tray.sender();
            match (f.bytes, f.path) {
                (Some(bytes), _) => {
                    crate::attach::arrive(tx, ctx.clone(), name, bytes.to_vec());
                }
                #[cfg(not(target_arch = "wasm32"))]
                (None, Some(path)) => {
                    let ctx = ctx.clone();
                    crate::spawn_blocking(move || match std::fs::read(&path) {
                        Ok(data) => crate::attach::arrive(tx, ctx, name, data),
                        Err(e) => {
                            let _ = tx.send(crate::attach::Arrived::Failed(format!("{name}: {e}")));
                        }
                    });
                }
                _ => {}
            }
        }
    }

    pub(super) fn attachments_row(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut drop_at = None;
        ui.horizontal_wrapped(|ui| {
            for (i, f) in self.tray.files.iter().enumerate() {
                let chip = ui.horizontal(|ui| {
                    if let Some(tex) = &f.preview {
                        let s = tex.size_vec2();
                        let h = 28.0;
                        ui.add(egui::Image::new((tex.id(), Vec2::new(s.x * h / s.y.max(1.0), h))));
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "{} {}",
                            f.name,
                            crate::attach::size(f.size as u64)
                        ))
                        .font(mono(11.0))
                        .color(VALUE),
                    );
                    let x = ui.small_button("x");
                    x.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            format!("remove {}", f.name),
                        )
                    });
                    x.clicked()
                });
                if chip.inner {
                    drop_at = Some(i);
                }
                ui.add_space(8.0);
            }
        });
        if let Some(i) = drop_at {
            self.tray.files.remove(i);
        }
        let _ = ctx;
    }

    /// Ctrl+V with a picture on the clipboard: egui only passes text pastes on, so the key is
    /// watched here and the clipboard read for an image off the UI thread.
    pub fn watch_paste(&mut self, ctx: &egui::Context, raw: &egui::RawInput) {
        if self.tab != Tab::Chat || self.settings.is_some() {
            return;
        }
        let pasted = raw.events.iter().any(|e| match e {
            egui::Event::Key { key: egui::Key::V, pressed: false, modifiers, .. } => {
                modifiers.command
            }
            egui::Event::Key { key: egui::Key::Paste, pressed: false, .. } => true,
            _ => false,
        });
        if pasted {
            crate::attach::paste(self.tray.sender(), ctx.clone());
        }
    }

    pub(super) fn composer(
        &mut self,
        ui: &mut Ui,
        ready: bool,
        busy: bool,
        hint_text: &str,
        session: Option<&Session>,
    ) -> Option<(String, Vec<foreman_core::Upload>)> {
        // Files go to the focused tile only: the tray is shared, and it is the tile you are using.
        let top = ui.cursor().min.y;
        let files = self.focused && !self.tray.files.is_empty();
        if files {
            self.attachments_row(ui);
            ui.add_space(4.0);
        }
        let compact = self.compact;
        let narrow = self.narrow;
        // While this tile's speech is being listened to or read, the words are written into the
        // draft for it, so typing or sending now would be overwritten or cut the take short.
        let speaking =
            self.voice.busy() && self.speaking_in.is_some() && self.speaking_in == self.drawing;
        let edit = ui
            .add_enabled_ui(!speaking, |ui| {
                prose_id(
                    ui,
                    ("composer", self.drawing),
                    &mut self.draft,
                    hint_text,
                    if compact { 2 } else { 3 },
                    // Long text scrolls in the box; the send key never leaves the window.
                    (ui.ctx().content_rect().height() * 0.3).clamp(60.0, 260.0),
                )
            })
            .inner;
        if self.refocus.is_some() && self.refocus == self.drawing {
            self.refocus = None;
            edit.request_focus();
        }
        self.slash_menu(ui, edit.rect);
        let enter = edit.has_focus()
            && ui.input(|i| {
                i.key_pressed(egui::Key::Enter) && (i.modifiers.command || !i.modifiers.shift)
            });
        if enter && self.draft.ends_with('\n') && !ui.input(|i| i.modifiers.command) {
            let trimmed = self.draft.trim_end_matches('\n').to_string();
            self.draft = trimmed;
        }
        let mut out = None;
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let sending = self.focused && !self.tray.files.is_empty();
                let can = ready && !speaking && (!self.draft.trim().is_empty() || sending);
                let label = if busy { "steer" } else { "send" };
                let width = if compact { 64.0 } else { 100.0 };
                let key = ui.add(Key::new(label).solid(READOUT).enabled(can).min_width(width));
                key.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, can, label));
                if (key.clicked() || (enter && can)) && can {
                    let files = if self.focused { self.tray.take() } else { Vec::new() };
                    out = Some((self.draft.trim().to_string(), files));
                    self.draft.clear();
                    self.follow = true;
                }
                if !compact || self.focused {
                    self.speak_key(ui);
                }
                if crate::attach::can_pick() && !compact {
                    let key = ui.add(Key::new("attach").outline(VALUE));
                    key.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "attach")
                    });
                    if key.clicked() {
                        crate::attach::pick(self.tray.sender(), ui.ctx().clone());
                    }
                }
                // Whatever room the keys leave holds the agent's status line.
                if let Some(session) = session {
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let (lean, short) = status_form(compact, narrow);
                        status_line(ui, session, lean, short);
                    });
                }
            });
        });
        let height = ui.cursor().min.y - top;
        let before = self.composer_h.insert(self.drawing, height);
        if before.is_none_or(|b| (b - height).abs() > 0.5) {
            // The transcript above gives up room by what this measured, so lay out again.
            ui.ctx().request_repaint();
        }
        out
    }

    /// The key that speaks into the composer: press to start the microphone, press again to stop
    /// and have what was said written into the draft.
    pub(super) fn speak_key(&mut self, ui: &mut Ui) {
        if cfg!(target_arch = "wasm32") {
            return;
        }
        let ctx = ui.ctx().clone();
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        let (listening, reading) = (self.voice.listening(), self.voice.reading());
        let label = if listening {
            format!("stop {:.0}s", self.voice.seconds())
        } else if reading {
            let w = self.voice.status.clone();
            if w.is_empty() { "reading".to_string() } else { w }
        } else {
            "speak".to_string()
        };
        let key = if listening {
            Key::new(&label).solid(FAULT)
        } else {
            Key::new(&label).outline(if reading { LEGEND } else { VALUE }).enabled(!reading)
        };
        let key = ui.add(key);
        key.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "speak"));
        if listening {
            // A meter beside the key, so a muted microphone shows before the words do not.
            let level = self.voice.level().sqrt().clamp(0.0, 1.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(36.0, 6.0), Sense::hover());
            ui.painter().rect_filled(rect, 2.0, WELL);
            let mut on = rect;
            on.set_width(rect.width() * level);
            ui.painter().rect_filled(on, 2.0, OK);
            key.clone().on_hover_text(format!("listening on {}", self.voice.device()));
        } else if !reading {
            key.clone()
                .on_hover_text("speak instead of typing (Ctrl+M); it stops when you go quiet");
        }
        if key.clicked() {
            if listening {
                self.voice.finish(&self.speech_now(), wake);
            } else if let Some(tile) = self.drawing {
                self.begin_speech(tile, wake);
            }
        }
        // Escape on a listening composer throws the take away instead of reading it.
        if listening && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.voice.cancel();
            self.drop_speech();
        }
    }
}

/// The agent's status line: the server and model in use, how full the context is and how
/// much of the input the prompt cache served. Whatever the agent has not said is left out.
/// How the status line is written: `(lean, short)`. A tile leaves out the server and the cache
/// but keeps the words; only a narrow window (a phone) also writes the tags as `m` and `c`.
pub(super) fn status_form(tile: bool, narrow: bool) -> (bool, bool) {
    (tile || narrow, narrow)
}

/// `lean` leaves out the server and the cache, for a tile or a small window; `short` also writes
/// the tags as `m` and `c`, for a phone, which has no room for the words.
pub(super) fn status_line(ui: &mut Ui, s: &Session, lean: bool, short: bool) {
    let u = &s.usage;
    let model = if !u.model.is_empty() { Some(u.model.as_str()) } else { s.model.as_deref() };
    let mut line = Line::new();
    let mut any = false;
    if !u.host.is_empty() && !lean {
        line = line.legend("server").gap(6.0).value(&u.host).size(11.0);
        any = true;
    }
    if let Some(m) = model {
        line = if any { line.gap(14.0) } else { line };
        let tag = if short { "m" } else { "model" };
        line = line.legend(tag).gap(if short { 4.0 } else { 6.0 }).set(m).size(11.0);
        any = true;
    }
    let fill = u.fill();
    if u.context > 0 {
        let tint = match fill {
            Some(f) if f >= 0.8 => FAULT,
            Some(f) if f >= 0.6 => WARN,
            _ => TRACE,
        };
        let size = match u.window {
            0 => tokens(u.context),
            w => format!("{} / {}", tokens(u.context), tokens(w)),
        };
        line = if any { line.gap(if short { 10.0 } else { 14.0 }) } else { line };
        if short {
            // Short: the percent when the window is known, else the token count.
            let short = fill.map_or_else(|| tokens(u.context), |f| format!("{:.0}%", f * 100.0));
            line = line.legend("c").gap(4.0).measured(short).tint(tint).size(11.0);
        } else {
            line = line.legend("context").gap(6.0).measured(size).tint(tint).size(11.0);
            if let Some(f) = fill {
                line = line.gap(6.0).measured(format!("{:.0}%", f * 100.0)).tint(tint).size(11.0);
            }
        }
        any = true;
    }
    if let Some(hit) = u.cache_hit().filter(|_| !lean) {
        line = if any { line.gap(14.0) } else { line };
        line = line.legend("cache").gap(6.0).measured(format!("{:.0}%", hit * 100.0)).size(11.0);
        any = true;
    }
    if !any {
        return;
    }
    let r = ui.scope(|ui| line.elided(ui)).inner;
    r.on_hover_text(format!(
        "{}{}{}",
        if u.host.is_empty() { String::new() } else { format!("server {}\n", u.host) },
        model.map(|m| format!("model {m}\n")).unwrap_or_default(),
        match (u.context, u.window) {
            (0, _) => "context not reported yet".to_string(),
            (c, 0) => format!("{c} tokens of context; the window is not known"),
            (c, w) => format!("{c} of {w} tokens of context"),
        }
    ));
}
