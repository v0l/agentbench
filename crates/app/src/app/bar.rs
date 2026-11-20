use super::*;
use base64::Engine;

const ICON_BYTES: u64 = 256 * 1024;

pub(super) fn initials(name: &str) -> String {
    let words: Vec<&str> =
        name.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let mut out: String = match words.as_slice() {
        [] => "?".to_string(),
        [one] => one.chars().take(2).collect(),
        [a, b, ..] => a.chars().take(1).chain(b.chars().take(1)).collect(),
    };
    out = out.to_uppercase();
    out
}

pub(super) fn colour_of(name: &str) -> egui::Color32 {
    let hue = name.bytes().fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 360;
    egui::Color32::from(egui::ecolor::Hsva::new(hue as f32 / 360.0, 0.45, 0.55, 1.0))
}

pub(super) fn icon_from_file(path: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let meta = std::fs::metadata(path.trim()).ok()?;
        if !meta.is_file() || meta.len() > ICON_BYTES {
            return None;
        }
        let bytes = std::fs::read(path.trim()).ok()?;
        crate::attach::decode(&bytes)?;
        Some(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}

pub(super) fn keep_icon(icon: &mut String) {
    if let Some(encoded) = icon_from_file(icon) {
        *icon = encoded;
    } else if !icon.is_empty() && decode_icon(icon).is_none() {
        icon.clear();
    }
}

fn decode_icon(icon: &str) -> Option<egui::ColorImage> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(icon.trim()).ok()?;
    crate::attach::decode(&bytes)
}

impl App {
    fn workspace_texture(&mut self, ctx: &egui::Context, slot: u32) -> Option<egui::TextureHandle> {
        let icon = self
            .servers
            .iter()
            .find(|r| r.slot == slot)
            .map(|r| r.icon.clone())
            .unwrap_or_default();
        if icon.is_empty() {
            self.server_icons.remove(&slot);
            return None;
        }
        if self.server_icons.get(&slot).is_none_or(|(have, _)| *have != icon) {
            let tex = decode_icon(&icon).map(|img| {
                ctx.load_texture(format!("workspace-icon-{slot}"), img, Default::default())
            });
            self.server_icons.insert(slot, (icon, tex));
        }
        self.server_icons.get(&slot).and_then(|(_, t)| t.clone())
    }

    pub fn workspace_bar_shown(&self) -> bool {
        !self.servers.is_empty() && self.client.is_some()
    }

