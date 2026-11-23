use super::*;
use agentbench_core::{Activity, GoalStatus, Risky};

pub fn issue_counts(issues: &[Issue], project: Id) -> [usize; 5] {
    let mut out = [0; 5];
    for i in issues.iter().filter(|i| i.project_id == project) {
        if let Some(at) = IssueState::ALL.iter().position(|s| *s == i.state) {
            out[at] += 1;
        }
    }
    out
}

pub fn goal_counts(issues: &[Issue], project: Id, goal: &str) -> [usize; 5] {
    let mine: Vec<Issue> = issues
        .iter()
        .filter(|i| i.project_id == project && i.goal.as_deref() == Some(goal))
        .cloned()
        .collect();
    issue_counts(&mine, project)
}

pub fn last_activity(project: &Project, issues: &[Issue], sessions: &[Session]) -> i64 {
    let issue = issues.iter().filter(|i| i.project_id == project.id).map(|i| i.updated_at).max();
    let session = sessions.iter().filter(|s| s.project_id == project.id).map(|s| s.updated_at).max();
    issue.max(session).unwrap_or(project.created_at)
}

fn state_tint(state: IssueState) -> egui::Color32 {
    match state {
        IssueState::Doing => TRACE,
        IssueState::Review => READOUT,
        IssueState::Todo => VALUE,
        _ => LEGEND,
    }
}

fn count_chips(ui: &mut Ui, counts: &[usize; 5], skip_done: bool) {
    ui.horizontal_wrapped(|ui| {
        for (state, n) in IssueState::ALL.into_iter().zip(counts) {
            if skip_done && state == IssueState::Done {
                continue;
            }
            chip(ui, &format!("{n} {}", state.as_str()), state_tint(state), *n > 0);
        }
    });
}

fn clickable(ui: &mut Ui, id: impl std::fmt::Debug + std::hash::Hash, body: impl FnOnce(&mut Ui)) -> egui::Response {
    let inner = ui.scope(body).response;
    let r = ui.interact(inner.rect, ui.scope_id().with(id), Sense::click());
    if r.hovered() {
        ui.painter().rect_stroke(inner.rect.expand(2.0), 3.0, Stroke::new(1.0, ETCH), egui::StrokeKind::Outside);
    }
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
}

struct Row {
    project: Project,
    server: Option<String>,
    level: agentbench_core::Automation,
    inherited: bool,
    running: usize,
    counts: [usize; 5],
    vision: bool,
    last: i64,
}

impl App {
    pub fn open_project_view(&mut self, id: Id) {
        self.tab = Tab::Projects;
        self.project_view = Some(id);
        self.project = Some(id);
        if let Some(c) = &mut self.client {
            c.notes(id);
        }
    }

    pub fn set_tab_projects(&mut self) {
        self.tab = Tab::Projects;
    }

    pub fn project_notes_len(&self, id: Id) -> usize {
        self.project_notes.get(&id).map_or(0, |n| n.len())
    }

    pub fn close_project_view(&mut self) {
        self.project_view = None;
    }

    pub fn project_view(&self) -> Option<Id> {
        self.project_view.filter(|_| self.tab == Tab::Projects)
    }

    pub fn set_project_query(&mut self, query: &str) {
        self.project_query = query.to_string();
    }

    pub fn set_project_sort(&mut self, by: &str) {
        if let Some(s) = ProjectSort::ALL.into_iter().find(|s| s.label() == by) {
            self.project_sort = s;
        }
    }

    pub fn project_summary(&self, id: Id) -> Option<([usize; 5], usize, bool)> {
        let o = self.overview.as_ref()?;
        Some((
            issue_counts(&o.issues, id),
            o.sessions.iter().filter(|s| s.project_id == id && s.status.busy()).count(),
            o.visions.iter().any(|v| v.project_id == id && !v.body.summary.trim().is_empty()),
        ))
    }

    pub fn listed_projects(&self) -> Vec<Id> {
        let Some(o) = &self.overview else { return Vec::new() };
        let mut list: Vec<&Project> = o
            .projects
            .iter()
            .filter(|p| !p.supervisor)
            .filter(|p| project_matches(p, &self.project_query))
            .collect();
        sort_projects(&mut list, &o.issues, &o.sessions, self.project_sort);
        list.into_iter().map(|p| p.id).collect()
    }

