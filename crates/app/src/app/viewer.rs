//! The large preview of a file a chat carried: pictures to zoom and pan, text to read and search.

use super::*;
use agentbench_core::Attachment;
use egui::text::{LayoutJob, TextFormat};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

const TEXT_CAP: usize = 1 << 20;
const IMAGE_EDGE: u32 = 4096;
const ZOOM_STEP: f32 = 1.25;
const ZOOM_MIN: f32 = 0.05;
const ZOOM_MAX: f32 = 16.0;

enum Decoded {
    Image(egui::ColorImage),
    Text { text: String, truncated: bool },
    Info,
}

type Loaded = Option<(Arc<Vec<u8>>, Decoded)>;

enum Content {
    Loading,
    Failed,
    Image(egui::TextureHandle),
    Text { lines: Vec<String>, text: Arc<str>, truncated: bool },
    Info,
}

pub(super) struct Viewer {
    session: SessionId,
    files: Vec<Attachment>,
    index: usize,
    content: Content,
    bytes: Option<Arc<Vec<u8>>>,
    rx: Receiver<Loaded>,
    fit: bool,
    scale: f32,
    shown: f32,
    offset: Vec2,
    search: String,
    jump: bool,
}

pub fn decode_text(bytes: &[u8]) -> (String, bool) {
    let truncated = bytes.len() > TEXT_CAP;
    let cut = &bytes[..bytes.len().min(TEXT_CAP)];
    (String::from_utf8_lossy(cut).into_owned(), truncated)
}

fn wants_text(file: &Attachment) -> bool {
    file.is_text() || file.is_markdown()
}

