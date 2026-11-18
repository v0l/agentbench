use egui::{Margin, Sense, Stroke, Ui, Vec2};
use crate::key::Key;
use egui_bench::prelude::*;
use std::time::Duration;

pub fn spinner(ui: &Ui, center: egui::Pos2, radius: f32, tint: egui::Color32) {
    let t = ui.input(|i| i.time) as f32;
    let start = t * std::f32::consts::TAU * 1.2;
    let sweep = std::f32::consts::TAU * 0.7;
    let steps = 14;
    let points: Vec<egui::Pos2> = (0..=steps)
        .map(|i| {
            let a = start + sweep * i as f32 / steps as f32;
            center + Vec2::new(a.cos(), a.sin()) * radius
        })
        .collect();
    let p = ui.painter();
    p.circle_stroke(center, radius, Stroke::new(1.0, tint.gamma_multiply(0.2)));
    p.add(egui::Shape::line(points, Stroke::new(1.6, tint)));
    ui.ctx().request_repaint_after(Duration::from_millis(33));
}

pub fn rail_block(
    ui: &mut Ui,
    rail: egui::Color32,
    fill: egui::Color32,
    body: impl FnOnce(&mut Ui),
) {
    let r = egui::Frame::NONE
        .fill(fill)
        .inner_margin(Margin { left: 12, right: 8, top: 4, bottom: 4 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui);
        });
    let rect = r.response.rect;
    ui.painter().rect_filled(
        egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.left() + 3.0, rect.bottom())),
        0.0,
        rail,
    );
}

pub fn code_block(
    ui: &mut Ui,
    text: &str,
    colour: egui::Color32,
    salt: impl std::hash::Hash + std::fmt::Debug,
) {
    egui::Frame::NONE.fill(CHASSIS).corner_radius(2).inner_margin(Margin::symmetric(8, 5)).show(
        ui,
        |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt(salt)
                .max_height(320.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(text.trim_end()).font(mono(11.0)).color(colour),
                        )
                        .wrap(),
                    );
                });
        },
    );
}

pub fn option_row(
    ui: &mut Ui,
    label: &str,
    multiple: bool,
    on: bool,
    salt: impl std::hash::Hash + std::fmt::Debug,
) -> egui::Response {
    let (pad, mark) = (8.0, 22.0);
    let tint = if on { READOUT } else { VALUE };
    let width = ui.available_width();
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = ui.painter().layout(label.to_string(), font, tint, width - pad * 2.0 - mark);
    let tall = if crate::key::phone(ui.ctx()) { crate::key::TAP } else { 0.0 };
    let (rect, _) = ui
        .allocate_exact_size(Vec2::new(width, (galley.size().y + 10.0).max(tall)), Sense::hover());
    let resp = ui.interact(rect, ui.scope_id().with(salt), Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::Button, true, on, label.to_string())
    });
    if ui.is_rect_visible(rect) {
        let lit = on || resp.hovered();
        let fill = if on {
            READOUT.gamma_multiply(0.18)
        } else if resp.hovered() {
            ETCH.gamma_multiply(0.6)
        } else {
            WELL
        };
        let p = ui.painter();
        p.rect_filled(rect, 2.0, fill);
        p.rect_stroke(
            rect,
            2.0,
            Stroke::new(1.0, if lit { tint } else { ETCH }),
            egui::StrokeKind::Inside,
        );
        let edge = Stroke::new(1.0, if lit { tint } else { LEGEND });
        let at = egui::pos2(rect.left() + pad + 4.0, rect.center().y);
        if multiple {
            let tick = egui::Rect::from_center_size(at, Vec2::splat(9.0));
            p.rect_stroke(tick, 1.0, edge, egui::StrokeKind::Inside);
            if on {
                p.rect_filled(tick.shrink(2.0), 0.0, tint);
            }
        } else {
            p.circle_stroke(at, 4.5, edge);
            if on {
                p.circle_filled(at, 2.5, tint);
            }
        }
        let top = egui::pos2(rect.left() + pad + mark, rect.top() + 5.0);
        p.galley(top, galley, tint);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

pub fn read_link(ui: &mut Ui, reading: bool) -> egui::Response {
    let (label, tint) =
        if reading { ("■ stop reading", READOUT) } else { ("▶ read aloud", LEGEND) };
    let r = ui.add(
        egui::Label::new(egui::RichText::new(label).font(legend_font(10.0)).color(tint))
            .sense(egui::Sense::click()),
    );
    let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
    if r.hovered() {
        ui.painter().line_segment(
            [r.rect.left_bottom(), r.rect.right_bottom()],
            egui::Stroke::new(1.0, tint),
        );
    }
    r.on_hover_text("read this reply aloud with the server's voice")
}

pub fn press(ui: &mut Ui, label: &str, enabled: bool, key: Key) -> bool {
    let r = ui.add(key.enabled(enabled));
    r.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
    r.clicked()
}

pub fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

pub fn ago(ms: i64, now: i64) -> String {
    let s = ((now - ms) / 1000).max(0);
    match s {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_in_the_largest_whole_unit() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(4096), "4 KB");
        assert_eq!(size(3 << 20), "3.0 MB");
    }

    #[test]
    fn ages_round_down_to_the_unit() {
        assert_eq!(ago(0, 30_000), "just now");
        assert_eq!(ago(0, 5 * 60_000), "5m ago");
        assert_eq!(ago(0, 3 * 3_600_000), "3h ago");
        assert_eq!(ago(0, 2 * 86_400_000), "2d ago");
    }
}
