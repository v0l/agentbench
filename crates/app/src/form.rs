//! Text inputs drawn in the window's own typeface.
//!
//! `egui_bench`'s fields use its figure font, which is the code font. Here they use the
//! proportional family, so they follow the font chosen in settings like the rest of the text.

use egui::{Response, Stroke, Ui};
use egui_bench::prelude::*;

/// The size of the text inside an input, relative to [`crate::fonts::BASE`].
const SIZE: f32 = 13.0;

pub fn field(ui: &mut Ui, text: &mut String, hint: &str) -> Response {
    field_of(ui, text, hint, false, 1, f32::INFINITY, None, f32::INFINITY)
}

/// A one line field that leaves `reserve` points for whatever `after` adds beside it.
pub fn field_then(
    ui: &mut Ui,
    text: &mut String,
    hint: &str,
    reserve: f32,
    after: impl FnOnce(&mut Ui),
) -> Response {
    let w = (ui.available_width() - reserve).max(60.0);
    let r = field_of(ui, text, hint, false, 1, w, None, f32::INFINITY);
    after(ui);
    r
}

pub fn secret(ui: &mut Ui, text: &mut String) -> Response {
    field_of(ui, text, "", true, 1, f32::INFINITY, None, f32::INFINITY)
}

pub fn prose(ui: &mut Ui, text: &mut String, hint: &str, rows: usize) -> Response {
    field_of(ui, text, hint, false, rows, f32::INFINITY, None, f32::INFINITY)
}

/// A multi line input with an id of its own. Without one egui numbers a field by the widgets
/// drawn before it, so one that appears above (an attachment row) takes its focus away.
pub fn prose_id(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    text: &mut String,
    hint: &str,
    rows: usize,
    max_height: f32,
) -> Response {
    field_of(ui, text, hint, false, rows, f32::INFINITY, Some(egui::Id::new(id)), max_height)
}

fn field_of(
    ui: &mut Ui,
    text: &mut String,
    hint: &str,
    secret: bool,
    rows: usize,
    width: f32,
    id: Option<egui::Id>,
    max_height: f32,
) -> Response {
    let font = egui::FontId::proportional(SIZE);
    let hint = egui::RichText::new(hint).font(font.clone()).color(LEGEND.gamma_multiply(0.6));
    egui::Frame::NONE
        .fill(WELL)
        .stroke(Stroke::new(1.0, ETCH))
        .corner_radius(theme::RADIUS)
        .inner_margin(egui::Margin { left: 6, right: 6, top: 3, bottom: 3 })
        .show(ui, |ui| {
            let mut edit = if rows > 1 {
                egui::TextEdit::multiline(text).desired_rows(rows)
            } else {
                egui::TextEdit::singleline(text)
            };
            if let Some(id) = id {
                edit = edit.id(id);
            }
            edit = edit
                .frame(egui::Frame::NONE)
                .font(font)
                .text_color(VALUE)
                .hint_text(hint)
                .password(secret)
                .desired_width(if width.is_finite() { width - 12.0 } else { width });
            if rows > 1 && max_height.is_finite() {
                // Long text scrolls inside the box instead of growing it over the keys below.
                egui::ScrollArea::vertical()
                    .max_height(max_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| clipboard_menu(ui.add(edit)))
                    .inner
            } else {
                clipboard_menu(ui.add(edit))
            }
        })
        .inner
}
