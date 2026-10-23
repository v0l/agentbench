//! The top bar: title, tabs, action keys, badges, and the drawer that holds the project list on a narrow window.

use super::*;

impl App {
    pub(super) fn header(&mut self, ui: &mut Ui) {
        if self.narrow {
            // A phone is too narrow for the title, the tabs and every key on one line, so the
            // title and the two keys used most come first, then the tabs, then the rest.
            ui.horizontal(|ui| {
                if hamburger(ui, self.drawer).clicked() {
                    self.drawer = !self.drawer;
                }
                self.header_title(ui);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.key_new_chat(ui);
                    self.key_settings(ui);
                });
            });
            ui.add_space(4.0);
            let w = ui.available_width();
            ui.allocate_ui(Vec2::new(w, 24.0), |ui| self.header_tabs(ui, w));
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                self.key_file_issue(ui);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| self.header_badges(ui));
            });
        } else {
            ui.horizontal(|ui| {
                self.header_title(ui);
                ui.add_space(18.0);
                let w = (ui.available_width() - 640.0 - 120.0).clamp(200.0, 560.0);
                ui.allocate_ui(Vec2::new(w, 24.0), |ui| self.header_tabs(ui, w));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    self.key_new_chat(ui);
                    self.key_file_issue(ui);
                    for g in Grid::ALL.into_iter().rev() {
                        if grid_button(ui, g, self.grid == g).clicked() {
                            self.set_grid(g);
                        }
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
        let id = egui::Id::new("drawer");
        let m = egui::Modal::new(id)
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .area(
                egui::Modal::default_area(id)
                    .anchor(egui::Align2::LEFT_TOP, Vec2::new(0.0, top)),
            )
            .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(8))
            .show(ctx, |ui| {
                ui.set_width(width - 16.0);
                ui.set_height(screen.height() - top - 16.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    self.side(ui);
                });
            });
        let picked = before != (self.project, self.session, self.tab);
        if m.should_close() || picked {
            self.drawer = false;
        }
    }

    pub(super) fn header_title(&self, ui: &mut Ui) {
        ui.label(
            egui::RichText::new("FOREMAN")
                .font(legend_font(17.0))
                .color(VALUE)
                .extra_letter_spacing(2.5),
        );
    }

    pub(super) fn header_tabs(&mut self, ui: &mut Ui, _width: f32) {
        let open = self.overview.as_ref().map_or(0, |o| {
            o.issues.iter().filter(|i| Some(i.project_id) == self.project && i.state.is_open()).count()
        });
        let issues_label = format!("issues {open}");
        let opts: Vec<(Tab, &str)> =
            vec![(Tab::Chat, "agents"), (Tab::Issues, issues_label.as_str())];
        tabs(ui, &mut self.tab, &opts);
    }

    pub(super) fn key_new_chat(&mut self, ui: &mut Ui) {
        let ready = self.overview.is_some();
        if ui
            .add(Key::new("new chat").solid(READOUT).enabled(ready && self.project.is_some()))
            .clicked()
        {
            self.new_chat(None);
        }
    }

    pub(super) fn key_file_issue(&mut self, ui: &mut Ui) {
        let ready = self.overview.is_some();
        if ui
            .add(Key::new("file issue").outline(VALUE).enabled(ready && self.project.is_some()))
            .clicked()
            && let Some(p) = self.project
        {
            self.file_issue = Some(FileIssue { project: p, title: String::new(), body: String::new() });
        }
    }

    pub(super) fn key_settings(&mut self, ui: &mut Ui) {
        let key = ui.add(Key::new("settings").outline(VALUE));
        key.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "settings"));
        if key.clicked() {
            self.open_settings();
        }
    }

    /// Where the server is, and how many agents are in each state.
    pub(super) fn header_badges(&mut self, ui: &mut Ui) {
        if !self.remote {
            chip(ui, "this machine", OK, self.overview.is_some());
        }
        if let Some(o) = &self.overview {
            status_badge(ui, &o.sessions);
        }
    }
}
