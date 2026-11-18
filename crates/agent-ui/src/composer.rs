use crate::form::prose_id;
use crate::widgets::size;
use egui::{Response, Ui, Vec2};
use crate::key::Key;
use egui_bench::prelude::*;

pub struct Pending<'a> {
    pub name: &'a str,
    pub size: u64,
    pub preview: Option<&'a egui::TextureHandle>,
}

pub fn pending_row(ui: &mut Ui, files: &[Pending]) -> Option<usize> {
    let mut drop_at = None;
    ui.horizontal_wrapped(|ui| {
        for (i, f) in files.iter().enumerate() {
            let chip = ui.horizontal(|ui| {
                if let Some(tex) = f.preview {
                    let s = tex.size_vec2();
                    let h = 28.0;
                    ui.add(egui::Image::new((tex.id(), Vec2::new(s.x * h / s.y.max(1.0), h))));
                }
                ui.label(
                    egui::RichText::new(format!("{} {}", f.name, size(f.size)))
                        .font(mono(11.0))
                        .color(VALUE),
                );
                let x = ui.small_button("x");
                x.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::Role::Button, true, format!("remove {}", f.name))
                });
                x.clicked()
            });
            if chip.inner {
                drop_at = Some(i);
            }
            ui.add_space(8.0);
        }
    });
    drop_at
}

pub fn field_id(salt: impl std::hash::Hash + std::fmt::Debug) -> egui::Id {
    egui::Id::unique(("composer", salt))
}

pub fn text_box(
    ui: &mut Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    draft: &mut String,
    hint: &str,
    rows: usize,
) -> Response {
    let max = (ui.ctx().content_rect().height() * 0.3).clamp(60.0, 260.0);
    let r = prose_id(ui, ("composer", salt), draft, hint, rows, max);
    if hint.is_empty() {
        ui.ctx().accesskit_node_builder(r.id, |node| node.set_label("message"));
    }
    r
}

pub fn recall(ui: &Ui, id: egui::Id, draft: &mut String, sent: &[&str]) {
    if !ui.memory(|m| m.has_focus(id)) {
        return;
    }
    let typed = draft.trim_end();
    let at = if typed.is_empty() { None } else { sent.iter().position(|t| *t == typed) };
    let up = egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::ArrowUp);
    let down = egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::ArrowDown);
    let next = if !typed.is_empty() && at.is_none() {
        return;
    } else if ui.input_mut(|i| i.consume_shortcut(&up)) {
        at.map_or(0, |n| n + 1)
    } else if at.is_some() && ui.input_mut(|i| i.consume_shortcut(&down)) {
        match at {
            Some(0) | None => {
                draft.clear();
                return;
            }
            Some(n) => n - 1,
        }
    } else {
        return;
    };
    let Some(text) = sent.get(next) else { return };
    *draft = text.to_string();
    let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
    let end = egui::text::CCursor::new(draft.chars().count());
    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
    state.store(ui.ctx(), id);
}

pub fn entered(ui: &Ui, edit: &Response, draft: &mut String) -> bool {
    let enter = edit.has_focus()
        && ui.input(|i| {
            i.key_pressed(egui::Key::Enter) && (i.modifiers.command || !i.modifiers.shift)
        });
    if enter && draft.ends_with('\n') && !ui.input(|i| i.modifiers.command) {
        let trimmed = draft.trim_end_matches('\n').to_string();
        *draft = trimmed;
    }
    enter
}

pub fn send_key(ui: &mut Ui, label: &str, can: bool, width: f32) -> bool {
    let key = ui.add(Key::new(label).solid(READOUT).enabled(can).min_width(width));
    key.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, can, label));
    key.clicked()
}

pub fn stop_key(ui: &mut Ui) -> bool {
    let key = ui.add(Key::new("stop").outline(FAULT));
    key.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, "stop"));
    key.clicked()
}

