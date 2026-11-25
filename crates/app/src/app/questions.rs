use super::*;
use agentbench_core::{LogItem, LogQuery, SupervisorLog};

const LOG_MAX_H: f32 = 220.0;
const LOG_MAX_H_NARROW: f32 = 170.0;
const REFRESH: Duration = Duration::from_secs(15);
const KINDS: [(&str, &str); 6] =
    [("", "any kind"), ("user", "you"), ("question", "workers"), ("judge", "judge"), ("check_in", "check in"), ("watch,groom,judge_issues,process,main_dirty", "other")];
const STATES: [&str; 3] = ["all", "open", "closed"];

#[derive(Default)]
pub(super) struct QuestionsLog {
    pub open: bool,
    kind: String,
    project: Option<Id>,
    state: String,
    search: String,
    answered_only: bool,
    data: Option<SupervisorLog>,
    asked: Option<Instant>,
    seen_cursor: Option<Id>,
}

impl QuestionsLog {
    pub(super) fn opened(open: bool) -> QuestionsLog {
        QuestionsLog { open, ..Default::default() }
    }

    fn query(&self) -> LogQuery {
        let state = self.state.as_str();
        LogQuery {
            kind: Some(self.kind.clone()).filter(|k| !k.is_empty()),
            project: self.project.map(|p| p.to_string()),
            state: Some(state.to_string()).filter(|s| *s == "open" || *s == "closed"),
            q: Some(self.search.trim().to_string()).filter(|q| !q.is_empty()),
            limit: Some(if self.open { 100 } else { 1 }),
            needs_answer: self.answered_only.then_some(true),
            ..Default::default()
        }
    }

    pub(super) fn arrived(&mut self, q: LogQuery, log: SupervisorLog) {
        if q == self.query() || self.data.is_none() {
            self.data = Some(log);
        } else if let Some(d) = &mut self.data {
            d.open = log.open;
        }
    }

    fn stale(&mut self) {
        self.asked = None;
    }
}

pub fn log_order(items: &[LogItem]) -> Vec<&LogItem> {
    let mut out: Vec<&LogItem> = items.iter().collect();
    out.sort_by_key(|i| (!i.open(), if i.open() { i.arrived_at } else { -i.id }));
    out
}

fn question_line(i: &LogItem, project: &str) -> String {
    let mut whereabouts = String::new();
    if !project.is_empty() {
        whereabouts.push_str(&format!(", {project}"));
    }
    if let Some(n) = i.issue_id {
        whereabouts.push_str(&format!(" #{n}"));
    }
    let question = i.question.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{}{whereabouts}: {question}", i.who())
}

fn answer_line(i: &LogItem) -> String {
    if i.open() {
        return format!("waiting {}", agentbench_core::span(agentbench_core::now_ms() - i.arrived_at));
    }
    let a = i.answer();
    if a.is_empty() { "closed".into() } else { a.split_whitespace().collect::<Vec<_>>().join(" ") }
}

impl App {
    pub fn questions_log(&self) -> Option<&SupervisorLog> {
        self.questions.data.as_ref()
    }

    pub fn questions_open(&self) -> usize {
        self.questions.data.as_ref().map_or(0, |d| d.open)
    }

    pub(super) fn questions_fetch(&mut self) {
        let Some(c) = &mut self.client else { return };
        if !c.supervisor_log_supported() {
            return;
        }
        let changed = self.cursor != self.questions.seen_cursor;
        let due = self.questions.asked.is_none_or(|t| t.elapsed() >= REFRESH);
        if !(changed || due) {
            return;
        }
        self.questions.seen_cursor = self.cursor;
        self.questions.asked = Some(Instant::now());
        c.supervisor_log(&self.questions.query());
    }

