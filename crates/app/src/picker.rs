//! A model picker in the manner of yalr's: a button showing the current model that opens a
//! dialog with a filter box and a long, scrolling, sorted list. The list is drawn by row so a
//! server with thousands of models costs the same as one with ten.

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Ui, Vec2};
use egui_bench::key::Key;
use egui_bench::prelude::*;

const ROW_H: f32 = 30.0;
const LIST_H: f32 = 380.0;
const WIDTH: f32 = 520.0;

#[derive(Clone, Default)]
struct State {
    open: bool,
    query: String,
    custom: bool,
    custom_text: String,
    fresh: bool,
}

pub struct ModelPicker<'a> {
    id: egui::Id,
    models: &'a [String],
    empty: Option<&'a str>,
    noun: &'a str,
}

impl<'a> ModelPicker<'a> {
    pub fn new(id: impl std::hash::Hash + std::fmt::Debug, models: &'a [String]) -> Self {
        ModelPicker { id: egui::Id::new(("model-picker", id)), models, empty: None, noun: "model" }
    }

    /// What the list holds, for the labels: "model" by default.
    pub fn noun(mut self, noun: &'a str) -> Self {
        self.noun = noun;
        self
    }

    /// Show a first row that clears the choice, labelled `label`.
    pub fn allow_empty(mut self, label: &'a str) -> Self {
        self.empty = Some(label);
        self
    }

    /// Draws the button, and the dialog when it is open. True when `value` changed.
    pub fn show(self, ui: &mut Ui, value: &mut String) -> bool {
        let mut state: State = ui.data(|d| d.get_temp(self.id)).unwrap_or_default();
        let mut changed = false;
        if self.trigger(ui, value) {
            state = State { open: true, fresh: true, ..State::default() };
        }
        if state.open {
            changed = self.dialog(ui.ctx(), &mut state, value);
        }
        ui.data_mut(|d| d.insert_temp(self.id, state));
        changed
    }

    fn trigger(&self, ui: &mut Ui, value: &str) -> bool {
        let w = ui.available_width().max(120.0);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, KEY_H), Sense::click());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        resp.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                true,
                format!("choose {}", self.noun),
            )
        });
        let edge = if resp.hovered() { READOUT_DIM } else { ETCH };
        let p = ui.painter();
        p.rect(rect, theme::RADIUS, WELL, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
        let (text, colour) = if value.is_empty() {
            (self.empty.map_or_else(|| format!("select {}", self.noun), str::to_string), LEGEND)
        } else {
            (value.to_string(), VALUE)
        };
        let label = rect.shrink2(Vec2::new(8.0, 0.0)).with_max_x(rect.right() - 26.0);
        one_line(p, label, &text, colour, Align2::LEFT_CENTER);
        chevrons(p, Pos2::new(rect.right() - 13.0, rect.center().y), LEGEND);
        resp.clicked()
    }

    fn dialog(&self, ctx: &egui::Context, st: &mut State, value: &mut String) -> bool {
        let mut picked: Option<String> = None;
        let mut close = false;

        let q = st.query.trim().to_lowercase();
        let rows: Vec<&String> =
            self.models.iter().filter(|m| q.is_empty() || m.to_lowercase().contains(&q)).collect();
        let known = self.models.iter().any(|m| m == value);
        let custom_now = !value.is_empty() && !known;

        let m = egui::Modal::new(self.id.with("dialog"))
            .backdrop_color(Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(WIDTH);
                modal_title(ui, &format!("Select {}", self.noun));

                let filter = field(ui, &mut st.query, &format!("filter {}s…", self.noun));
                if st.fresh {
                    filter.request_focus();
                    st.fresh = false;
                }
                let enter = filter.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if enter && let Some(first) = rows.first() {
                    picked = Some((*first).clone());
                }
                ui.add_space(6.0);

                // The fixed rows, then the models. All one scrolling list.
                let lead = usize::from(self.empty.is_some()) + 1;
                let total = rows.len() + lead;
                egui::ScrollArea::vertical()
                    .max_height(LIST_H)
                    .auto_shrink([false, true])
                    .show_rows(ui, ROW_H, total, |ui, range| {
                        for i in range {
                            let (label, id, tag) = match i {
                                0 if self.empty.is_some() => {
                                    (self.empty.unwrap_or_default(), None, value.is_empty())
                                }
                                i if i + 1 == lead => {
                                    ("custom…", Some(None), custom_now || st.custom)
                                }
                                i => {
                                    let m = rows[i - lead].as_str();
                                    (m, Some(Some(m)), m == value.as_str())
                                }
                            };
                            let is_empty = id.is_none();
                            let is_custom = matches!(id, Some(None));
                            let dim = is_empty || is_custom;
                            if entry(ui, label, dim, tag, is_custom && custom_now, value) {
                                match id {
                                    None => picked = Some(String::new()),
                                    Some(None) => st.custom = !st.custom,
                                    Some(Some(m)) => picked = Some(m.to_string()),
                                }
                            }
                        }
                    });
                if rows.is_empty() && !self.models.is_empty() {
                    ui.add_space(6.0);
                    hint(ui, "nothing matches. Use custom… to type the name as is.");
                }
                if st.custom {
                    ui.add_space(8.0);
                    let r = field(ui, &mut st.custom_text, &format!("{} name…", self.noun));
                    let go = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let name = st.custom_text.trim().to_string();
                        let ok = !name.is_empty();
                        if ui
                            .add(Key::new("use").solid(READOUT).enabled(ok).min_width(80.0))
                            .clicked()
                            || (go && ok)
                        {
                            picked = Some(name);
                        }
                        if ui.add(Key::new("cancel").outline(VALUE).min_width(80.0)).clicked() {
                            st.custom = false;
                        }
                    });
                }

                footer(ui, |ui| {
                    close = ui.add(Key::new("close").outline(VALUE).min_width(100.0)).clicked();
                    let n = self.models.len();
                    let shown = if q.is_empty() {
                        format!("{n} {}s", self.noun)
                    } else {
                        format!("{} of {n} {}s", rows.len(), self.noun)
                    };
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.label(legend(shown));
                    });
                });
            });
        if m.should_close() {
            close = true;
        }
        let mut changed = false;
        if let Some(p) = picked {
            changed = *value != p;
            *value = p;
            close = true;
        }
        if close {
            *st = State::default();
        }
        changed
    }
}

