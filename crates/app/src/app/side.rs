//! The project list: projects, their worktrees and chats, with the rows' menus.

use super::*;

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
        let projects = o.projects.clone();
        let worktrees = o.worktrees.clone();
        if projects.is_empty() {
            section(ui, "no projects yet", "", |ui| {
                note(
                    ui,
                    "Add a git repository. Its main checkout is the project, and every worktree off it is a branch your agents can work in.",
                    LEGEND,
                );
            });
        }
        field(ui, &mut self.side_filter, "filter projects and chats");
        ui.add_space(4.0);
        let needle = self.side_filter.trim().to_lowercase();
        let filtering = !needle.is_empty();
        self.chat_stack(ui, &needle);
        ui.add_space(6.0);
        let label =
            format!("{} projects ({})", if self.projects_open { "v" } else { ">" }, projects.len());
        if Line::new().legend(&label).show(ui).interact(Sense::click()).clicked() {
            self.projects_open = !self.projects_open;
        }
        ui.add_space(4.0);
        let listed = self.projects_open || projects.is_empty();
        for p in projects.iter().filter(|_| listed) {
            if filtering && !self.project_matches(p, &worktrees, &needle) {
                continue;
            }
            let selected = self.project == Some(p.id);
            let rail = if selected { Some(READOUT) } else { None };
            let trees: Vec<&Worktree> = worktrees.iter().filter(|w| w.project_id == p.id).collect();
            let home = p.home;
            let trees_menu: Vec<Worktree> = trees.iter().map(|w| (*w).clone()).collect();
            let open = self.overview.as_ref().map_or(0, |o| {
                o.issues.iter().filter(|i| i.project_id == p.id && i.state.is_open()).count()
            });
            let busy = self.busy_in(p.id);
            let collapsed = false;
            let mut picked_head = false;
            let mut pick = false;
            let mut menu = None;
            card(
                ui,
                rail,
                |ui| {
                    let r = Line::new().legend(&p.name).show(ui).interact(Sense::click());
                    picked_head = r.clicked();
                    menu = Some(r);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let count =
                            if home { "no repo".to_string() } else { format!("{open} open") };
                        Line::new().note(count).size(10.5).show(ui);
                        if busy > 0 {
                            chip(ui, &format!("{busy} active"), TRACE, false);
                        }
                    });
                },
                |ui| {
                    let shown =
                        if home { "no repo, starts in ~".to_string() } else { short_path(&p.path) };
                    let r = Line::new().note(shown).size(10.5).elided(ui);
                    if r.interact(Sense::click()).clicked() {
                        pick = true;
                    }
                    if collapsed {
                        return;
                    }
                    ui.add_space(2.0);
                    self.project_chats(ui, p.id, &trees);
                },
            );
            if pick || picked_head {
                self.project = Some(p.id);
                if picked_head && !self.opened.remove(&p.id) {
                    self.opened.insert(p.id);
                }
            }
            let Some(r) = menu else {
                continue;
            };
            r.context_menu(|ui| {
                if ui
                    .add(
                        Key::new(if collapsed { "expand" } else { "collapse" })
                            .outline(VALUE)
                            .min_width(160.0),
                    )
                    .clicked()
                {
                    if self.opened.contains(&p.id) {
                        self.opened.remove(&p.id);
                    } else {
                        self.opened.insert(p.id);
                    }
                    ui.close();
                }
                if home {
                    return;
                }
                for w in trees_menu.iter().filter(|w| !w.main) {
                    let label = if w.missing { "forget" } else { "remove" };
                    let key = Key::new(format!("{label} worktree {}", w.label()))
                        .outline(FAULT)
                        .min_width(160.0);
                    if ui.add(key).clicked() {
                        self.confirm = Some(Confirm::Worktree(w.id, w.label(), w.missing));
                        ui.close();
                    }
                }
                if ui.add(Key::new("new worktree").outline(VALUE).min_width(160.0)).clicked() {
                    self.new_worktree = Some((p.id, String::new()));
                    ui.close();
                }
                if ui.add(Key::new("rescan worktrees").outline(VALUE).min_width(160.0)).clicked() {
                    if let Some(c) = &mut self.client {
                        c.sync_project(p.id);
                    }
                    ui.close();
                }
                if ui.add(Key::new("remove project").outline(FAULT).min_width(160.0)).clicked() {
                    self.confirm = Some(Confirm::Project(p.id, p.name.clone()));
                    ui.close();
                }
            });
            ui.add_space(8.0);
        }
        let add = ui.add(Key::new("add project").outline(VALUE).min_width(ui.available_width()));
        add.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "add project")
        });
        if add.clicked() {
            self.add_project =
                Some(AddProject { path: String::new(), name: String::new(), create: false });
        }
    }

    fn busy_in(&self, project: Id) -> usize {
        self.overview.as_ref().map_or(0, |o| {
            o.sessions.iter().filter(|s| s.project_id == project && s.status.busy()).count()
        })
    }

    fn project_matches(
        &self,
        p: &agentbench_core::Project,
        worktrees: &[Worktree],
        needle: &str,
    ) -> bool {
        let has = |text: &str| text.to_lowercase().contains(needle);
        if has(&p.name) || has(&p.path) {
            return true;
        }
        let Some(o) = &self.overview else {
            return false;
        };
        worktrees.iter().any(|w| w.project_id == p.id && has(&w.label()))
            || o.sessions.iter().any(|s| s.project_id == p.id && has(&s.title))
    }

    fn start_label(&self, project: Id) -> String {
        let found = self.overview.iter().flat_map(|o| &o.projects).find(|p| p.id == project);
        found.map_or(String::new(), |p| if p.home { "~".into() } else { p.name.clone() })
    }

    fn project_chats(&mut self, ui: &mut Ui, project: Id, trees: &[&Worktree]) {
        let main = trees.iter().find(|w| w.main).or(trees.first()).map(|w| (*w).clone());
        let Some(w) = main.filter(|w| !w.missing) else {
            return;
        };
        let label = if self.is_home(project) { "new chat in ~" } else { "new chat" };
        let r = Line::new().value(label).tint(READOUT).size(11.5).show(ui);
        let r = r.interact(Sense::click());
        let name = format!("start chat in {}", self.start_label(project));
        r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
        if r.on_hover_text(&w.path).clicked() {
            self.new_chat(Some(&w));
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
        chats.sort_by_key(|(s, _, _)| (!s.status.busy(), std::cmp::Reverse(s.updated_at)));
        if chats.is_empty() {
            hint(ui, if needle.is_empty() { "no chats yet" } else { "no chats match" });
            return;
        }
        let total = chats.len();
        let shown =
            if self.all_chats || !needle.is_empty() { total } else { total.min(SHOWN_CHATS) };
        for (s, project, branch) in chats.iter().take(shown) {
            self.session_row(ui, s, project, branch);
        }
        if total > SHOWN_CHATS && needle.is_empty() {
            let text = if self.all_chats {
                "show fewer".to_string()
            } else {
                format!("show {} more", total - shown)
            };
            if Line::new().note(text).size(10.5).show(ui).interact(Sense::click()).clicked() {
                self.all_chats = !self.all_chats;
            }
        }
    }

    pub(super) fn session_row(&mut self, ui: &mut Ui, s: &Session, project: &str, branch: &str) {
        let on = self.session == Some(s.id);
        let (tint, word) = status_look(s.status);
        let mut close = false;
        let resp = ui
            .horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 30.0), Sense::hover());
                let dot = egui::pos2(rect.center().x, rect.top() + 8.0);
                if s.status == SessionStatus::Running {
                    spinner(ui, dot, 4.5, tint);
                } else {
                    ui.painter().circle_filled(dot, 3.0, tint);
                }
                let hovered = ui.rect_contains_pointer(ui.max_rect());
                let reserve = if hovered { 18.0 } else { 0.0 };
                let size = Vec2::new(ui.available_width() - reserve, 30.0);
                ui.allocate_ui_with_layout(size, Layout::top_down(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    Line::new()
                        .value(&s.title)
                        .tint(if on { READOUT } else { VALUE })
                        .size(11.5)
                        .elided(ui);
                    let place = if branch.is_empty() || branch == "main" {
                        project.to_string()
                    } else {
                        format!("{project} / {branch}")
                    };
                    let when = if s.status.busy() { word.to_string() } else { ago(s.updated_at) };
                    Line::new().note(format!("{place}  {when}")).size(10.0).elided(ui);
                });
                if hovered {
                    let x = ui
                        .add(
                            egui::Label::new(egui::RichText::new("×").size(14.0).color(LEGEND))
                                .sense(Sense::click()),
                        )
                        .on_hover_text("close and delete this chat");
                    if x.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    close = x.clicked();
                }
            })
            .response
            .interact(Sense::click());
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, format!("chat {}", s.title))
        });
        if close {
            self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
        }
        let row = resp.rect;
        if self.grid_now().slots() > 1 && self.tab == Tab::Chat && self.shown().contains(&s.id) {
            let tint = if on { READOUT } else { LEGEND };
            ui.painter().rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(row.right() - 2.0, row.top() + 1.0),
                    egui::pos2(row.right(), row.bottom() - 1.0),
                ),
                0.0,
                tint,
            );
        }
        if resp.clicked() && !close {
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
            if ui.add(Key::new("close chat").outline(FAULT).min_width(150.0)).clicked() {
                self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
                ui.close();
            }
        });
    }
}
