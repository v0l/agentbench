//! Small widgets and helpers shared by the views: the hamburger, the grid key, status lamps and badges, code blocks, and the text shorteners.

use super::*;

/// The three bars that open the project list on a narrow window.
pub(super) fn hamburger(ui: &mut Ui, open: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(44.0, 44.0), Sense::click());
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
    p.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, if open || resp.hovered() { tint } else { ETCH }),
        egui::StrokeKind::Inside,
    );
    for i in -1..=1 {
        let y = rect.center().y + i as f32 * 5.0;
        p.line_segment(
            [egui::pos2(rect.left() + 9.0, y), egui::pos2(rect.right() - 9.0, y)],
            Stroke::new(1.5, tint),
        );
    }
    resp
}

/// The chat grid key: shows columns x rows and opens a board to pick a size by hovering over it.
pub(super) fn grid_picker(ui: &mut Ui, grid: Grid) -> Option<Grid> {
    let (cols, rows) = grid.dims();
    let key = ui.add(Key::new(format!("{cols} x {rows}")).outline(VALUE).min_width(64.0));
    key.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "layout"));
    let mut picked = None;
    egui::Popup::from_toggle_button_response(&key).show(|ui| {
        let cell = 20.0;
        let gap = 3.0;
        let side = Grid::MAX_SIDE;
        let span = side as f32 * (cell + gap) - gap;
        let (rect, resp) = ui.allocate_exact_size(Vec2::splat(span), Sense::click());
        let over = resp.hover_pos().map(|p| {
            let at = |v: f32| (((v / (cell + gap)) as usize) + 1).min(side);
            (at(p.x - rect.left()), at(p.y - rect.top()))
        });
        let (hc, hr) = over.unwrap_or((cols, rows));
        let painter = ui.painter();
        for r in 0..side {
            for c in 0..side {
                let on = c < hc && r < hr;
                let at = rect.min + Vec2::new(c as f32 * (cell + gap), r as f32 * (cell + gap));
                let cell_rect = egui::Rect::from_min_size(at, Vec2::splat(cell));
                painter.rect_filled(
                    cell_rect,
                    2.0,
                    if on { READOUT.gamma_multiply(0.8) } else { WELL },
                );
                painter.rect_stroke(
                    cell_rect,
                    2.0,
                    Stroke::new(1.0, if on { READOUT } else { ETCH }),
                    egui::StrokeKind::Inside,
                );
            }
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("{hc} x {hr}  ({} chats)", hc * hr))
                .size(11.5)
                .color(VALUE),
        );
        if resp.clicked() {
            picked = Some(Grid::new(hc, hr));
            ui.close();
        }
    });
    picked
}

pub(super) fn status_look(status: SessionStatus) -> (egui::Color32, &'static str) {
    match status {
        SessionStatus::Running => (TRACE, "working"),
        SessionStatus::Waiting => (READOUT, "needs you"),
        SessionStatus::Failed => (FAULT, "failed"),
        SessionStatus::Stopped => (LEGEND, "stopped"),
        SessionStatus::Idle => (OK, "idle"),
        SessionStatus::Unknown => (LEGEND, "unknown"),
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
        SessionStatus::Unknown,
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

pub fn usage_labels(report: &agentbench_core::UsageReport, project: Option<agentbench_core::Id>, now: i64) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = report
        .quotas
        .iter()
        .filter(|q| !q.expired(now))
        .map(|q| (q.label(now), q.level(now) > 0 || q.exhausted_until(now).is_some()))
        .collect();
    if report.spend.total_usd > 0.0 {
        let mine = project
            .and_then(|id| report.spend.projects.iter().find(|p| p.project_id == id))
            .map_or(0.0, |p| p.total_usd);
        out.push((
            format!("${:.2} today, ${mine:.2} project", report.spend.today_usd),
            false,
        ));
    }
    out
}

pub(super) fn usage_badges(ui: &mut Ui, report: Option<&agentbench_core::UsageReport>, project: Option<agentbench_core::Id>) {
    let Some(report) = report else { return };
    let now = agentbench_core::now_ms();
    for (text, hot) in usage_labels(report, project, now).into_iter().rev() {
        let short = text.split(", ").next().unwrap_or(&text).replace(" today", "");
        chip(ui, &short, if hot { READOUT } else { LEGEND }, hot).on_hover_text(text);
    }
}

pub(super) fn section_panel(ui: &mut Ui, body: impl FnOnce(&mut Ui)) {
    egui::Frame::NONE
        .fill(CHASSIS)
        .stroke(Stroke::new(1.0, ETCH))
        .corner_radius(3)
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui);
        });
}

