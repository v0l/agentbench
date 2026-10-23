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
                None if self.starting.is_some() => note(ui, "starting the foreman server", LEGEND),
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
        for p in &projects {
            let selected = self.project == Some(p.id);
            let rail = if selected { Some(READOUT) } else { None };
            let trees: Vec<&Worktree> = worktrees.iter().filter(|w| w.project_id == p.id).collect();
            let home = p.home;
            let open = self.overview.as_ref().map_or(0, |o| {
                o.issues.iter().filter(|i| i.project_id == p.id && i.state.is_open()).count()
            });
            let collapsed = self.collapsed.contains(&p.id);
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
                        let count = if home { "no repo".to_string() } else { format!("{open} open") };
                        Line::new().note(count).size(10.5).show(ui);
                    });
                },
                |ui| {
                    let shown = if home { "~ chats anywhere".to_string() } else { short_path(&p.path) };
                    let r = Line::new().note(shown).size(10.5).elided(ui);
                    if r.interact(Sense::click()).clicked() {
                        pick = true;
                    }
                    if collapsed {
                        return;
                    }
                    ui.add_space(2.0);
                    let last = trees.len().saturating_sub(1);
                    for (i, w) in trees.iter().enumerate() {
                        self.worktree_row(ui, w, i == last);
                    }
                },
            );
            if pick || picked_head {
                self.project = Some(p.id);
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
                    if collapsed {
                        self.collapsed.remove(&p.id);
                    } else {
                        self.collapsed.insert(p.id);
                    }
                    ui.close();
                }
                if home {
                    return;
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
        add.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "add project"));
        if add.clicked() {
            self.add_project =
                Some(AddProject { path: String::new(), name: String::new(), create: false });
        }
    }

    pub(super) fn worktree_row(&mut self, ui: &mut Ui, w: &Worktree, last: bool) {
        let home = self.is_home(w.project_id);
        let sessions: Vec<Session> = self.sessions_of(w.id).into_iter().cloned().collect();
        let running = sessions.iter().any(|s| s.status.busy());
        let tint = if w.missing {
            FAULT
        } else if running {
            TRACE
        } else if w.main || !sessions.is_empty() {
            VALUE
        } else {
            LEGEND
        };
        let top = ui.cursor().top();
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(TREE_INDENT);
                let label = if home { "new chat in ~".to_string() } else { w.label() };
                let r = Line::new().value(label).tint(tint).size(11.5).show(ui);
                if w.main && !home {
                    chip(ui, "main", LEGEND, false);
                }
                if w.missing {
                    chip(ui, "gone", FAULT, true);
                }
                r
            })
            .response
            .interact(Sense::click());
        let row = resp.rect;
        for (i, s) in sessions.iter().enumerate() {
            self.session_row(ui, s, i + 1 == sessions.len());
        }
        let bottom = ui.min_rect().bottom();
        let x = row.left() + TREE_X;
        let p = ui.painter();
        let stroke = Stroke::new(1.0, ETCH);
        let elbow = row.center().y;
        p.vline(x, top..=if last { elbow } else { bottom + ui.spacing().item_spacing.y }, stroke);
        p.hline(x..=row.left() + TREE_INDENT - 4.0, elbow, stroke);
        if resp.clicked() && !w.missing {
            self.new_chat(Some(w));
        }
        resp.on_hover_text(&w.path).context_menu(|ui| {
            if ui
                .add(
                    Key::new("new chat here").outline(READOUT).min_width(170.0).enabled(!w.missing),
                )
                .clicked()
            {
                self.new_chat(Some(w));
                ui.close();
            }
            if !home
                && ui
                    .add(Key::new("start in a new worktree").outline(VALUE).min_width(170.0))
                    .clicked()
            {
                self.open_start(None);
                if let Some(s) = &mut self.start {
                    s.project = w.project_id;
                }
                ui.close();
            }
            if !w.main {
                let label = if w.missing { "forget worktree" } else { "remove worktree" };
                if ui.add(Key::new(label).outline(FAULT).min_width(170.0)).clicked() {
                    self.confirm = Some(Confirm::Worktree(w.id, w.label(), w.missing));
                    ui.close();
                }
            }
        });
    }

    pub(super) fn session_row(&mut self, ui: &mut Ui, s: &Session, last: bool) {
        let on = self.session == Some(s.id);
        let (tint, word) = status_look(s.status);
        let top = ui.cursor().top();
        let mut close = false;
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(TREE_INDENT * 2.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 16.0), Sense::hover());
                if s.status == SessionStatus::Running {
                    spinner(ui, rect.center(), 4.5, tint);
                } else {
                    ui.painter().circle_filled(rect.center(), 3.0, tint);
                }
                let hovered = ui.rect_contains_pointer(ui.max_rect());
                let reserve = if hovered { 18.0 } else { 0.0 };
                ui.allocate_ui(Vec2::new(ui.available_width() - reserve, 16.0), |ui| {
                    Line::new()
                        .value(&s.title)
                        .tint(if on { READOUT } else { VALUE })
                        .size(11.5)
                        .gap(6.0)
                        .note(format!("{} {}", s.agent, word))
                        .size(10.5)
                        .elided(ui)
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
        if close {
            self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
        }
        let row = resp.rect;
        let x = row.left() + TREE_INDENT + TREE_X;
        let stroke = Stroke::new(1.0, ETCH);
        let elbow = row.center().y;
        let reach = if last { elbow } else { row.bottom() + ui.spacing().item_spacing.y };
        ui.painter().vline(x, top - ui.spacing().item_spacing.y..=reach, stroke);
        ui.painter().hline(x..=row.left() + TREE_INDENT * 2.0 - 2.0, elbow, stroke);
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