pub fn copy_pressed(ui: &Ui) -> bool {
    ui.input(|i| {
        i.events.iter().any(|e| match e {
            egui::Event::Copy => true,
            egui::Event::Key { key: egui::Key::C, pressed: true, modifiers, .. } => {
                modifiers.command && !modifiers.shift
            }
            _ => false,
        })
    })
}

pub fn nothing_selected(ui: &Ui, id: egui::Id) -> bool {
    egui::TextEdit::load_state(ui.ctx(), id)
        .and_then(|state| state.cursor.char_range())
        .is_none_or(|range| range.primary == range.secondary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    struct Box {
        draft: String,
        sent: Vec<String>,
        history: Vec<String>,
        files: Vec<(String, u64)>,
    }

    fn harness(history: &[&str]) -> Harness<'static, Box> {
        harness_with(history, vec![("shot.png".into(), 2048)])
    }

    fn harness_with(history: &[&str], files: Vec<(String, u64)>) -> Harness<'static, Box> {
        let state = Box {
            draft: String::new(),
            sent: Vec::new(),
            history: history.iter().map(|s| s.to_string()).collect(),
            files,
        };
        Harness::builder().with_size([600.0, 300.0]).build_ui_state(
            |ui, b: &mut Box| {
                egui_bench::theme::install(ui.ctx());
                let pending: Vec<Pending> =
                    b.files.iter().map(|(n, s)| Pending { name: n, size: *s, preview: None }).collect();
                if let Some(i) = pending_row(ui, &pending) {
                    b.files.remove(i);
                }
                let sent: Vec<&str> = b.history.iter().map(String::as_str).collect();
                recall(ui, field_id("t"), &mut b.draft, &sent);
                let edit = text_box(ui, "t", &mut b.draft, "say something", 3);
                let enter = entered(ui, &edit, &mut b.draft);
                let can = !b.draft.trim().is_empty();
                if (send_key(ui, "send", can, 100.0) || enter) && can {
                    b.sent.push(b.draft.trim().to_string());
                    b.draft.clear();
                }
            },
            state,
        )
    }

    #[test]
    fn enter_sends_the_draft_and_clears_it() {
        let mut h = harness(&[]);
        h.run_steps(2);
        h.get_by_label("say something").click();
        h.run_steps(1);
        h.get_by_label("say something").type_text("hello there");
        h.run_steps(1);
        h.key_press(egui::Key::Enter);
        h.run_steps(2);
        assert_eq!(h.state().sent, vec!["hello there".to_string()]);
        assert!(h.state().draft.is_empty());
    }

    fn arrow(h: &mut Harness<'_, Box>, key: egui::Key) {
        h.event(egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        h.run_steps(2);
    }

    #[test]
    fn up_brings_back_the_last_message_and_down_clears_it() {
        let mut h = harness_with(&["newest", "older"], Vec::new());
        h.run_steps(2);
        h.get_by_label("say something").click();
        h.run_steps(1);
        arrow(&mut h, egui::Key::ArrowUp);
        assert_eq!(h.state().draft, "newest");
        arrow(&mut h, egui::Key::ArrowDown);
        assert!(h.state().draft.is_empty(), "{:?}", h.state().draft);
        h.event(egui::Event::Text("half typed".into()));
        h.run_steps(2);
        arrow(&mut h, egui::Key::ArrowUp);
        assert_eq!(h.state().draft, "half typed");
    }

    #[test]
    fn a_waiting_file_can_be_taken_off() {
        let mut h = harness(&[]);
        h.run_steps(2);
        h.get_by_label("remove shot.png").click();
        h.run_steps(2);
        assert!(h.state().files.is_empty());
    }

    #[test]
    fn an_empty_draft_cannot_be_sent() {
        let mut h = harness(&[]);
        h.run_steps(2);
        h.get_by_label("send").click();
        h.run_steps(2);
        assert!(h.state().sent.is_empty());
    }
}