fn decode(file: &Attachment, bytes: &[u8]) -> Decoded {
    if file.is_image() {
        return match image::load_from_memory(bytes) {
            Ok(img) => {
                let img = if img.width() > IMAGE_EDGE || img.height() > IMAGE_EDGE {
                    img.thumbnail(IMAGE_EDGE, IMAGE_EDGE)
                } else {
                    img
                };
                let rgba = img.to_rgba8();
                let size = [rgba.width() as usize, rgba.height() as usize];
                Decoded::Image(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
            }
            Err(_) => Decoded::Info,
        };
    }
    if wants_text(file) {
        let (text, truncated) = decode_text(bytes);
        return Decoded::Text { text, truncated };
    }
    Decoded::Info
}

fn matches(line: &str, needle: &str) -> bool {
    !needle.is_empty() && line.to_lowercase().contains(needle)
}

impl App {
    pub(super) fn open_viewer(&mut self, ctx: &egui::Context, session: SessionId, file: &Attachment) {
        let mut files: Vec<Attachment> = self
            .transcripts
            .get(&session)
            .map(|t| {
                t.messages
                    .iter()
                    .flat_map(|m| match &m.entry {
                        Entry::File { file, .. } => vec![file.clone()],
                        Entry::User { attachments, .. } => attachments.clone(),
                        _ => Vec::new(),
                    })
                    .filter(|f| f.is_image())
                    .collect()
            })
            .unwrap_or_default();
        if !file.is_image() || !files.iter().any(|f| f.id == file.id) {
            files = vec![file.clone()];
        }
        let index = files.iter().position(|f| f.id == file.id).unwrap_or(0);
        let mut viewer = Viewer {
            session,
            files,
            index,
            content: Content::Loading,
            bytes: None,
            rx: channel().1,
            fit: true,
            scale: 1.0,
            shown: 1.0,
            offset: Vec2::ZERO,
            search: String::new(),
            jump: false,
        };
        self.load_viewer(ctx, &mut viewer);
        self.viewer = Some(viewer);
    }

    fn load_viewer(&self, ctx: &egui::Context, v: &mut Viewer) {
        let (tx, rx) = channel();
        v.rx = rx;
        v.content = Content::Loading;
        v.bytes = None;
        v.fit = true;
        v.offset = Vec2::ZERO;
        v.search.clear();
        let Some(c) = &self.client else {
            v.content = Content::Failed;
            return;
        };
        let file = v.files[v.index].clone();
        let ctx = ctx.clone();
        c.attachment(v.session, file.id, move |bytes| match bytes {
            Some(bytes) => crate::spawn_blocking(move || {
                let decoded = decode(&file, &bytes);
                let _ = tx.send(Some((Arc::new(bytes), decoded)));
                ctx.request_repaint();
            }),
            None => {
                let _ = tx.send(None);
                ctx.request_repaint();
            }
        });
    }

    pub fn viewer_open(&self) -> bool {
        self.viewer.is_some()
    }

    pub fn viewer_ready(&self) -> bool {
        self.viewer.as_ref().is_some_and(|v| !matches!(v.content, Content::Loading))
    }

    pub fn viewer_scale(&self) -> f32 {
        self.viewer.as_ref().map_or(0.0, |v| v.shown)
    }

    pub fn viewer_truncated(&self) -> bool {
        matches!(
            self.viewer.as_ref().map(|v| &v.content),
            Some(Content::Text { truncated: true, .. })
        )
    }

    pub fn viewer_index(&self) -> Option<(usize, usize)> {
        self.viewer.as_ref().map(|v| (v.index, v.files.len()))
    }

    pub(super) fn viewer_modal(&mut self, ctx: &egui::Context) {
        let Some(mut v) = self.viewer.take() else { return };
        if let Ok(got) = v.rx.try_recv() {
            match got {
                None => v.content = Content::Failed,
                Some((bytes, decoded)) => {
                    v.content = match decoded {
                        Decoded::Image(img) => Content::Image(ctx.load_texture(
                            format!("viewer-{}", v.files[v.index].id),
                            img,
                            egui::TextureOptions::LINEAR,
                        )),
                        Decoded::Text { text, truncated } => Content::Text {
                            lines: text.lines().map(str::to_string).collect(),
                            text: text.into(),
                            truncated,
                        },
                        Decoded::Info => Content::Info,
                    };
                    v.bytes = Some(bytes);
                }
            }
        }
        let file = v.files[v.index].clone();
        let key = (v.session, file.id);
        let saved = self.tray.saved_path(key);
        let (mut close, mut save, mut copy, mut external, mut reveal) =
            (false, false, false, false, false);
        let mut step: i32 = 0;
        let width = modal_width(ctx, 960.0);
        let body_h = (ctx.content_rect().height() * 0.62).clamp(220.0, 720.0);
        let runnable = crate::attach::is_runnable(&file.name);
        let m = egui::Modal::new(egui::Id::new("viewer"))
            .backdrop_color(egui::Color32::from_black_alpha(200))
            .show(ctx, |ui| {
                ui.set_width(width);
                modal_title(ui, &file.name);
                Line::new()
                    .value(crate::attach::size(file.size))
                    .size(11.0)
                    .gap(10.0)
                    .note(file.mime.clone())
                    .size(11.0)
                    .show(ui);
                ui.add_space(8.0);
                match &v.content {
                    Content::Loading => {
                        let (r, _) = ui.allocate_exact_size(Vec2::new(width, body_h), Sense::hover());
                        spinner(ui, r.center(), 12.0, LEGEND);
                    }
                    Content::Failed => {
                        status(ui, false, "could not download this file");
                    }
                    Content::Info => {
                        let pdf = file.mime == "application/pdf";
                        let why = if pdf {
                            "pages are not rendered in the app, open it externally to read it"
                        } else if matches!(agentbench_core::media_kind(&file.mime), Some("audio" | "video")) {
                            "play it by opening it externally"
                        } else {
                            "no preview for this kind of file"
                        };
                        section(ui, "file", why, |ui| {
                            reading(ui, "name", &file.name);
                            reading(ui, "size", &crate::attach::size(file.size));
                            reading(ui, "type", &file.mime);
                        });
                    }
                    Content::Image(tex) => {
                        let tools = image_tools(ui, &mut v.fit, &mut v.scale, v.shown, &mut step, v.files.len(), v.index);
                        let _ = tools;
                        let (rect, resp) =
                            ui.allocate_exact_size(Vec2::new(width, body_h), Sense::click_and_drag());
                        ui.painter().rect_filled(rect, 0.0, WELL);
                        let size = tex.size_vec2();
                        let fit = (rect.width() / size.x).min(rect.height() / size.y).min(1.0);
                        if resp.hovered() {
                            let (scroll, pinch) =
                                ctx.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
                            let factor = pinch * (scroll / 400.0).exp();
                            if (factor - 1.0).abs() > 1e-4 {
                                v.fit = false;
                                v.scale = (v.shown * factor).clamp(ZOOM_MIN, ZOOM_MAX);
                            }
                        }
                        if resp.dragged() && !v.fit {
                            v.offset += resp.drag_delta();
                        }
                        if v.fit {
                            v.offset = Vec2::ZERO;
                        }
                        v.shown = if v.fit { fit } else { v.scale };
                        let drawn = egui::Rect::from_center_size(rect.center() + v.offset, size * v.shown);
                        ui.painter_at(rect).image(
                            tex.id(),
                            drawn,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );
                    }
                    Content::Text { lines, text, truncated } => {
                        let needle = v.search.to_lowercase();
                        let hits = lines.iter().filter(|l| matches(l, &needle)).count();
                        let first = lines.iter().position(|l| matches(l, &needle));
                        if !file.is_markdown() {
                            ui.horizontal(|ui| {
                                let w = (width - 200.0).max(120.0);
                                ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
                                    let before = v.search.clone();
                                    field(ui, &mut v.search, "search in file");
                                    v.jump = v.search != before;
                                });
                                if !needle.is_empty() {
                                    Line::new()
                                        .measured(format!("{hits} lines"))
                                        .size(11.0)
                                        .show(ui);
                                }
                            });
                            ui.add_space(4.0);
                        }
                        if *truncated {
                            note(
                                ui,
                                format!(
                                    "truncated at {}, save to see all of it",
                                    crate::attach::size(TEXT_CAP as u64)
                                ),
                                WARN,
                            );
                        }
                        let font = mono(12.0);
                        let row_h = ctx.fonts_mut(|f| f.row_height(&font));
                        let digits = lines.len().to_string().len();
                        let mut area = egui::ScrollArea::both()
                            .id_salt(("viewer-text", file.id))
                            .auto_shrink([false, false])
                            .max_height(body_h);
                        if v.jump && let Some(i) = first {
                            area = area.vertical_scroll_offset(i as f32 * row_h);
                        }
                        if file.is_markdown() {
                            area.show(ui, |ui| {
                                markdown::show(ui, &mut self.markdown, text, file.id as usize + 7_000_000);
                            });
                        } else {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            area.show_rows(ui, row_h, lines.len(), |ui, range| {
                                for i in range {
                                    let hit = matches(&lines[i], &needle);
                                    let mut job = LayoutJob::default();
                                    job.append(
                                        &format!("{:>digits$}  ", i + 1),
                                        0.0,
                                        TextFormat { font_id: font.clone(), color: LEGEND, ..Default::default() },
                                    );
                                    job.append(
                                        &lines[i],
                                        0.0,
                                        TextFormat {
                                            font_id: font.clone(),
                                            color: if hit { READOUT } else { VALUE },
                                            ..Default::default()
                                        },
                                    );
                                    ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
                                }
                            });
                        }
                    }
                }
                if runnable {
                    note(ui, "programs and scripts are not opened, save it instead", WARN);
                }
                footer(ui, |ui| {
                    if press(ui, "close", true, Key::new("close").solid(READOUT).min_width(100.0)) {
                        close = true;
                    }
                    let have = v.bytes.is_some();
                    if press(ui, "save", have, Key::new("save").outline(VALUE).min_width(100.0)) {
                        save = true;
                    }
                    if matches!(v.content, Content::Text { .. })
                        && press(ui, "copy", true, Key::new("copy").outline(VALUE).min_width(100.0))
                    {
                        copy = true;
                    }
                    let can_open = !runnable && cfg!(not(target_arch = "wasm32"));
                    if press(ui, "open externally", can_open, Key::new("open externally").outline(VALUE).min_width(140.0)) {
                        external = true;
                    }
                    if saved.is_some()
                        && press(ui, "reveal in folder", true, Key::new("reveal in folder").outline(VALUE).min_width(140.0))
                    {
                        reveal = true;
                    }
                });
            });
        if m.should_close() {
            close = true;
        }
        if v.files.len() > 1 && matches!(v.content, Content::Image(_)) {
            let (left, right) = ctx.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                )
            });
            step += right as i32 - left as i32;
        }
        let (tx, here) = (self.tray.sender(), ctx.clone());
        if save && let Some(bytes) = &v.bytes {
            crate::attach::save_keyed(
                tx.clone(),
                here.clone(),
                key,
                file.name.clone(),
                file.mime.clone(),
                bytes.to_vec(),
            );
        }
        if copy && let Content::Text { text, .. } = &v.content {
            ctx.copy_text(text.to_string());
            self.notice = Some((true, format!("copied {}", file.name)));
        }
        if external && let Some(c) = &self.client {
            crate::attach::open_external(c, tx.clone(), here.clone(), v.session, &file);
        }
        #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
        if reveal && let Some(path) = saved {
            crate::attach::reveal(tx, here, path);
        }
        #[cfg(any(target_arch = "wasm32", target_os = "android"))]
        let _ = (reveal, saved, tx, here);
        if close {
            return;
        }
        if step != 0 && v.files.len() > 1 {
            let n = v.files.len() as i32;
            v.index = (v.index as i32 + step).rem_euclid(n) as usize;
            self.load_viewer(ctx, &mut v);
        }
        self.viewer = Some(v);
    }
}