/// One line of the list: the name truncated to fit, ACTIVE on the chosen one.
fn entry(ui: &mut Ui, label: &str, dim: bool, active: bool, show_value: bool, value: &str) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_H), Sense::click());
    let p = ui.painter();
    if active {
        p.rect_filled(rect, 0.0, READOUT.gamma_multiply(0.09));
        p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())), 0.0, READOUT);
    } else if resp.hovered() {
        p.rect_filled(rect, 0.0, BAND);
    }
    p.line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(1.0, ETCH.gamma_multiply(0.5)),
    );
    let tag = if show_value {
        value.to_string()
    } else if active {
        "ACTIVE".to_string()
    } else {
        String::new()
    };
    let tag_w =
        if tag.is_empty() { 0.0 } else { (tag.chars().count() as f32 * 7.0).min(220.0) + 8.0 };
    let name = Rect::from_min_max(
        Pos2::new(rect.left() + 12.0, rect.top()),
        Pos2::new(rect.right() - 10.0 - tag_w, rect.bottom()),
    );
    one_line(p, name, label, if dim { LEGEND } else { VALUE }, Align2::LEFT_CENTER);
    if !tag.is_empty() {
        let at = Rect::from_min_max(
            Pos2::new(rect.right() - 10.0 - tag_w, rect.top()),
            rect.right_bottom() - Vec2::new(10.0, 0.0),
        );
        one_line(p, at, &tag, READOUT, Align2::RIGHT_CENTER);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Text on one line inside `area`, cut with an ellipsis rather than overflowing.
fn one_line(p: &egui::Painter, area: Rect, text: &str, colour: Color32, align: Align2) {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), figure(12.5), colour);
    job.wrap = egui::text::TextWrapping {
        max_width: area.width().max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = p.layout_job(job);
    let x = match align {
        Align2::RIGHT_CENTER => area.right() - galley.size().x,
        _ => area.left(),
    };
    let at = Pos2::new(x, area.center().y - galley.size().y / 2.0);
    p.galley(at, galley, colour);
}

/// The up-and-down pair that marks a picker.
fn chevrons(p: &egui::Painter, c: Pos2, colour: Color32) {
    let s = Stroke::new(1.3, colour);
    for dir in [-1.0f32, 1.0] {
        let tip = Pos2::new(c.x, c.y + dir * 6.0);
        let base = c.y + dir * 2.5;
        p.line_segment([Pos2::new(c.x - 3.5, base), tip], s);
        p.line_segment([Pos2::new(c.x + 3.5, base), tip], s);
    }
}
