//! The project list: projects, their worktrees and chats, with the rows' menus.

use super::*;

const ICON: f32 = 24.0;

impl App {
    pub(super) fn side(&mut self, ui: &mut Ui) {
        let Some(o) = &self.overview else {
            match &self.offline {
                Some((e, _)) => {
                    status(ui, false, e);
                    hint(ui, "retrying every few seconds");
                }
                None if self.starting.is_some() => {
                    note(ui, "starting the agentbench server", LEGEND)
                }
                None => note(ui, "connecting", LEGEND),
            }
            return;
        };
        let no_projects = o.projects.iter().all(|p| p.home);
        field(ui, &mut self.side_filter, "filter chats");
        ui.add_space(4.0);
        let needle = self.side_filter.trim().to_lowercase();
        let room = (ui.available_height() - 40.0).max(60.0);
        egui::ScrollArea::vertical()
            .id_salt("chats")
            .max_height(room)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                self.chat_stack(ui, &needle);
            });
        if no_projects {
            note(ui, "Add a git repository to start chatting with an agent in it.", LEGEND);
        }
        ui.add_space(6.0);
        let add = ui.add(Key::new("add project").outline(VALUE).min_width(ui.available_width()));
        add.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "add project")
        });
        if add.clicked() {
            self.add_project =
                Some(AddProject { path: String::new(), name: String::new(), create: false });
        }
    }

    fn chat_stack(&mut self, ui: &mut Ui, needle: &str) {
        let Some(o) = &self.overview else {
            return;
        };
        let mut chats: Vec<(Session, String, String)> = o
            .sessions
            .iter()
            .map(|s| {
                let project =
                    o.projects.iter().find(|p| p.id == s.project_id).map(|p| p.name.clone());
                let branch = o.worktrees.iter().find(|w| w.id == s.worktree_id).map(|w| w.label());
                (s.clone(), project.unwrap_or_default(), branch.unwrap_or_default())
            })
            .filter(|(s, project, branch)| {
                needle.is_empty()
                    || s.title.to_lowercase().contains(needle)
                    || project.to_lowercase().contains(needle)
                    || branch.to_lowercase().contains(needle)
            })
            .collect();
        chats.sort_by_key(|(s, _, _)| chat_key(s));
        if chats.is_empty() {
            hint(ui, if needle.is_empty() { "no chats yet" } else { "no chats match" });
            return;
        }
        for (s, project, branch) in &chats {
            self.session_row(ui, s, project, branch);
        }
    }

    fn project_badge(&mut self, ui: &mut Ui, rect: egui::Rect, project: Id, name: &str) {
        if let Some(Icon::Ready(tex)) = self.project_icon(project) {
            let tex = tex.clone();
            egui::Image::new((tex.id(), rect.size())).corner_radius(4.0).paint_at(ui, rect);
            return;
        }
        let hue = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 360;
        let fill = egui::ecolor::Hsva::new(hue as f32 / 360.0, 0.45, 0.55, 1.0);
        ui.painter().rect_filled(rect, 4.0, egui::Color32::from(fill));
        let letter = name.chars().find(|c| c.is_alphanumeric()).unwrap_or('~');
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            letter.to_uppercase().to_string(),
            egui::FontId::proportional(12.0),
            egui::Color32::WHITE,
        );
    }

    pub(super) fn session_row(&mut self, ui: &mut Ui, s: &Session, project: &str, branch: &str) {
        let on = self.session == Some(s.id);
        let (tint, word) = status_look(s.status);
        let (row, resp) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("chat {}", s.title))
        });
        let tiled =
            self.grid_now().slots() > 1 && self.tab == Tab::Chat && self.shown().contains(&s.id);
        let fill = if on {
            Some(READOUT.gamma_multiply(0.14))
        } else if tiled {
            Some(LEGEND.gamma_multiply(0.10))
        } else if resp.hovered() {
            Some(LEGEND.gamma_multiply(0.06))
        } else {
            None
        };
        if let Some(fill) = fill {
            ui.painter().rect_filled(row, 3.0, fill);
        }
        let badge = egui::Rect::from_min_size(
            egui::pos2(row.left() + 6.0, row.top() + 7.0),
            Vec2::splat(ICON),
        );
        self.project_badge(ui, badge, s.project_id, project);
        let dot = badge.right_bottom() - Vec2::splat(1.0);
        ui.painter().circle_filled(dot, 4.5, CHASSIS);
        if s.status == SessionStatus::Running {
            spinner(ui, dot, 4.0, tint);
        } else {
            ui.painter().circle_filled(dot, 3.0, tint);
        }
        let mut inner = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_min_max(
                    egui::pos2(row.left() + 6.0 + ICON + 8.0, row.top() + 6.0),
                    egui::pos2(row.right() - 8.0, row.bottom() - 4.0),
                ))
                .layout(Layout::top_down(Align::Min)),
        );
        inner.spacing_mut().item_spacing.y = 0.0;
        Line::new()
            .value(&s.title)
            .tint(if on { READOUT } else { VALUE })
            .size(11.5)
            .elided(&mut inner);
        let place = if branch.is_empty() || branch == "main" {
            project.to_string()
        } else {
            format!("{project} / {branch}")
        };
        let when = if s.status.busy() { word.to_string() } else { ago(s.updated_at) };
        Line::new().note(format!("{place}  {when}")).size(10.0).elided(&mut inner);
        if resp.clicked() {
            self.open_session(s.id, s.project_id);
        }
        resp.context_menu(|ui| {
            if s.status.busy() && ui.add(Key::new("stop").outline(FAULT).min_width(150.0)).clicked()
            {
                if let Some(c) = &mut self.client {
                    c.interrupt(s.id);
                }
                ui.close();
            }
            if !self.is_home(s.project_id)
                && ui.add(Key::new("rename project").outline(VALUE).min_width(150.0)).clicked()
            {
                self.rename_project_from_chat(s.id);
                ui.close();
            }
            if ui.add(Key::new("close chat").outline(FAULT).min_width(150.0)).clicked() {
                self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
                ui.close();
            }
        });
    }
}