fn image_tools(
    ui: &mut Ui,
    fit: &mut bool,
    scale: &mut f32,
    shown: f32,
    step: &mut i32,
    count: usize,
    index: usize,
) {
    ui.horizontal_wrapped(|ui| {
        if count > 1 {
            if press(ui, "previous", true, Key::new("previous").outline(VALUE).min_width(80.0)) {
                *step -= 1;
            }
            if press(ui, "next", true, Key::new("next").outline(VALUE).min_width(80.0)) {
                *step += 1;
            }
            Line::new().value(format!("{} / {}", index + 1, count)).size(11.0).show(ui);
            ui.add_space(12.0);
        }
        if press(ui, "zoom out", true, Key::new("zoom out").outline(VALUE).min_width(80.0)) {
            *fit = false;
            *scale = (shown / ZOOM_STEP).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if press(ui, "zoom in", true, Key::new("zoom in").outline(VALUE).min_width(80.0)) {
            *fit = false;
            *scale = (shown * ZOOM_STEP).clamp(ZOOM_MIN, ZOOM_MAX);
        }
        if press(ui, "fit", true, Key::new("fit").outline(VALUE).min_width(60.0)) {
            *fit = true;
        }
        if press(ui, "100%", true, Key::new("100%").outline(VALUE).min_width(60.0)) {
            *fit = false;
            *scale = 1.0;
        }
        Line::new().measured(format!("{:.0}%", shown * 100.0)).size(11.0).show(ui);
    });
    ui.add_space(6.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_big_text_file_is_cut_and_marked() {
        let big = vec![b'a'; TEXT_CAP + 10];
        let (text, truncated) = decode_text(&big);
        assert!(truncated);
        assert_eq!(text.len(), TEXT_CAP);
        let (text, truncated) = decode_text(b"small");
        assert_eq!((text.as_str(), truncated), ("small", false));
    }

    #[test]
    fn search_ignores_case_and_empty_needles() {
        assert!(matches("Hello World", "world"));
        assert!(!matches("Hello", ""));
    }
}
