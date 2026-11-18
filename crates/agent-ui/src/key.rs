use egui::{Color32, Response, Ui, Vec2, Widget};
use egui_bench::key::{KEY_H, Key as Inner};

pub const PHONE_WIDTH: f32 = 720.0;
pub const TAP: f32 = 44.0;

pub fn phone(ctx: &egui::Context) -> bool {
    ctx.content_rect().width() < PHONE_WIDTH || ctx.content_rect().height() < 480.0
}

pub struct Key {
    inner: Inner,
    label_empty: bool,
}

impl Key {
    pub fn new(label: impl Into<String>) -> Key {
        let label = label.into();
        Key { label_empty: label.is_empty(), inner: Inner::new(label) }
    }

    pub fn more() -> Key {
        Key { inner: Inner::more(), label_empty: true }
    }

    pub fn solid(mut self, tint: Color32) -> Key {
        self.inner = self.inner.solid(tint);
        self
    }

    pub fn outline(mut self, tint: Color32) -> Key {
        self.inner = self.inner.outline(tint);
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Key {
        self.inner = self.inner.enabled(enabled);
        self
    }

    pub fn min_width(mut self, width: f32) -> Key {
        self.inner = self.inner.min_width(width);
        self
    }

    pub fn id_salt(mut self, salt: impl std::hash::Hash + std::fmt::Debug) -> Key {
        self.inner = self.inner.id_salt(salt);
        self
    }

    pub fn width(&self, ui: &Ui) -> f32 {
        self.inner.width(ui)
    }

    pub fn paint(self, ui: &mut Ui, rect: egui::Rect) -> Response {
        self.inner.paint(ui, rect)
    }
}

impl Widget for Key {
    fn ui(self, ui: &mut Ui) -> Response {
        if !phone(ui.ctx()) {
            return ui.add(self.inner);
        }
        let mut w = self.inner.width(ui).min(egui_bench::text::room(ui));
        if self.label_empty {
            w = w.max(TAP);
        }
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, TAP.max(KEY_H)), egui::Sense::hover());
        self.inner.paint(ui, rect)
    }
}
