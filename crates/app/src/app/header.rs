//! The top bar: title, tabs, action keys, badges, and the drawer that holds the project list on a narrow window.

use super::*;

impl App {
    pub(super) fn header(&mut self, ui: &mut Ui) {
        if self.narrow {
            ui.horizontal(|ui| {
                if hamburger(ui, self.drawer).clicked() {
                    self.drawer = !self.drawer;
                }
                let w = (ui.available_width() - 150.0).max(100.0);
                ui.allocate_ui(Vec2::new(w, 24.0), |ui| self.header_tabs(ui, w));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.phone_more(ui);
                    self.key_new_chat_short(ui);
                });
            });
            if self.link() != Link::Connected && self.remote {
                ui.horizontal(|ui| self.link_chip(ui));
            }
        } else {
            ui.horizontal(|ui| {
                self.header_title(ui);
                ui.add_space(18.0);
                let w = (ui.available_width() - 640.0 - 120.0).clamp(200.0, 560.0);
                ui.allocate_ui(Vec2::new(w, 24.0), |ui| self.header_tabs(ui, w));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.key_new_chat(ui);
                    self.key_file_issue(ui);
                    if let Some(g) = grid_picker(ui, self.grid) {
                        self.set_grid(g);
                    }
                    self.key_settings(ui);
                    self.header_badges(ui);
                });
            });
        }
    }

    /// The project list on a narrow window: a drawer from the left edge, opened by the hamburger
    /// and put away by picking something, tapping outside it, or the hamburger again.
    pub(super) fn drawer_panel(&mut self, ctx: &egui::Context) {
        if !self.drawer {
            return;
        }
        let screen = ctx.content_rect();
        let top = 0.0;
        let before = (self.project, self.session, self.tab);
        let width = (screen.width() * 0.86).min(420.0);
        let id = egui::Id::unique("drawer");
        let m = egui::Modal::new(id)
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .area(egui::Modal::default_area(id).anchor(egui::Align2::LEFT_TOP, Vec2::new(0.0, top)))
            .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(8))
            .show(ctx, |ui| {
                ui.set_width(width - 16.0);
                ui.set_height(screen.height() - top - 16.0);
                // The fleet's status, which has no room in a phone's header.
                let row = Vec2::new(ui.available_width(), 22.0);
                ui.allocate_ui_with_layout(row, Layout::right_to_left(Align::Center), |ui| {
                    self.header_badges(ui);
                });
                ui.add_space(4.0);
                ui.spacing_mut().item_spacing.y = 4.0;
                self.side(ui);
            });
        let picked = before != (self.project, self.session, self.tab);
        if m.should_close() || picked {
            self.drawer = false;
        }
    }

    pub(super) fn header_title(&self, ui: &mut Ui) {
        ui.label(
            egui::RichText::new("AGENTBENCH")
                .font(legend_font(17.0))
                .color(VALUE)
                .extra_letter_spacing(2.5),
        );
    }

    pub(super) fn header_tabs(&mut self, ui: &mut Ui, _width: f32) {
        let open = self.overview.as_ref().map_or(0, |o| {
            o.issues
                .iter()
                .filter(|i| Some(i.project_id) == self.project && i.state.is_open())
                .count()
        });
        let issues_label = format!("issues {open}");
        let opts: Vec<(Tab, &str)> =
            vec![(Tab::Chat, "agents"), (Tab::Issues, issues_label.as_str())];
        if self.issues_first && self.tab == Tab::Issues {
            return;
        }
        tabs(ui, &mut self.tab, &opts);
    }

    pub(super) fn key_new_chat(&mut self, ui: &mut Ui) {
        let ready = self.overview.is_some();
        let key = ui.add(Key::new("new chat").solid(READOUT).enabled(ready));
        let key = key.on_hover_text("ctrl+n");
        if key.clicked() {
            self.open_start(None);
        }
    }

    pub(super) fn key_new_chat_short(&mut self, ui: &mut Ui) {
        let ready = self.overview.is_some();
        let key = ui.add(Key::new("new").solid(READOUT).enabled(ready).min_width(64.0));
        key.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ready, "new chat"));
        if key.clicked() {
            self.open_start(None);
        }
    }

    pub(super) fn phone_more(&mut self, ui: &mut Ui) {
        let key = ui.add(Key::new("...").outline(VALUE).min_width(36.0));
        key.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "more"));
        let can_file = self.overview.is_some() && self.project.is_some_and(|p| !self.is_home(p));
        egui::Popup::from_toggle_button_response(&key).show(|ui| {
            ui.set_min_width(190.0);
            ui.spacing_mut().item_spacing.y = 6.0;
            if let Some(o) = &self.overview {
                ui.horizontal(|ui| status_badge(ui, &o.sessions));
                if !self.remote {
                    chip(ui, "this machine", OK, true);
                }
            }
            if ui.add(Key::new("settings").outline(VALUE).min_width(190.0)).clicked() {
                self.open_settings();
                ui.close();
            }
            if ui
                .add(Key::new("file issue").outline(VALUE).enabled(can_file).min_width(190.0))
                .clicked()
                && let Some(p) = self.project
            {
                self.file_issue =
                    Some(FileIssue { project: p, title: String::new(), body: String::new() });
                ui.close();
            }
        });
    }

    pub(super) fn key_file_issue(&mut self, ui: &mut Ui) {
        let ready = self.overview.is_some();
        if ui
            .add(
                Key::new("file issue")
                    .outline(VALUE)
                    .enabled(ready && self.project.is_some_and(|p| !self.is_home(p))),
            )
            .clicked()
            && let Some(p) = self.project
        {
            self.file_issue =
                Some(FileIssue { project: p, title: String::new(), body: String::new() });
        }
    }

    pub(super) fn key_settings(&mut self, ui: &mut Ui) {
        let key = ui.add(Key::new("settings").outline(VALUE));
        key.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "settings"));
        if key.clicked() {
            self.open_settings();
        }
    }

    pub(super) fn link_chip(&mut self, ui: &mut Ui) {
        if self.client.is_none() && self.starting.is_none() && self.offline.is_none() {
            return;
        }
        let secs = |d: Duration| d.as_secs() + 1;
        let (text, tint) = match self.link() {
            Link::Connected => ("connected".to_string(), OK),
            Link::Reconnecting(d) if d.is_zero() => ("reconnecting".to_string(), WARN),
            Link::Reconnecting(d) => (format!("reconnecting in {}s", secs(d)), WARN),
            Link::Offline(d) => (format!("offline, retry in {}s", secs(d)), FAULT),
        };
        let r = chip(ui, &text, tint, true);
        let why = self.offline.as_ref().map(|(e, _)| e.clone()).unwrap_or_default();
        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Status, true, &text));
        if !why.is_empty() {
            r.on_hover_text(why);
        }
    }

    /// Where the server is, and how many agents are in each state.
    pub(super) fn header_badges(&mut self, ui: &mut Ui) {
        if !self.remote {
            chip(ui, "this machine", OK, self.overview.is_some());
        }
        if let Some(o) = &self.overview {
            status_badge(ui, &o.sessions);
            usage_badges(ui, o.usage.as_ref(), self.project);
        }
        if self.remote {
            self.link_chip(ui);
        }
    }
}
