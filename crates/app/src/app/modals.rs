//! The modal dialogs: scan, add project, file an issue, start a chat, add a worktree and confirm.

use super::*;

impl App {
    pub(super) fn modals(&mut self, ctx: &egui::Context) {
        self.add_project_modal(ctx);
        self.file_issue_modal(ctx);
        self.start_modal(ctx);
        self.worktree_modal(ctx);
        self.confirm_modal(ctx);
        self.terminal_modal(ctx);
        self.settings_modal(ctx);
        self.scan_modal(ctx);
    }

    /// The camera, looking for a pairing code. Pairs and closes itself when it reads one.
    pub(super) fn scan_modal(&mut self, ctx: &egui::Context) {
        let Some(scanner) = &self.scanner else {
            return;
        };
        let state = scanner.state();
        if let Scan::Found(text) = &state {
            let found = agentbench_core::pairing::Pairing::parse(text);
            self.scanner = None;
            self.scan_view = None;
            match found {
                Some(p) => self.pair(ctx, p),
                None => {
                    self.notice =
                        Some((false, "that code is not an agentbench pairing code".into()))
                }
            }
            return;
        }
        if let Some((grey, w, h)) = scanner.preview() {
            let (grey, w, h) = crate::scan::upright(&grey, w, h);
            let image = egui::ColorImage::from_gray([w, h], &grey);
            match &mut self.scan_view {
                Some(t) => t.set(image, egui::TextureOptions::LINEAR),
                None => {
                    self.scan_view =
                        Some(ctx.load_texture("scan", image, egui::TextureOptions::LINEAR))
                }
            }
        }
        let mut close = false;
        let view = self.scan_view.clone();
        let m = egui::Modal::new(egui::Id::new("scan"))
            .backdrop_color(egui::Color32::from_black_alpha(200))
            .show(ctx, |ui| {
                ui.set_width(modal_width(ctx, 420.0));
                modal_title(ui, "Scan the code");
                match &state {
                    Scan::Failed(why) => status(ui, false, why),
                    Scan::Starting => hint(ui, "starting the camera"),
                    _ => hint(ui, "Point the camera at the code in agentbench's settings on your computer, under pair a phone."),
                }
                if let Some(t) = &view {
                    ui.add_space(8.0);
                    let width = ui.available_width();
                    let size = t.size_vec2();
                    ui.add(egui::Image::new(egui::load::SizedTexture::new(t.id(), size)).fit_to_exact_size(Vec2::new(width, width * size.y / size.x.max(1.0))));
                }
                ui.add_space(8.0);
                if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                    close = true;
                }
            });
        if m.should_close() || close {
            self.scanner = None;
            self.scan_view = None;
        } else {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    pub(super) fn terminal_modal(&mut self, ctx: &egui::Context) {
        let Some(t) = &mut self.terminal else {
            return;
        };
        let session = t.session;
        let now = web_time::Instant::now();
        let on_shell = t.job.is_none();
        let font = mono(13.0);
        let (cell_w, cell_h) = ctx.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let width = modal_width(ctx, 900.0);
        let height = (ctx.content_rect().height() * 0.6).clamp(200.0, 560.0);
        let size = (((height / cell_h) as u16).max(5), (((width - 20.0) / cell_w) as u16).max(20));
        if let Some(c) = &mut self.client {
            if t.jobs_asked.is_none_or(|a| now.duration_since(a) >= CONSOLE_EVERY) {
                c.jobs(session);
                t.jobs_asked = Some(now);
            }
            if on_shell {
                let data = terminal_bytes(&ctx.input(|i| i.events.clone()));
                let resize = if size != t.size { size } else { (0, 0) };
                if !data.is_empty() || resize != (0, 0) {
                    c.terminal_send(session, &data, resize.0, resize.1);
                    t.size = size;
                    t.asked = None;
                }
                if t.asked.is_none_or(|a| now.duration_since(a) >= TERMINAL_EVERY) {
                    c.terminal(session);
                    t.asked = Some(now);
                }
            }
        }
        let screen = t.screen.clone();
        let jobs = t.jobs.clone();
        let mut selected = t.job.clone();
        let mut close = false;
        let mut kill = None;
        let m = egui::Modal::new(egui::Id::new("terminal"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    modal_title(ui, "Terminal");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = ui.add(Key::new("close").outline(FAULT)).clicked();
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    if tab_chip(ui, "shell", READOUT, selected.is_none()).clicked() {
                        selected = None;
                    }
                    for j in &jobs {
                        let tint = if j.running {
                            OK
                        } else if j.exit == Some(0) {
                            LEGEND
                        } else {
                            FAULT
                        };
                        let on = selected.as_deref() == Some(j.handle.as_str());
                        if tab_chip(ui, &format!("{} {}", j.handle, job_state(j)), tint, on).clicked() {
                            selected = Some(j.handle.clone());
                        }
                    }
                });
                ui.add_space(4.0);
                let job = selected.as_ref().and_then(|h| jobs.iter().find(|j| &j.handle == h));
                let Some(job) = job else {
                    hint(
                        ui,
                        "A shell in this chat's folder. The agent never sees what you type or paste here. Commands the agent starts in the background get a tab each.",
                    );
                    ui.add_space(4.0);
                    egui::Frame::NONE.fill(CHASSIS).inner_margin(6).show(ui, |ui| {
                        ui.set_min_height(height);
                        let Some(s) = &screen else {
                            return hint(ui, "starting");
                        };
                        let origin = ui.cursor().min;
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(s.rows.join("\n")).font(font.clone()),
                            )
                            .wrap_mode(egui::TextWrapMode::Extend),
                        );
                        if s.cursor_shown && s.alive {
                            let at = origin
                                + Vec2::new(
                                    s.cursor_col as f32 * cell_w,
                                    s.cursor_row as f32 * cell_h,
                                );
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(at, Vec2::new(cell_w, cell_h)),
                                0.0,
                                READOUT.gamma_multiply(0.6),
                            );
                        }
                        if !s.alive {
                            hint(ui, "the shell has exited");
                        }
                    });
                    return;
                };
                note(ui, &job.command, LEGEND);
                ui.add_space(4.0);
                egui::Frame::NONE.fill(CHASSIS).inner_margin(6).show(ui, |ui| {
                    egui::ScrollArea::both()
                        .max_height(height)
                        .min_scrolled_height(height)
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            if job.dropped > 0 {
                                hint(
                                    ui,
                                    &format!("{} bytes of older output were dropped", job.dropped),
                                );
                            }
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(strip_ansi(&job.text)).font(font.clone()),
                                )
                                .selectable(true)
                                .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        });
                });
                ui.add_space(6.0);
                footer(ui, |ui| {
                    if job.running
                        && ui.add(Key::new("stop it").solid(FAULT).min_width(100.0)).clicked()
                    {
                        kill = Some(job.handle.clone());
                    }
                });
            });
        if close || m.backdrop_response.clicked() {
            if let Some(c) = &mut self.client {
                c.terminal_close(session);
            }
            self.terminal = None;
            return;
        }
        if let Some(t) = &mut self.terminal {
            t.job = selected;
            if let Some(handle) = kill
                && let Some(c) = &mut self.client
            {
                c.kill_job(session, &handle);
                t.jobs_asked = None;
            }
        }
        ctx.request_repaint_after(TERMINAL_EVERY);
    }

    pub(super) fn add_project_modal(&mut self, ctx: &egui::Context) {
        let Some(a) = &mut self.add_project else {
            return;
        };
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::new("add-project")).backdrop_color(egui::Color32::from_black_alpha(150)).show(ctx, |ui| {
            ui.set_width(modal_width(ctx, 500.0));
            modal_title(ui, "Add a project");
            section(ui, "repository", "a folder on the server", |ui| {
                row_help(ui, "path", "Any folder inside the repo works, even a worktree. The main checkout becomes the project.", |ui| {
                    field(ui, &mut a.path, "~/git/snort");
                });
                row_help(ui, "name", "Taken from the origin remote when left empty.", |ui| {
                    field(ui, &mut a.name, "optional");
                });
                row_help(ui, "new folder", "Make the folder if it is missing and run git init in it, so a brand new project can start here.", |ui| {
                    ui.checkbox(&mut a.create, "create it");
                });
            });
            footer(ui, |ui| {
                if ui.add(Key::new(if a.create { "create" } else { "add" }).solid(READOUT).enabled(!a.path.trim().is_empty()).min_width(100.0)).clicked() {
                    go = true;
                }
                if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                    close = true;
                }
            });
        });
        if m.should_close() {
            close = true;
        }
        if go {
            let new = NewProject {
                path: a.path.trim().to_string(),
                name: Some(a.name.trim().to_string()).filter(|n| !n.is_empty()),
                create: a.create,
            };
            if let Some(c) = &mut self.client {
                c.add_project(&new);
            }
            self.add_project = None;
        } else if close {
            self.add_project = None;
        }
    }

    pub(super) fn file_issue_modal(&mut self, ctx: &egui::Context) {
        let Some(f) = &mut self.file_issue else {
            return;
        };
        let name = self
            .overview
            .as_ref()
            .and_then(|o| o.projects.iter().find(|p| p.id == f.project))
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::new("file-issue"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(modal_width(ctx, 520.0));
                modal_title(ui, &format!("File an issue in {name}"));
                section(ui, "issue", "what needs doing", |ui| {
                    row(ui, "title", |ui| {
                        field(ui, &mut f.title, "fix the login loop");
                    });
                    prose(ui, &mut f.body, "details, steps, what done looks like", 6);
                });
                footer(ui, |ui| {
                    if ui
                        .add(
                            Key::new("file")
                                .solid(READOUT)
                                .enabled(!f.title.trim().is_empty())
                                .min_width(100.0),
                        )
                        .clicked()
                    {
                        go = true;
                    }
                    if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                        close = true;
                    }
                });
            });
        if m.should_close() {
            close = true;
        }
        if go {
            let new = NewIssue {
                project_id: f.project,
                title: f.title.trim().to_string(),
                body: f.body.trim().to_string(),
                ..Default::default()
            };
            if let Some(c) = &mut self.client {
                c.add_issue(&new);
            }
            self.file_issue = None;
        } else if close {
            self.file_issue = None;
        }
    }

    pub(super) fn start_modal(&mut self, ctx: &egui::Context) {
        let Some(o) = &self.overview else {
            return;
        };
        let Some(s) = &mut self.start else {
            return;
        };
        let agents = o.agents.clone();
        let agent_models = self.agent_models.clone();
        let trees: Vec<Worktree> = o
            .worktrees
            .iter()
            .filter(|w| w.project_id == s.project && !w.missing)
            .cloned()
            .collect();
        let issue = s.issue.and_then(|id| o.issues.iter().find(|i| i.id == id)).cloned();
        let project = o
            .projects
            .iter()
            .find(|p| p.id == s.project)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::new("start")).backdrop_color(egui::Color32::from_black_alpha(150)).show(ctx, |ui| {
            ui.set_width(modal_width(ctx, 520.0));
            modal_title(ui, &match &issue {
                Some(i) => format!("Start an agent on #{}", i.id),
                None => format!("Start an agent in {project}"),
            });
            section(ui, "agent", "who does the work", |ui| {
                row(ui, "agent", |ui| {
                    if choice(ui, "agent", &mut s.agent, agents.iter().map(|a| {
                        let label = if a.available { a.label.clone() } else { format!("{} (not installed)", a.label) };
                        (a.name.clone(), label)
                    })) {
                        s.model.clear();
                    }
                });
                row_help(ui, "model", "Passed to the agent as is. Empty uses its default.", |ui| {
                    let list = agent_models.get(&s.agent);
                    model_input(ui, "start-model", &mut s.model, "default", list);
                });
            });
            ui.add_space(8.0);
            section(ui, "worktree", "where it works", |ui| {
                let mut opts: Vec<(PlaceChoice, String)> = vec![(PlaceChoice::New, "a new worktree".into())];
                for w in &trees {
                    let label = if w.main { format!("{} (main checkout)", w.label()) } else { w.label() };
                    opts.push((if w.main { PlaceChoice::Main } else { PlaceChoice::Existing(w.id) }, label));
                }
                row(ui, "place", |ui| {
                    choice(ui, "place", &mut s.place, opts);
                });
                if s.place == PlaceChoice::New {
                    row_help(ui, "branch", "Made off the main checkout's branch, beside the repo in .worktrees.", |ui| {
                        let hint_text = issue.as_ref().map(|i| format!("issue-{}-{}", i.id, agentbench_core::branch_slug(&i.title))).unwrap_or("fix-login".into());
                        field(ui, &mut s.branch, &hint_text);
                    });
                }
                let busy = trees.iter().any(|w| {
                    let here = match &s.place {
                        PlaceChoice::Main => w.main,
                        PlaceChoice::Existing(id) => w.id == *id,
                        PlaceChoice::New => false,
                    };
                    here && o.sessions.iter().any(|x| x.worktree_id == w.id && x.status.busy())
                });
                if busy {
                    note(ui, "Another agent is working in this worktree. Two agents in one checkout will trip over each other.", WARN);
                }
            });
            ui.add_space(8.0);
            section(ui, "prompt", if issue.is_some() { "empty sends the issue" } else { "the first message" }, |ui| {
                prose(ui, &mut s.prompt, "what should it do", 4);
            });
            let available = agents.iter().any(|a| a.name == s.agent && a.available);
            let needs_branch = s.place == PlaceChoice::New && s.branch.trim().is_empty() && issue.is_none();
            if !available {
                status(ui, false, "that agent is not installed on the server");
            } else if needs_branch {
                status(ui, false, "name the branch for the new worktree");
            }
            footer(ui, |ui| {
                let ready = available && !needs_branch;
                if ui.add(Key::new("start").solid(READOUT).enabled(ready).min_width(100.0)).clicked() {
                    go = true;
                }
                if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                    close = true;
                }
            });
        });
        if m.should_close() {
            close = true;
        }
        if go {
            let place = match &s.place {
                PlaceChoice::Main => Place::Main,
                PlaceChoice::Existing(id) => Place::Existing { worktree_id: *id },
                PlaceChoice::New => Place::New { branch: s.branch.trim().to_string() },
            };
            let new = NewSession {
                project_id: s.project,
                agent: s.agent.clone(),
                place,
                title: None,
                model: Some(s.model.trim().to_string()).filter(|m| !m.is_empty()),
                issue_id: s.issue,
                prompt: Some(s.prompt.trim().to_string()).filter(|p| !p.is_empty()),
                attachments: Vec::new(),
            };
            if let Some(c) = &mut self.client {
                match s.issue {
                    Some(iid) => c.start_issue(iid, &new),
                    None => {
                        c.add_session(&new);
                        self.awaiting = Some(new.project_id);
                    }
                }
            }
            self.start = None;
        } else if close {
            self.start = None;
        }
    }

    pub(super) fn worktree_modal(&mut self, ctx: &egui::Context) {
        let Some((pid, branch)) = &mut self.new_worktree else {
            return;
        };
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::new("worktree")).backdrop_color(egui::Color32::from_black_alpha(150)).show(ctx, |ui| {
            ui.set_width(modal_width(ctx, 460.0));
            modal_title(ui, "New worktree");
            section(ui, "branch", "checked out beside the repo", |ui| {
                row_help(ui, "name", "An existing branch is checked out, a new one is made off the main checkout.", |ui| {
                    field(ui, branch, "fix-login");
                });
            });
            footer(ui, |ui| {
                if ui.add(Key::new("make it").solid(READOUT).enabled(!branch.trim().is_empty()).min_width(100.0)).clicked() {
                    go = true;
                }
                if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                    close = true;
                }
            });
        });
        if m.should_close() {
            close = true;
        }
        if go {
            let (pid, branch) = (*pid, branch.trim().to_string());
            if let Some(c) = &mut self.client {
                c.add_worktree(pid, &NewWorktree { branch, base: None });
            }
            self.new_worktree = None;
        } else if close {
            self.new_worktree = None;
        }
    }

    pub(super) fn confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(c) = &self.confirm else {
            return;
        };
        let (title, body, key) = match c {
            Confirm::Project(_, name) => (
                format!("Remove {name}?"),
                "Its issues, sessions and transcripts are deleted from agentbench. The repository and its worktrees stay on disk.",
                "remove",
            ),
            Confirm::Worktree(_, name, missing) => (
                format!("Remove the {name} worktree?"),
                if *missing {
                    "It is already gone from disk. Its sessions and their transcripts are deleted."
                } else {
                    "The checkout is deleted, uncommitted changes with it. The branch is kept. Its sessions and transcripts are deleted."
                },
                "remove",
            ),
            Confirm::Session(_, name) => (
                format!("Close {name}?"),
                "The agent is stopped if it is working and the conversation is deleted. The worktree and its changes stay.",
                "close and delete",
            ),
            Confirm::Issue(_, name) => (
                format!("Delete {name}?"),
                "Its comments go with it. Sessions that worked on it are kept.",
                "delete",
            ),
        };
        let mut done = false;
        let mut gone = None;
        let m = egui::Modal::new(egui::Id::new("confirm"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(modal_width(ctx, 400.0));
                modal_title(ui, &title);
                note(ui, body, LEGEND);
                footer(ui, |ui| {
                    if ui.add(Key::new(key).solid(FAULT).min_width(100.0)).clicked() {
                        if let Some(client) = &mut self.client {
                            match c {
                                Confirm::Project(id, _) => client.remove_project(*id),
                                Confirm::Worktree(id, _, _) => client.remove_worktree(*id, true),
                                Confirm::Session(id, _) => {
                                    client.delete_session(*id);
                                    gone = Some(*id);
                                }
                                Confirm::Issue(id, _) => client.delete_issue(*id),
                            }
                        }
                        done = true;
                    }
                    if ui.add(Key::new("keep it").outline(VALUE).min_width(100.0)).clicked() {
                        done = true;
                    }
                });
            });
        if let Some(id) = gone {
            self.forget(id);
        }
        if done || m.should_close() {
            self.confirm = None;
        }
    }

    /// Escape stops the agent working in the open chat. Dialogs, pickers and menus take the
    /// key first to close themselves, so this only sees an Escape nothing else wanted.
    pub(super) fn escape_stops_agent(&mut self, ctx: &egui::Context) {
        // Escape while the microphone is open throws the take away, and only that.
        if egui::Popup::is_any_open(ctx) || self.voice.listening() {
            return;
        }
        let Some(s) = self.current_session().filter(|s| s.status.busy()).map(|s| s.id) else {
            return;
        };
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            && let Some(c) = &mut self.client
        {
            c.interrupt(s);
        }
    }
}