    pub(super) fn workspace_bar(&mut self, ui: &mut Ui) {
        if !self.workspace_bar_shown() {
            return;
        }
        let ctx = ui.ctx().clone();
        let icon = if agent_ui::key::phone(&ctx) { agent_ui::key::TAP } else { 40.0 };
        let mut pick: Option<Option<u32>> = None;
        let mut add = false;
        let entries: Vec<(Option<u32>, String)> =
            std::iter::once((None, "all workspaces".to_string()))
                .chain(self.server_names().into_iter().map(|(s, n)| (Some(s), n)))
                .collect();
        egui::Panel::left("workspaces")
            .exact_size(56.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(CHASSIS.gamma_multiply(0.6))
                    .inner_margin(egui::Margin::symmetric(8, 8)),
            )
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                for (slot, name) in &entries {
                    let tex = slot.and_then(|s| self.workspace_texture(&ctx, s));
                    let selected = self.view == *slot;
                    let (running, waiting, pending, down) = match slot {
                        Some(s) => {
                            let (r, w, p) =
                                self.client.as_ref().map_or((0, 0, 0), |c| c.activity(*s));
                            (r, w, p, self.client.as_ref().is_some_and(|c| c.offline(*s).is_some()))
                        }
                        None => {
                            let c = self.client.as_ref();
                            let all = c.map(|c| c.slots()).unwrap_or_default();
                            let sum = |f: fn((usize, usize, usize)) -> usize| {
                                all.iter()
                                    .map(|s| f(c.map_or((0, 0, 0), |c| c.activity(*s))))
                                    .sum::<usize>()
                            };
                            (sum(|a| a.0), sum(|a| a.1), sum(|a| a.2), false)
                        }
                    };
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(icon), Sense::click());
                    let label = match slot {
                        Some(_) => format!("workspace {name}"),
                        None => "all workspaces".to_string(),
                    };
                    resp.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label)
                    });
                    let fill = match slot {
                        Some(_) => colour_of(name),
                        None => CHASSIS.gamma_multiply(1.6),
                    };
                    let fade = if down { 0.35 } else { 1.0 };
                    let radius = if selected || resp.hovered() { 12.0 } else { 20.0 };
                    ui.painter().rect_filled(rect, radius, fill.gamma_multiply(fade));
                    if let Some(tex) = &tex {
                        egui::Image::new((tex.id(), rect.size()))
                            .corner_radius(radius)
                            .tint(egui::Color32::WHITE.gamma_multiply(fade))
                            .paint_at(ui, rect);
                    } else {
                        let text = if slot.is_some() { initials(name) } else { "ALL".to_string() };
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            text,
                            egui::FontId::proportional(14.0),
                            egui::Color32::WHITE.gamma_multiply(fade),
                        );
                    }
                    if selected {
                        let pill = egui::Rect::from_min_size(
                            egui::pos2(ui.max_rect().left() - 8.0, rect.center().y - 10.0),
                            Vec2::new(3.0, 20.0),
                        );
                        ui.painter().rect_filled(pill, 1.5, VALUE);
                    }
                    if !down && waiting + pending > 0 {
                        ui.painter().circle_filled(
                            rect.right_top() + Vec2::new(-3.0, 3.0),
                            6.0,
                            CHASSIS,
                        );
                        ui.painter().circle_filled(
                            rect.right_top() + Vec2::new(-3.0, 3.0),
                            4.5,
                            FAULT,
                        );
                        resp.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                format!("{label}, needs you"),
                            )
                        });
                    } else if down {
                        resp.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                format!("{label}, offline"),
                            )
                        });
                    } else if running > 0 {
                        resp.widget_info(|| {
                            egui::WidgetInfo::labeled(
                                egui::WidgetType::Button,
                                true,
                                format!("{label}, working"),
                            )
                        });
                        ui.painter().circle_filled(
                            rect.right_bottom() + Vec2::new(-3.0, -3.0),
                            6.0,
                            CHASSIS,
                        );
                        ui.painter().circle_filled(
                            rect.right_bottom() + Vec2::new(-3.0, -3.0),
                            4.5,
                            TRACE,
                        );
                    }
                    let tip = self.workspace_tip(*slot, name, down);
                    resp.clone().on_hover_ui(|ui| {
                        for (i, line) in tip.iter().enumerate() {
                            ui.label(egui::RichText::new(line).size(if i == 0 {
                                12.0
                            } else {
                                10.5
                            }));
                        }
                    });
                    if resp.clicked() {
                        pick = Some(*slot);
                    }
                    if slot.is_none() {
                        ui.separator();
                    }
                }
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(icon), Sense::click());
                resp.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "add workspace")
                });
                ui.painter().rect_stroke(
                    rect,
                    20.0,
                    Stroke::new(1.0, LEGEND),
                    egui::StrokeKind::Inside,
                );
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "+",
                    egui::FontId::proportional(20.0),
                    LEGEND,
                );
                resp.clone().on_hover_text("Add a workspace: another agentbench server");
                add = resp.clicked();
            });
        if let Some(view) = pick {
            self.set_view(view);
        }
        if add {
            let slot = self.servers.iter().map(|r| r.slot).max().unwrap_or(0) + 1;
            self.add_workspace = Some(Remote {
                slot,
                name: String::new(),
                url: String::new(),
                token: String::new(),
                icon: String::new(),
            });
        }
    }

    pub fn workspace_tip(&self, slot: Option<u32>, name: &str, down: bool) -> Vec<String> {
        let Some(c) = &self.client else {
            return vec![name.to_string()];
        };
        let Some(slot) = slot else {
            return vec![
                "All workspaces".to_string(),
                "projects and chats from every workspace together".to_string(),
            ];
        };
        let mut out = vec![name.to_string(), c.url(slot)];
        out.extend(c.compat(slot).notice());
        match (down, c.host(slot)) {
            (true, _) => out.push("server offline".to_string()),
            (false, Some(h)) => out.push(format!(
                "RAM {}%, disk {}%, CPU {}%",
                h.mem_percent(),
                h.disk_percent(),
                h.cpu_percent()
            )),
            (false, None) => {}
        }
        out
    }

    pub(super) fn add_workspace_modal(&mut self, ctx: &egui::Context) {
        let Some(mut w) = self.add_workspace.take() else {
            return;
        };
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::unique("add-workspace"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(modal_width(ctx, 500.0));
                modal_title(ui, "Add a workspace");
                section(ui, "workspace", "another agentbench server", |ui| {
                    row(ui, "name", |ui| {
                        field(ui, &mut w.name, "workstation");
                    });
                    row_help(ui, "address", "Where that server listens.", |ui| {
                        field(ui, &mut w.url, "https://agentbench.example.com");
                    });
                    row_help(ui, "token", "Printed by `agentbench token` on that machine.", |ui| {
                        secret(ui, &mut w.token);
                    });
                    row_help(ui, "icon", "Optional. Path to a small image file.", |ui| {
                        field(ui, &mut w.icon, "/path/to/icon.png");
                    });
                });
                footer(ui, |ui| {
                    let ready = !w.url.trim().is_empty() && !w.token.trim().is_empty();
                    if ui
                        .add(Key::new("add").solid(READOUT).enabled(ready).min_width(100.0))
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
            keep_icon(&mut w.icon);
            let name = if w.name.trim().is_empty() {
                crate::hub::host_name(&w.url)
            } else {
                w.name.trim().to_string()
            };
            let slot = self.add_server(&name, w.url.trim(), w.token.trim());
            if let Some(r) = self.servers.iter_mut().find(|r| r.slot == slot) {
                r.icon = w.icon.clone();
            }
            self.set_view(Some(slot));
        } else if !close {
            self.add_workspace = Some(w);
        }
    }
}