    pub(super) fn questions_panel(&mut self, ui: &mut Ui, session: SessionId) {
        if !self.client.as_ref().is_some_and(|c| c.supervisor_log_supported()) {
            return;
        }
        self.questions_fetch();
        if !self.questions.open {
            return;
        }
        let open_count = self.questions_open();
        let mut toggle = false;
        let mut refetch = false;
        let mut jump_to: Option<i64> = None;
        let narrow = self.narrow;
        let projects: Vec<(Id, String)> = self
            .overview
            .iter()
            .flat_map(|o| &o.projects)
            .filter(|p| !p.builtin())
            .map(|p| (p.id, p.name.clone()))
            .collect();
        ui.add_space(4.0);
        section_panel(ui, |ui| {
            ui.horizontal(|ui| {
                let arrow = ui.add(Key::new("hide").outline(VALUE));
                arrow.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "hide questions log"));
                toggle = arrow.clicked();
                let count = match open_count {
                    0 => "nothing open".to_string(),
                    n => format!("{n} open"),
                };
                let badge = Line::new()
                    .legend("questions log")
                    .size(10.0)
                    .tint(LEGEND)
                    .gap(8.0)
                    .value(&count)
                    .size(11.0)
                    .tint(if open_count > 0 { READOUT } else { VALUE })
                    .elided(ui);
                badge.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, format!("questions log panel, {count}")));
            });
            let q = &mut self.questions;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
                for s in STATES {
                    let on = q.state == s || (q.state.is_empty() && s == "all");
                    let r = tab_chip(ui, s, READOUT, on);
                    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, on, format!("log state {s}")));
                    if r.clicked() && !on {
                        q.state = s.to_string();
                        refetch = true;
                    }
                }
                ui.add_space(6.0);
                for (k, label) in KINDS {
                    let on = q.kind == k;
                    let r = tab_chip(ui, label, TRACE, on);
                    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, on, format!("log kind {label}")));
                    if r.clicked() && !on {
                        q.kind = k.to_string();
                        refetch = true;
                    }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
                let shown = q.project.and_then(|p| projects.iter().find(|(id, _)| *id == p)).map_or("every project", |(_, n)| n.as_str());
                let combo = egui::ComboBox::from_id_salt("log-project").selected_text(shown).width(130.0).show_ui(ui, |ui| {
                    if ui.selectable_label(q.project.is_none(), "every project").clicked() {
                        q.project = None;
                        refetch = true;
                    }
                    for (id, name) in &projects {
                        if ui.selectable_label(q.project == Some(*id), name).clicked() {
                            q.project = Some(*id);
                            refetch = true;
                        }
                    }
                });
                combo.response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::ComboBox, true, "log project"));
                let width = if narrow { (ui.available_width() - 8.0).max(120.0) } else { 220.0 };
                let edit = ui.add(egui::TextEdit::singleline(&mut q.search).hint_text("search questions and answers").desired_width(width));
                crate::system::ime_field(&edit, &q.search);
                edit.widget_info(|| egui::WidgetInfo::labeled(egui::Role::TextInput, true, "log search"));
                if edit.changed() {
                    refetch = true;
                }
                let r = tap_toggle(ui, "needs an answer", q.answered_only);
                r.widget_info(|| egui::WidgetInfo::selected(egui::Role::CheckBox, true, q.answered_only, "log needs an answer"));
                if r.clicked() {
                    q.answered_only = !q.answered_only;
                    refetch = true;
                }
            });
            let Some(data) = &q.data else {
                hint(ui, "loading the log");
                return;
            };
            if data.items.is_empty() {
                hint(ui, "nothing in the log matches");
                return;
            }
            let names: HashMap<Id, &str> = projects.iter().map(|(id, n)| (*id, n.as_str())).collect();
            egui::ScrollArea::vertical()
                .id_salt("questions_scroll")
                .max_height(if narrow { LOG_MAX_H_NARROW } else { LOG_MAX_H })
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for item in log_order(&data.items) {
                        let project = item
                            .project_id
                            .and_then(|p| names.get(&p).copied())
                            .unwrap_or(item.project.as_str());
                        let row = ui.scope(|ui| {
                            ui.set_width(ui.available_width());
                            let asked = Line::new()
                                .legend(item.kind_label())
                                .size(10.0)
                                .tint(LEGEND)
                                .gap(6.0)
                                .value(if narrow { agentbench_core::supervisor_log::clip_chars(&question_line(item, project), 110).0 } else { question_line(item, project) })
                                .size(12.0)
                                .tint(VALUE);
                            if narrow {
                                asked.wrapped(ui);
                            } else {
                                asked.elided(ui);
                            }
                            Line::new()
                                .note(answer_line(item))
                                .size(11.0)
                                .tint(if item.open() { READOUT } else { TRACE })
                                .elided(ui);
                        });
                        let r = ui.interact(row.response.rect, ui.scope_id().with(("log", item.id)), Sense::click());
                        let spoken = format!("log item {}: {} {}. {}", item.id, item.kind_label(), question_line(item, project), answer_line(item));
                        r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, spoken.clone()));
                        if r.hovered() {
                            ui.painter().rect_stroke(row.response.rect.expand(1.0), 2.0, Stroke::new(1.0, ETCH), egui::StrokeKind::Outside);
                        }
                        if r.clicked() {
                            jump_to = Some(item.arrived_at);
                        }
                        ui.add_space(4.0);
                    }
                });
        });
        ui.add_space(4.0);
        if toggle {
            self.toggle_questions();
        }
        if refetch {
            self.questions.stale();
            self.questions.seen_cursor = None;
            self.questions_fetch();
        }
        if let Some(at) = jump_to {
            self.seek_transcript(session, at);
        }
    }

    pub(super) fn toggle_questions(&mut self) {
        self.questions.open = !self.questions.open;
        self.questions.stale();
        self.questions.seen_cursor = None;
        self.questions_fetch();
    }

    pub(super) fn seek_transcript(&mut self, session: SessionId, at: i64) {
        self.follow = false;
        let t = self.transcripts.entry(session).or_default();
        t.seek = Some(at);
    }

    pub fn transcript_seeking(&self, session: SessionId) -> bool {
        self.transcripts.get(&session).is_some_and(|t| t.seek.is_some())
    }

    pub fn last_seek_target(&self, session: SessionId) -> Option<Id> {
        self.transcripts.get(&session).and_then(|t| t.sought)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_items_sort_first_oldest_first_then_newest_closed() {
        let item = |id, at, closed: Option<i64>| LogItem { id, arrived_at: at, closed_at: closed, ..Default::default() };
        let items = vec![item(4, 40, None), item(3, 30, Some(31)), item(2, 20, None), item(1, 10, Some(11))];
        let order: Vec<Id> = log_order(&items).iter().map(|i| i.id).collect();
        assert_eq!(order, vec![2, 4, 3, 1]);
    }
}
