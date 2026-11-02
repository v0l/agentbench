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
        let no_projects = o.projects.iter().all(|p| p.builtin());
        let existing = o.supervisor.and_then(|id| o.sessions.iter().find(|s| s.id == id).cloned());
        match existing {
            Some(s) => self.session_row(ui, &s, "supervisor", ""),
            None => {
                let key =
                    ui.add(Key::new("supervisor").outline(VALUE).min_width(ui.available_width()));
                key.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "supervisor")
                });
                if key.clicked() {
                    self.open_supervisor();
                }
            }
        }
        ui.add_space(4.0);
        let Some(o) = &self.overview else {
            return;
        };
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
            .filter(|s| Some(s.id) != o.supervisor)
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
        let tree = self
            .overview
            .iter()
            .flat_map(|o| &o.worktrees)
            .find(|w| w.id == s.worktree_id)
            .cloned();
        let tiles = self.grid_now().slots();
        let dims = self.grid_now().dims();
        let board: Vec<Option<(SessionId, String)>> = (0..tiles)
            .map(|i| {
                let id = self.slots[i]?;
                Some((id, self.session_by_id(id).map(|s| s.title.clone()).unwrap_or_default()))
            })
            .collect();
        resp.context_menu(|ui| {
            let width = 190.0;
            if s.status.busy() && ui.add(Key::new("stop").outline(FAULT).min_width(width)).clicked()
            {
                if let Some(c) = &mut self.client {
                    c.interrupt(s.id);
                }
                ui.close();
            }
            if let Some(w) = tree.as_ref().filter(|w| !w.missing) {
                if ui.add(Key::new("new chat here").outline(READOUT).min_width(width)).clicked() {
                    self.new_chat(Some(w));
                    ui.close();
                }
                if ui.add(Key::new("open terminal").outline(VALUE).min_width(width)).clicked() {
                    self.terminal = Some(TerminalView::new(s.id));
                    ui.close();
                }
            }
            if let Some(issue) = s.issue_id
                && ui
                    .add(Key::new(format!("open issue #{issue}")).outline(VALUE).min_width(width))
                    .clicked()
            {
                self.project = Some(s.project_id);
                self.tab = Tab::Issues;
                self.detail = Some(issue);
                ui.close();
            }
            if tiles > 1 {
                if let Some(at) = tile_board(ui, dims, &board, s.id) {
                    self.open_in_tile(s.id, at);
                    ui.close();
                }
            }
            if ui.add(Key::new("rename chat").outline(VALUE).min_width(width)).clicked() {
                self.rename_chat = Some((s.id, s.title.clone()));
                ui.close();
            }
            if !self.is_home(s.project_id)
                && ui.add(Key::new("rename project").outline(VALUE).min_width(width)).clicked()
            {
                self.rename_project_from_chat(s.id);
                ui.close();
            }
            if let Some(w) = tree.as_ref()
                && ui.add(Key::new("copy worktree path").outline(VALUE).min_width(width)).clicked()
            {
                ui.ctx().copy_text(w.path.clone());
                ui.close();
            }
            if ui.add(Key::new("copy chat id").outline(VALUE).min_width(width)).clicked() {
                ui.ctx().copy_text(s.id.to_string());
                ui.close();
            }
            if ui.add(Key::new("close chat").outline(FAULT).min_width(width)).clicked() {
                self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
                ui.close();
            }
        });
    }
}

fn tile_board(
    ui: &mut Ui,
    (cols, rows): (usize, usize),
    tiles: &[Option<(SessionId, String)>],
    chat: SessionId,
) -> Option<usize> {
    let (cell, gap) = (22.0, 3.0);
    ui.add_space(2.0);
    ui.label(egui::RichText::new("open in tile").size(10.5).color(LEGEND));
    ui.add_space(2.0);
    let size = Vec2::new(cols as f32 * (cell + gap) - gap, rows as f32 * (cell + gap) - gap);
    let (row, _) = ui.allocate_exact_size(Vec2::new(190.0, size.y), Sense::hover());
    let rect = egui::Rect::from_center_size(row.center(), size);
    let mut picked = None;
    for i in 0..cols * rows {
        let at = rect.min
            + Vec2::new((i % cols) as f32 * (cell + gap), (i / cols) as f32 * (cell + gap));
        let cell_rect = egui::Rect::from_min_size(at, Vec2::splat(cell));
        let resp = ui.interact(cell_rect, ui.id().with(("tile-pick", i)), Sense::click());
        let here = tiles.get(i).and_then(|t| t.as_ref()).is_some_and(|(id, _)| *id == chat);
        let taken = tiles.get(i).is_some_and(|t| t.is_some());
        let tint = if here {
            READOUT
        } else if resp.hovered() {
            VALUE
        } else if taken {
            LEGEND
        } else {
            LEGEND.gamma_multiply(0.55)
        };
        let fill = if here {
            tint.gamma_multiply(0.25)
        } else if resp.hovered() {
            tint.gamma_multiply(0.15)
        } else {
            WELL
        };
        let p = ui.painter();
        p.rect_filled(cell_rect, 2.0, fill);
        p.rect_stroke(cell_rect, 2.0, Stroke::new(1.0, tint), egui::StrokeKind::Inside);
        p.text(
            cell_rect.center(),
            egui::Align2::CENTER_CENTER,
            (i + 1).to_string(),
            egui::FontId::monospace(11.0),
            if here || resp.hovered() { tint } else { VALUE.gamma_multiply(0.8) },
        );
        let tip = match tiles.get(i).and_then(|t| t.as_ref()) {
            Some((_, title)) if !here => format!("tile {}: replaces {title}", i + 1),
            Some(_) => format!("tile {}: this chat", i + 1),
            None => format!("tile {}: empty", i + 1),
        };
        let resp = resp.on_hover_text(tip);
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            picked = Some(i);
        }
    }
    ui.add_space(4.0);
    picked
}