    pub(super) fn projects_tab(&mut self, ui: &mut Ui) {
        if self.overview.is_none() {
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            match self.project_view.filter(|id| self.has_project(*id)) {
                Some(id) => self.project_page(ui, id),
                None => self.project_list(ui),
            }
        });
    }

    fn has_project(&self, id: Id) -> bool {
        self.overview.as_ref().is_some_and(|o| o.projects.iter().any(|p| p.id == id))
    }

    fn project_list(&mut self, ui: &mut Ui) {
        let phone = ui.available_width() < 560.0;
        ui.spacing_mut().item_spacing.y = 8.0;
        if phone {
            field(ui, &mut self.project_query, "filter projects");
            choice(ui, "project-sort", &mut self.project_sort, ProjectSort::ALL.map(|s| (s, s.label().to_string())));
        } else {
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 210.0).clamp(120.0, 420.0);
                ui.allocate_ui(Vec2::new(w, KEY_H), |ui| field(ui, &mut self.project_query, "filter projects"));
                ui.allocate_ui(Vec2::new(190.0, KEY_H), |ui| {
                    choice(ui, "project-sort", &mut self.project_sort, ProjectSort::ALL.map(|s| (s, s.label().to_string())));
                });
            });
        }
        let ids = self.listed_projects();
        let rows: Vec<Row> = {
            let o = self.overview.as_ref().unwrap();
            let default = self.default_level();
            ids.iter()
                .filter_map(|id| o.projects.iter().find(|p| p.id == *id))
                .map(|p| Row {
                    project: p.clone(),
                    server: self.server_label(p.id),
                    level: p.level(default),
                    inherited: p.automation.is_none(),
                    running: o.sessions.iter().filter(|s| s.project_id == p.id && s.status.busy()).count(),
                    counts: issue_counts(&o.issues, p.id),
                    vision: o.visions.iter().any(|v| v.project_id == p.id && !v.body.summary.trim().is_empty()),
                    last: last_activity(p, &o.issues, &o.sessions),
                })
                .collect()
        };
        if rows.is_empty() {
            note(ui, "No project matches. Clear the filter or add a project from the agents tab.", LEGEND);
        }
        let mut open = None;
        for row in &rows {
            let r = clickable(ui, ("project-row", row.project.id), |ui| {
                section_panel(ui, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                        self.project_badge(ui, rect, row.project.id, &row.project.name);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            Line::new().value(&row.project.name).size(13.0).elided(ui);
                            let place = match &row.server {
                                Some(s) => format!("{}  [{s}]", short_path(&row.project.path)),
                                None => short_path(&row.project.path),
                            };
                            Line::new().note(place).size(10.5).elided(ui);
                        });
                    });
                    ui.horizontal_wrapped(|ui| {
                        let word = if row.inherited { format!("{} (default)", row.level.label()) } else { row.level.label().to_string() };
                        chip(ui, &word, if row.inherited { READOUT_DIM } else { READOUT }, false);
                        let running = format!("{} running", row.running);
                        chip(ui, &running, TRACE, row.running > 0);
                        chip(ui, if row.vision { "vision" } else { "no vision" }, if row.vision { OK } else { LEGEND }, row.vision);
                    });
                    count_chips(ui, &row.counts, true);
                    Line::new().note(format!("last activity {}", ago(row.last))).size(10.5).elided(ui);
                    ui.add_space(4.0);
                });
            });
            r.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::Button, true, format!("open project {}", row.project.name))
            });
            if r.clicked() {
                open = Some(row.project.id);
            }
        }
        if let Some(id) = open {
            self.open_project_view(id);
        }
    }

    fn project_page(&mut self, ui: &mut Ui, id: Id) {
        let Some(o) = self.overview.clone() else { return };
        let Some(p) = o.projects.iter().find(|p| p.id == id).cloned() else { return };
        ui.spacing_mut().item_spacing.y = 8.0;
        let phone = ui.available_width() < 560.0;
        let mut back = false;
        let mut edit = false;
        let mut show_all = false;
        let mut open_chat: Option<(SessionId, Id)> = None;
        let mut open_issue: Option<Id> = None;
        ui.horizontal_wrapped(|ui| {
            back = ui.add(Key::new("all projects").outline(VALUE)).clicked();
            Line::new().value(&p.name).size(15.0).elided(ui);
        });

        let trees: Vec<&Worktree> = o.worktrees.iter().filter(|w| w.project_id == id).collect();
        section(ui, "overview", &short_path(&p.path), |ui| {
            let col = if phone { 0.0 } else { 120.0 };
            let line = |ui: &mut Ui, label: &str, value: String| {
                if phone {
                    Line::new().legend(label).size(10.0).elided(ui);
                    Line::new().value(value).size(11.5).wrapped(ui);
                } else {
                    Line::new().legend(label).hanging(ui, col, Line::new().value(value).size(11.5));
                }
            };
            line(ui, "path", p.path.clone());
            line(ui, "default branch", p.branch.clone().unwrap_or_else(|| "none".into()));
            line(ui, "server", self.server_label(id).unwrap_or_else(|| "this server".into()));
            line(ui, "worktrees", trees.len().to_string());
            let risky = if p.risky.is_empty() {
                "none".to_string()
            } else {
                p.risky.iter().map(|r: &Risky| r.label()).collect::<Vec<_>>().join(", ")
            };
            line(ui, "risky actions", risky);
            line(ui, "deploy", p.deploy.clone().unwrap_or_else(|| "not set".into()));
            Line::new().legend("automation").size(10.0).show(ui);
            self.level_row(ui, &p, if phone { 150.0 } else { 220.0 });
            if self.server_has(id, agentbench_core::API_MODEL_LEVEL) {
                self.model_levels_row(ui, &p);
            }
        });

        let vision = o.visions.iter().find(|v| v.project_id == id);
        section(ui, "vision", "what the project is for", |ui| {
            match vision.filter(|v| !v.body.summary.trim().is_empty() || !v.body.goals.is_empty()) {
                None => note(ui, "No vision yet. Write one so agents know what to work on.", LEGEND),
                Some(v) => {
                    if !v.body.summary.trim().is_empty() {
                        note(ui, v.body.summary.trim(), VALUE);
                    }
                    for g in &v.body.goals {
                        ui.add_space(2.0);
                        let tint = match g.status {
                            GoalStatus::Active => READOUT,
                            GoalStatus::Done => OK,
                            GoalStatus::Dropped => LEGEND,
                        };
                        ui.horizontal_wrapped(|ui| {
                            chip(ui, g.status.as_str(), tint, g.status == GoalStatus::Active);
                            let id = Line::new().value(&g.id).tint(LEGEND).size(11.0).elided(ui);
                            id.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, format!("goal {}", g.id)));
                        });
                        note(ui, &g.text, VALUE);
                        count_chips(ui, &goal_counts(&o.issues, id, &g.id), false);
                    }
                    if !v.body.non_goals.is_empty() {
                        ui.add_space(4.0);
                        Line::new().legend("non-goals").size(10.0).show(ui);
                        for n in &v.body.non_goals {
                            note(ui, n, LEGEND);
                        }
                    }
                }
            }
            if ui.add(Key::new("edit vision").outline(VALUE)).clicked() {
                edit = true;
            }
        });

        let mut mine: Vec<&Session> = o.sessions.iter().filter(|s| s.project_id == id && Some(s.id) != o.supervisor).collect();
        mine.sort_by_key(|s| (!s.status.busy(), std::cmp::Reverse(s.updated_at)));
        section(ui, "agents at work", &format!("{} chats, {} running", mine.len(), mine.iter().filter(|s| s.status.busy()).count()), |ui| {
            if mine.is_empty() {
                note(ui, "No agents have worked here yet.", LEGEND);
            }
            for s in mine.iter().take(12) {
                let r = clickable(ui, ("project-chat", s.id), |ui| {
                    let (tint, word) = status_look(s.status);
                    ui.horizontal_wrapped(|ui| {
                        chip(ui, word, tint, s.status.busy());
                        if s.stuck {
                            chip(ui, "stuck", FAULT, true);
                        }
                    });
                    Line::new().value(&s.title).size(11.5).tint(VALUE).wrapped(ui);
                    let branch = o.worktrees.iter().find(|w| w.id == s.worktree_id).map(|w| w.label()).unwrap_or_default();
                    let model = s.model.clone().filter(|m| !m.is_empty()).unwrap_or_else(|| s.agent.clone());
                    let issue = s.issue_id.map(|i| format!("  issue #{}", crate::hub::untag(i))).unwrap_or_default();
                    Line::new().note(format!("{model}  {branch}{issue}  {}", ago(s.updated_at))).size(10.5).wrapped(ui);
                });
                r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("open chat {}", s.title)));
                if r.clicked() {
                    open_chat = Some((s.id, s.project_id));
                }
            }
            if mine.len() > 12 {
                hint(ui, &format!("and {} more in the agents tab", mine.len() - 12));
            }
        });

        let counts = issue_counts(&o.issues, id);
        section(ui, "issues", "counts by state", |ui| {
            count_chips(ui, &counts, false);
            let mut top: Vec<&Issue> = o
                .issues
                .iter()
                .filter(|i| i.project_id == id && matches!(i.state, IssueState::Todo | IssueState::Doing | IssueState::Review))
                .collect();
            top.sort_by_key(|i| {
                (IssueState::ALL.iter().position(|s| *s == i.state).map(|n| 5 - n), std::cmp::Reverse(i.priority))
            });
            for i in top.iter().take(6) {
                let r = clickable(ui, ("project-issue", i.id), |ui| {
                    Line::new()
                        .value(i.state.as_str())
                        .tint(state_tint(i.state))
                        .size(10.5)
                        .gap(8.0)
                        .value(format!("#{} {}", crate::hub::untag(i.id), i.title))
                        .size(11.5)
                        .tint(VALUE)
                        .wrapped(ui);
                });
                if r.clicked() {
                    open_issue = Some(i.id);
                }
            }
            if ui.add(Key::new("show all").outline(VALUE)).clicked() {
                show_all = true;
            }
        });

        let notes = self.project_notes.get(&id);
        section(ui, "memory", "notes agents keep for this project", |ui| {
            let live: Vec<_> = notes.into_iter().flatten().filter(|n| n.current.is_some()).collect();
            if live.is_empty() {
                note(ui, "No notes yet.", LEGEND);
            }
            for n in live {
                egui::CollapsingHeader::new(egui_bench::text::legend(&n.topic))
                    .id_salt(("project-note", id, &n.topic))
                    .show(ui, |ui| {
                        note(ui, n.current.as_deref().unwrap_or_default(), VALUE);
                    });
            }
        });

        let feed: Vec<&Activity> = o
            .autopilot
            .iter()
            .flat_map(|a| &a.activity)
            .filter(|a| a.project_id == Some(id))
            .collect();
        let mut feed = feed;
        feed.sort_by_key(|a| std::cmp::Reverse(a.at));
        let mut done: Vec<&Issue> = o.issues.iter().filter(|i| i.project_id == id && i.state == IssueState::Done).collect();
        done.sort_by_key(|i| std::cmp::Reverse(i.updated_at));
        section(ui, "recent activity", "autopilot and finished issues", |ui| {
            if feed.is_empty() && done.is_empty() {
                note(ui, "Nothing yet.", LEGEND);
            }
            for a in feed.iter().take(5) {
                Line::new().note(ago(a.at)).size(10.5).gap(8.0).value(&a.text).size(11.0).tint(VALUE).wrapped(ui);
            }
            for i in done.iter().take(5) {
                Line::new()
                    .note(ago(i.updated_at))
                    .size(10.5)
                    .gap(8.0)
                    .value(format!("done #{} {}", crate::hub::untag(i.id), i.title))
                    .size(11.0)
                    .tint(OK)
                    .wrapped(ui);
            }
        });

        if back {
            self.close_project_view();
        }
        if edit {
            self.open_vision(id);
        }
        if show_all {
            self.project = Some(id);
            self.scope = Scope::Project;
            self.tab = Tab::Issues;
            self.detail = None;
        }
        if let Some((sid, pid)) = open_chat {
            self.open_session(sid, pid);
        }
        if let Some(i) = open_issue {
            self.project = Some(id);
            self.open_issue_pane(i);
        }
    }
}
