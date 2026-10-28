//! A chat: the tiles, a tile's header, and the session view that holds the transcript above the composer.

use super::transcript::{latest_plan, plan_panel};
use super::*;

impl App {
    pub(super) fn chat(&mut self, ui: &mut Ui) {
        if self.grid_now().slots() == 1 {
            self.focused = true;
            self.compact = false;
            let tile = self.tiles()[0];
            self.in_pane(tile, |app| app.tile(ui, tile));
            return;
        }
        let (cols, rows) = self.grid_now().dims();
        let area = ui.available_rect_before_wrap();
        let w = (area.width() - TILE_GAP * (cols - 1) as f32) / cols as f32;
        let h = (area.height() - TILE_GAP * (rows - 1) as f32) / rows as f32;
        let rects: Vec<egui::Rect> = (0..cols * rows)
            .map(|i| {
                let (col, row) = ((i % cols) as f32, (i / cols) as f32);
                egui::Rect::from_min_size(
                    area.min + Vec2::new(col * (w + TILE_GAP), row * (h + TILE_GAP)),
                    Vec2::new(w, h),
                )
            })
            .collect();
        // Pressing anywhere in a tile focuses it.
        if ui.input(|i| i.pointer.any_pressed())
            && let Some(at) = rects.iter().position(|r| ui.rect_contains_pointer(*r))
            && at != self.focus_slot()
        {
            self.focus_tile(at);
        }
        let tiles = self.tiles();
        for (i, (rect, tile)) in rects.into_iter().zip(tiles).enumerate() {
            let focused = i == self.focus_slot();
            let edge = if focused { READOUT.gamma_multiply(0.75) } else { ETCH };
            ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("tile", i, tile))
                    .max_rect(rect.shrink(8.0))
                    .layout(Layout::top_down(Align::Min)),
            );
            child.set_clip_rect(rect.shrink(1.0).intersect(ui.clip_rect()));
            self.focused = focused;
            self.compact = true;
            self.in_pane(tile, |app| app.tile(&mut child, tile));
            self.compact = false;
            self.focused = true;
        }
        ui.allocate_rect(area, Sense::hover());
    }

    /// Runs `draw` with the draft and scroll state of the chat in this tile, so each tile
    /// keeps what was typed into it.
    pub(super) fn in_pane<R>(&mut self, tile: Tile, draw: impl FnOnce(&mut Self) -> R) -> R {
        let pane = match tile {
            Tile::Chat(s) => self.panes.remove(&s).unwrap_or_default(),
            Tile::New => std::mem::take(&mut self.new_pane),
            Tile::Empty => Pane::default(),
        };
        let draft = std::mem::replace(&mut self.draft, pane.draft);
        let follow = std::mem::replace(&mut self.follow, pane.follow);
        let before = self.drawing.replace(tile);
        let out = draw(self);
        self.drawing = before;
        let pane = Pane {
            draft: std::mem::replace(&mut self.draft, draft),
            follow: std::mem::replace(&mut self.follow, follow),
        };
        match tile {
            Tile::Chat(s) => {
                self.panes.insert(s, pane);
            }
            Tile::New => self.new_pane = pane,
            Tile::Empty => {}
        }
        out
    }

    pub(super) fn tile(&mut self, ui: &mut Ui, tile: Tile) {
        match tile {
            Tile::New => self.blank_chat(ui),
            Tile::Empty => {
                ui.add_space(12.0);
                hint(ui, "click a chat in the list to fill the focused tile");
            }
            Tile::Chat(sid) => match self.session_by_id(sid).cloned() {
                Some(s) => self.session_view(ui, s),
                None if self.compact => hint(ui, "loading"),
                None => self.blank_chat(ui),
            },
        }
    }

    pub(super) fn blank_chat(&mut self, ui: &mut Ui) {
        let Some(o) = self.overview.clone() else {
            return;
        };
        let kept = self.fresh.as_ref().map(|f| f.project).filter(|_| self.compact);
        let Some(project) = kept.or(self.project).or(o
            .projects
            .iter()
            .find(|p| !p.home)
            .or(o.projects.first())
            .map(|p| p.id))
        else {
            section(ui, "no projects yet", "", |ui| {
                note(
                    ui,
                    "Add a git repository on the left to start chatting with an agent in it.",
                    LEGEND,
                );
            });
            return;
        };
        let fresh =
            self.fresh.get_or_insert_with(|| Fresh::new(project, PlaceChoice::Main, String::new()));
        if fresh.project != project {
            *fresh = Fresh::new(project, PlaceChoice::Main, String::new());
        }
        if fresh.agent.is_empty() {
            fresh.agent = {
                let last = o.sessions.iter().max_by_key(|s| s.updated_at).map(|s| s.agent.clone());
                last.filter(|a| o.agents.iter().any(|x| &x.name == a && x.available))
                    .or_else(|| o.agents.iter().find(|a| a.available).map(|a| a.name.clone()))
                    .unwrap_or_default()
            };
        }
        let trees: Vec<Worktree> =
            o.worktrees.iter().filter(|w| w.project_id == project && !w.missing).cloned().collect();
        let name =
            o.projects.iter().find(|p| p.id == project).map(|p| p.name.clone()).unwrap_or_default();
        if matches!(fresh.place, PlaceChoice::Existing(id) if !trees.iter().any(|w| w.id == id)) {
            fresh.place = PlaceChoice::Main;
        }
        let home = o.projects.iter().any(|p| p.id == project && p.home);
        let here = match fresh.place {
            PlaceChoice::New => None,
            PlaceChoice::Existing(id) => trees.iter().find(|w| w.id == id),
            PlaceChoice::Main => trees.iter().find(|w| w.main),
        }
        .map(|w| w.label())
        .unwrap_or_else(|| "a new worktree".to_string());
        ui.add_space(ui.available_height() * if self.compact { 0.06 } else { 0.18 });
        ui.vertical_centered(|ui| {
            ui.set_max_width(ui.available_width().min(640.0));
            ui.label(
                egui::RichText::new(
                    if home { "chats in ~".to_string() } else { format!("{name} on {here}") }
                        .to_uppercase(),
                )
                    .font(legend_font(15.0))
                    .color(VALUE)
                    .extra_letter_spacing(2.0),
            );
            ui.add_space(4.0);
            hint(
                ui,
                if home {
                    "Not tied to a repository. The agent starts in your home folder: parse files, run skills, odd jobs."
                } else {
                    "Ask anything. The agent works in this worktree and can read and file issues."
                },
            );
            ui.add_space(14.0);
            let fresh = self.fresh.as_mut().expect("fresh chat");
            let mut row = |ui: &mut Ui| {
                ui.allocate_ui(Vec2::new(180.0, KEY_H), |ui| {
                    if choice(
                        ui,
                        "fresh-agent",
                        &mut fresh.agent,
                        o.agents.iter().map(|a| {
                            let label = if a.available {
                                a.label.clone()
                            } else {
                                format!("{} (not ready)", a.label)
                            };
                            (a.name.clone(), label)
                        }),
                    ) {
                        fresh.model.clear();
                    }
                });
                let mut opts: Vec<(PlaceChoice, String)> = Vec::new();
                for w in trees.iter().filter(|_| !home) {
                    let label = if w.main { format!("{} (main)", w.label()) } else { w.label() };
                    opts.push((
                        if w.main { PlaceChoice::Main } else { PlaceChoice::Existing(w.id) },
                        label,
                    ));
                }
                if !home {
                    opts.push((PlaceChoice::New, "+ new worktree".into()));
                    ui.allocate_ui(Vec2::new(200.0, KEY_H), |ui| {
                        choice(ui, "fresh-tree", &mut fresh.place, opts);
                    });
                }
                if fresh.place == PlaceChoice::New {
                    ui.allocate_ui(Vec2::new(160.0, KEY_H), |ui| {
                        field(ui, &mut fresh.branch, "branch, e.g. fix-login");
                    });
                }
                let default = o
                    .agents
                    .iter()
                    .find(|a| a.name == fresh.agent)
                    .map(|a| a.about.clone())
                    .unwrap_or_default();
                let hint_text = match fresh.agent.as_str() {
                    "native" => format!("model, {}", short_about(&default)),
                    _ => "model, the agent's default if empty".to_string(),
                };
                let list = self.agent_models.get(&fresh.agent);
                model_input(ui, "fresh-model", &mut fresh.model, &hint_text, list);
            };
            if self.compact || self.narrow {
                ui.horizontal_wrapped(&mut row);
            } else {
                ui.horizontal(&mut row);
            }
        });
        ui.add_space(10.0);
        let agent_ready = o
            .agents
            .iter()
            .any(|a| self.fresh.as_ref().is_some_and(|f| f.agent == a.name) && a.available);
        let needs_branch = self
            .fresh
            .as_ref()
            .is_some_and(|f| f.place == PlaceChoice::New && f.branch.trim().is_empty());
        let hint_text = if !agent_ready {
            "that agent is not ready on the server"
        } else if needs_branch {
            "name the new worktree's branch first"
        } else {
            "what should it do"
        };
        let sent = self.composer(ui, agent_ready && !needs_branch, false, hint_text, None);
        if let (Some((text, attachments)), Some(f)) = (sent, self.fresh.clone()) {
            let place = match f.place {
                PlaceChoice::Main => Place::Main,
                PlaceChoice::Existing(id) => Place::Existing { worktree_id: id },
                PlaceChoice::New => Place::New { branch: f.branch.trim().to_string() },
            };
            let new = NewSession {
                project_id: f.project,
                agent: f.agent.clone(),
                place,
                title: None,
                model: Some(f.model.trim().to_string()).filter(|m| !m.is_empty()),
                issue_id: None,
                prompt: Some(text).filter(|t| !t.is_empty()),
                attachments,
            };
            if let Some(c) = &mut self.client {
                c.add_session(&new);
            }
            self.awaiting = Some(f.project);
        }
    }

    /// The name of the project a chat belongs to.
    pub(super) fn project_name(&self, id: Id) -> String {
        self.overview
            .as_ref()
            .and_then(|o| o.projects.iter().find(|p| p.id == id))
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    /// A chat's header, on one row, in a window, a tile or a phone: the status, the project and
    /// worktree as chips like the status box's, and the stop and close keys. The agent, model and
    /// tokens are on hover and in the status line.
    pub(super) fn chat_header(&mut self, ui: &mut Ui, s: &Session, tree: Option<&Worktree>) {
        let project = self.project_name(s.project_id);
        let home = self.is_home(s.project_id);
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(Key::new("close").outline(FAULT))
                    .on_hover_text("stop it and delete this chat")
                    .clicked()
                {
                    self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
                }
                if s.status.busy()
                    && ui.add(Key::new("stop").outline(FAULT)).clicked()
                    && let Some(c) = &mut self.client
                {
                    c.interrupt(s.id);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let (_, word) = status_look(s.status);
                    lamp(
                        ui,
                        word,
                        s.status != SessionStatus::Stopped,
                        s.status == SessionStatus::Failed,
                    );
                    let about = format!(
                        "{}\n{} {}{}\n{} in, {} out",
                        s.title,
                        s.agent,
                        s.model.as_deref().unwrap_or_default(),
                        s.issue_id.map(|i| format!("\nissue #{i}")).unwrap_or_default(),
                        tokens(s.usage.input_tokens + s.usage.cache_read_tokens),
                        tokens(s.usage.output_tokens)
                    );
                    chip(ui, &project, READOUT, true).on_hover_text(&about);
                    if let Some(w) = tree.filter(|_| !home) {
                        chip(ui, &w.label(), VALUE, false).on_hover_text(&about);
                    }
                });
            });
        });
    }

    pub(super) fn session_view(&mut self, ui: &mut Ui, s: Session) {
        let tree = self.worktree(s.worktree_id).cloned();
        self.chat_header(ui, &s, tree.as_ref());
        self.session_body(ui, s, tree);
    }

    pub(super) fn session_body(&mut self, ui: &mut Ui, s: Session, tree: Option<Worktree>) {
        ui.add_space(6.0);

        // The agent's current plan stays up top while the transcript scrolls under it.
        if let Some(plan) = self.transcripts.get(&s.id).and_then(|t| latest_plan(&t.messages)) {
            plan_panel(ui, s.id, &plan, !(self.narrow || self.compact));
        }

        // The composer, and the row of attachments above it when there are any.
        let files = self.focused && !self.tray.files.is_empty();
        let input_h = self.composer_h.get(&self.drawing).copied().unwrap_or_else(|| {
            (if self.compact { 84.0 } else { 100.0 }) + if files { 40.0 } else { 0.0 }
        }) + 6.0;
        let avail = (ui.available_height() - input_h).max(120.0);
        // Taken out while the transcript is drawn, so drawing it can borrow the app and nothing
        // is cloned; put back below.
        let (messages, mut heights) = {
            let t = self.transcripts.entry(s.id).or_default();
            (std::mem::take(&mut t.messages), std::mem::take(&mut t.heights))
        };
        // The transcript gets a rect of its own and is clipped to it: something that cannot
        // wrap (a long link, a table) widens the ui it is drawn in, and in a tile that would
        // push the composer and the tile's edge along with it.
        let rect = egui::Rect::from_min_size(
            ui.cursor().min,
            Vec2::new(ui.available_width(), avail.min(ui.available_height())),
        );
        let mut inner = ui
            .new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)));
        inner.set_clip_rect(rect.intersect(ui.clip_rect()));
        let jump = self.jump_asked(ui);
        if jump {
            self.follow = true;
        }
        let scroll = egui::ScrollArea::vertical()
            .max_height(rect.height())
            .auto_shrink([false, false])
            .stick_to_bottom(self.follow)
            .show(&mut inner, |ui| {
                ui.set_max_width(ui.available_width());
                if messages.is_empty() {
                    hint(ui, "no messages yet");
                }
                // Heights hold for one width and one look of the entries.
                heights.keep_for(
                    (
                        ui.available_width().to_bits(),
                        self.thinking as u32 | (self.face.size.to_bits() << 1),
                        self.fonts_loaded,
                    ),
                    ui.available_width(),
                );
                let visible = ui.clip_rect().expand2(Vec2::new(0.0, OVERSCAN));
                for m in &messages {
                    // Each entry gets a ui of the column's width, clipped to it, so one with
                    // something that cannot wrap does not widen the entries after it.
                    let column = egui::Rect::from_min_size(
                        ui.cursor().min,
                        Vec2::new(ui.available_width(), f32::INFINITY),
                    );
                    // An entry that was measured and is out of sight is not laid out, only
                    // given the room it took.
                    if let Some(h) = heights.known(m.id)
                        && !(egui::Rect::from_min_size(column.min, Vec2::new(column.width(), h)))
                            .intersects(visible)
                    {
                        ui.allocate_space(Vec2::new(column.width(), h));
                        ui.add_space(6.0);
                        continue;
                    }
                    let mut one = ui.new_child(
                        egui::UiBuilder::new()
                            .id(ui.id().with(("entry", m.id)))
                            .max_rect(column)
                            .layout(Layout::top_down(Align::Min)),
                    );
                    one.set_clip_rect(
                        egui::Rect::from_min_max(
                            column.min,
                            egui::pos2(column.right(), f32::INFINITY),
                        )
                        .intersect(ui.clip_rect()),
                    );
                    self.entry(&mut one, m, s.id);
                    let used = one.min_rect();
                    let h = (used.bottom().max(column.top()) - column.top()).max(0.0);
                    heights.measured(m.id, h);
                    ui.allocate_rect(
                        egui::Rect::from_min_max(
                            column.min,
                            egui::pos2(column.right(), column.top() + h),
                        ),
                        Sense::hover(),
                    );
                    ui.add_space(6.0);
                }
                for (i, text) in s.queued.iter().enumerate() {
                    self.you(ui, -1 - i as Id, 0, text, &[], s.id, true);
                }
                if jump {
                    ui.scroll_to_cursor(Some(Align::BOTTOM));
                }
            });
        if let Some(t) = self.transcripts.get_mut(&s.id) {
            // Messages that arrived while drawing cannot happen, but keep anything that did.
            let arrived = std::mem::replace(&mut t.messages, messages);
            t.messages.extend(arrived);
            t.heights = heights;
        }
        ui.allocate_rect(rect, Sense::hover());
        let at_bottom =
            scroll.state.offset.y + scroll.inner_rect.height() >= scroll.content_size.y - 4.0;
        self.follow = at_bottom;
        if !at_bottom {
            self.latest_key(ui, scroll.inner_rect);
        }

        ui.add_space(6.0);
        let missing = tree.as_ref().is_some_and(|w| w.missing);
        let hint_text = if missing {
            "this worktree is gone, the session can only be read"
        } else if s.status == SessionStatus::Waiting {
            "answer the question above, or say something else"
        } else if s.status.busy() {
            "steer the agent while it works"
        } else {
            "tell the agent what to do next"
        };
        if let Some((text, files)) =
            self.composer(ui, !missing, s.status.busy(), hint_text, Some(&s))
        {
            let slash = files.is_empty().then(|| agentbench_core::parse_slash(&text)).flatten();
            match slash.map(|(c, _)| c.name) {
                Some("new") => {
                    let tree = self.worktree(s.worktree_id).cloned();
                    self.new_chat(tree.as_ref());
                    if let Some(f) = &mut self.fresh {
                        f.agent = s.agent.clone();
                    }
                }
                Some("close") => self.confirm = Some(Confirm::Session(s.id, s.title.clone())),
                _ => {
                    if let Some(c) = &mut self.client {
                        c.send_with(s.id, &text, files);
                    }
                }
            }
        }
    }

    fn jump_asked(&mut self, ui: &Ui) -> bool {
        if self.jump.is_some() && self.jump == self.drawing {
            self.jump = None;
            return true;
        }
        let composer = egui::Id::new(("composer", self.drawing));
        let in_composer = ui.memory(|m| m.has_focus(composer));
        let typing = ui.ctx().text_edit_focused() && !(in_composer && self.draft.is_empty());
        self.focused
            && !typing
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::End))
    }

    fn latest_key(&mut self, ui: &mut Ui, transcript: egui::Rect) {
        let key = Key::new("latest").outline(VALUE).id_salt(("latest", self.drawing));
        let size = Vec2::new(key.width(ui), KEY_H);
        let at = transcript.right_bottom() - size - Vec2::new(18.0, 10.0);
        let r = key.paint(ui, egui::Rect::from_min_size(at, size));
        r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "latest"));
        if r.clicked() {
            self.jump = self.drawing;
            ui.ctx().request_repaint();
        }
    }
}