pub(super) fn level_slider(
    ui: &mut Ui,
    label: &str,
    level: agentbench_core::Automation,
    inherited: bool,
    width: f32,
) -> Option<agentbench_core::Automation> {
    use agentbench_core::Automation;
    let stops = Automation::ALL.len();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::click_and_drag());
    let pad = 7.0;
    let x_of = |i: usize| rect.left() + pad + (rect.width() - 2.0 * pad) * i as f32 / (stops - 1) as f32;
    let mut picked = None;
    if let Some(p) = resp.interact_pointer_pos()
        && (resp.clicked() || resp.dragged())
    {
        let nearest = (0..stops)
            .min_by(|a, b| (x_of(*a) - p.x).abs().total_cmp(&(x_of(*b) - p.x).abs()))
            .unwrap_or(0);
        picked = Some(Automation::from_rank(nearest as u8));
    }
    let rank = level.rank() as usize;
    ui.input(|i| {
        use egui::accesskit::{Action, ActionData};
        for r in i.accesskit_action_requests(resp.id, Action::SetValue) {
            if let Some(ActionData::NumericValue(v)) = r.data {
                picked = Some(Automation::from_rank(v.round().clamp(0.0, (stops - 1) as f64) as u8));
            }
        }
        if i.has_accesskit_action_request(resp.id, Action::Increment) && rank + 1 < stops {
            picked = Some(Automation::from_rank(rank as u8 + 1));
        }
        if i.has_accesskit_action_request(resp.id, Action::Decrement) && rank > 0 {
            picked = Some(Automation::from_rank(rank as u8 - 1));
        }
    });
    if resp.has_focus() {
        ui.input(|i| {
            if i.key_pressed(egui::Key::ArrowRight) && rank + 1 < stops {
                picked = Some(Automation::from_rank(rank as u8 + 1));
            }
            if i.key_pressed(egui::Key::ArrowLeft) && rank > 0 {
                picked = Some(Automation::from_rank(rank as u8 - 1));
            }
        });
    }
    let value = picked.unwrap_or(level);
    resp.widget_info(|| egui::WidgetInfo::slider(true, value.rank() as f64, label));
    ui.ctx().accesskit_node_builder(resp.id, |b| {
        use egui::accesskit::Action;
        b.set_min_numeric_value(0.0);
        b.set_max_numeric_value((stops - 1) as f64);
        b.set_numeric_value_step(1.0);
        b.set_value(value.label().to_string());
        b.add_action(Action::SetValue);
        b.add_action(Action::Increment);
        b.add_action(Action::Decrement);
    });
    let tint = if inherited { READOUT_DIM } else { READOUT };
    let p = ui.painter();
    let y = rect.center().y;
    p.line_segment([egui::pos2(x_of(0), y), egui::pos2(x_of(stops - 1), y)], Stroke::new(2.0, ETCH));
    p.line_segment([egui::pos2(x_of(0), y), egui::pos2(x_of(value.rank() as usize), y)], Stroke::new(2.0, tint));
    for i in 0..stops {
        let lit = i <= value.rank() as usize;
        p.circle_filled(egui::pos2(x_of(i), y), 2.5, if lit { tint } else { ETCH });
    }
    let handle = egui::pos2(x_of(value.rank() as usize), y);
    let edge = if resp.hovered() || resp.has_focus() { VALUE } else { tint };
    p.circle_filled(handle, 6.0, WELL);
    p.circle_stroke(handle, 6.0, Stroke::new(2.0, edge));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(value.about());
    picked.filter(|p| *p != level || inherited)
}

/// One choice on its own row, with a tick box for several picks or a dot for one.
pub(super) fn tab_chip(ui: &mut Ui, text: &str, tint: egui::Color32, on: bool) -> egui::Response {
    let font = egui::FontId::proportional(11.5);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, tint);
    let (rect, _) = ui.allocate_exact_size(galley.size() + Vec2::new(16.0, 8.0), Sense::hover());
    let resp = ui.interact(rect, ui.id().with(("tab", text)), Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, text.to_string())
    });
    let fill = if on {
        tint.gamma_multiply(0.18)
    } else if resp.hovered() {
        tint.gamma_multiply(0.08)
    } else {
        egui::Color32::TRANSPARENT
    };
    let edge = if on || resp.hovered() { tint } else { ETCH };
    ui.painter().rect_filled(rect, 3.0, fill);
    ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, tint);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
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
    agent_ui::widgets::ago(ms, now)
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

/// The chat a `agentbench://open?session=…` link names, as a notification's tap carries.
pub(super) fn chat_in_link(link: &str) -> Option<SessionId> {
    let link = link.trim();
    let query = link.strip_prefix("agentbench://open?")?;
    query.split('&').find_map(|kv| kv.strip_prefix("session=")).and_then(|v| v.parse().ok())
}

/// Words a debug build takes as heard speech, from `agentbench://speak?partial=a+b&n=1`, so a
/// script can dictate on an emulator with no microphone. `final=` ends the take.
pub(super) fn speech_in_link(link: &str) -> Option<(String, bool)> {
    let query = link.trim().strip_prefix("agentbench://speak?")?;
    query.split('&').find_map(|kv| {
        let (key, value) = kv.split_once('=')?;
        let last = match key {
            "partial" => false,
            "final" => true,
            _ => return None,
        };
        Some((value.replace('+', " "), last))
    })
}


pub(super) fn server_status(ui: &mut Ui, up: bool, text: &str) {
    status(ui, up, text);
}

pub(super) fn server_chip(ui: &mut Ui, name: &str, down: bool) {
    let hue = name.bytes().fold(11u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 360;
    let fill = egui::ecolor::Hsva::new(hue as f32 / 360.0, 0.5, 0.6, 1.0);
    let tint = if down { LEGEND } else { egui::Color32::from(fill) };
    let text = if down { format!("{name}  server offline") } else { name.to_string() };
    let r = tab_chip(ui, &text, tint, false);
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("server {text}")));
}
