//! Small widgets and helpers shared by the views: the hamburger, the grid key, status lamps and badges, code blocks, and the text shorteners.

use super::*;


/// The three bars that open the project list on a narrow window.
pub(super) fn hamburger(ui: &mut Ui, open: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(34.0, 26.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "projects"));
    let tint = if open {
        READOUT
    } else if resp.hovered() {
        VALUE
    } else {
        LEGEND
    };
    let p = ui.painter();
    p.rect_filled(rect, 2.0, if open { tint.gamma_multiply(0.18) } else { WELL });
    p.rect_stroke(rect, 2.0, Stroke::new(1.0, if open || resp.hovered() { tint } else { ETCH }), egui::StrokeKind::Inside);
    for i in -1..=1 {
        let y = rect.center().y + i as f32 * 5.0;
        p.line_segment(
            [egui::pos2(rect.left() + 9.0, y), egui::pos2(rect.right() - 9.0, y)],
            Stroke::new(1.5, tint),
        );
    }
    resp
}

/// A key for one grid layout, drawn as the tiles it makes.
pub(super) fn grid_button(ui: &mut Ui, grid: Grid, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(26.0, 22.0), Sense::click());
    let resp = resp.on_hover_text(grid.about());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            true,
            on,
            format!("layout {}", grid.key()),
        )
    });
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let tint = if on {
        READOUT
    } else if resp.hovered() {
        VALUE
    } else {
        LEGEND
    };
    let p = ui.painter();
    p.rect_filled(rect, 2.0, if on { tint.gamma_multiply(0.18) } else { WELL });
    p.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, if on || resp.hovered() { tint } else { ETCH }),
        egui::StrokeKind::Inside,
    );
    let (cols, rows) = grid.dims();
    let art = rect.shrink2(Vec2::new(6.0, 5.0));
    let gap = 1.5;
    let (w, h) = (
        (art.width() - gap * (cols - 1) as f32) / cols as f32,
        (art.height() - gap * (rows - 1) as f32) / rows as f32,
    );
    for r in 0..rows {
        for c in 0..cols {
            let at = art.min + Vec2::new(c as f32 * (w + gap), r as f32 * (h + gap));
            p.rect_filled(egui::Rect::from_min_size(at, Vec2::new(w, h)), 0.5, tint);
        }
    }
    resp
}

/// A small spinning arc, for something that is working. Keeps the frame loop going while visible.
/// The colour and word a session status is shown with.
pub(super) fn status_look(status: SessionStatus) -> (egui::Color32, &'static str) {
    match status {
        SessionStatus::Running => (TRACE, "working"),
        SessionStatus::Waiting => (READOUT, "needs you"),
        SessionStatus::Failed => (FAULT, "failed"),
        SessionStatus::Stopped => (LEGEND, "stopped"),
        SessionStatus::Idle => (OK, "idle"),
    }
}

/// How many sessions are in each status, the ones that want attention first.
/// Statuses nobody is in are left out.
pub fn status_counts(sessions: &[Session]) -> Vec<(SessionStatus, usize)> {
    [
        SessionStatus::Waiting,
        SessionStatus::Failed,
        SessionStatus::Running,
        SessionStatus::Idle,
        SessionStatus::Stopped,
    ]
    .into_iter()
    .map(|st| (st, sessions.iter().filter(|s| s.status == st).count()))
    .filter(|(_, n)| *n > 0)
    .collect()
}

/// A chip per status across every project, so the whole fleet reads at a glance.
pub(super) fn status_badge(ui: &mut Ui, sessions: &[Session]) {
    let counts = status_counts(sessions);
    if counts.is_empty() {
        chip(ui, "no agents", LEGEND, false);
        return;
    }
    // Right to left, so add them reversed to read left to right.
    for (status, n) in counts.into_iter().rev() {
        let (tint, word) = status_look(status);
        chip(ui, &format!("{n} {word}"), tint, status != SessionStatus::Stopped)
            .on_hover_text(format!("{n} agent chat{} {word}", if n == 1 { "" } else { "s" }));
    }
}

pub(super) fn spinner(ui: &Ui, center: egui::Pos2, radius: f32, tint: egui::Color32) {
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

pub(super) fn rail_block(ui: &mut Ui, rail: egui::Color32, fill: egui::Color32, body: impl FnOnce(&mut Ui)) {
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

pub(super) fn code_block(
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

/// One choice on its own row, with a tick box for several picks or a dot for one.
pub(super) fn option_row(
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
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(width, galley.size().y + 10.0), Sense::hover());
    let resp = ui.interact(rect, ui.id().with(salt), Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, label.to_string())
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

/// A dialog's width: what it wants, but never wider than the screen less a margin.
pub(super) fn modal_width(ctx: &egui::Context, wanted: f32) -> f32 {
    wanted.min((ctx.content_rect().width() - 24.0).max(200.0))
}

pub(super) fn ago(ms: i64) -> String {
    let now = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    let s = ((now - ms) / 1000).max(0);
    match s {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

pub(super) fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

pub(super) fn short_about(about: &str) -> String {
    match about.split_once(" at ") {
        Some((model, _)) => format!("default {model}"),
        None => about.to_string(),
    }
}

pub(super) fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

/// The chat a `foreman://open?session=…` link names, as a notification's tap carries.
pub(super) fn chat_in_link(link: &str) -> Option<SessionId> {
    let query = link.trim().strip_prefix("foreman://open?")?;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("session="))
        .and_then(|v| v.parse().ok())
}
