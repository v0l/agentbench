use crate::form::field;
use crate::widgets::{press, size, spinner};
use egui::text::{LayoutJob, TextFormat};
use egui::{Sense, Ui, Vec2};
use egui_bench::key::Key;
use egui_bench::markdown;
use egui_bench::prelude::*;
use std::sync::Arc;

pub const TEXT_CAP: usize = 1 << 20;
const ZOOM_STEP: f32 = 1.25;
const ZOOM_MIN: f32 = 0.05;
const ZOOM_MAX: f32 = 16.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub id: i64,
    pub name: String,
    pub mime: String,
    pub size: u64,
}

impl Shown {
    fn file(&self) -> crate::transcript::File<'_> {
        crate::transcript::File { id: self.id, name: &self.name, mime: &self.mime, size: self.size }
    }

    pub fn wants_text(&self) -> bool {
        let f = self.file();
        f.is_text() || f.is_markdown()
    }
}

pub enum Decoded {
    Image(egui::ColorImage),
    Text { text: String, truncated: bool },
    Info,
}

pub enum Content {
    Loading,
    Failed,
    Image(egui::TextureHandle),
    Text { lines: Vec<String>, text: Arc<str>, truncated: bool },
    Info,
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pressed {
    pub close: bool,
    pub save: bool,
    pub copy: bool,
    pub external: bool,
    pub reveal: bool,
    pub step: i32,
}

pub struct Options {
    pub runnable: bool,
    pub can_open: bool,
    pub can_reveal: bool,
}

pub struct Viewer {
    pub files: Vec<Shown>,
    pub index: usize,
    pub content: Content,
    pub bytes: Option<Arc<Vec<u8>>>,
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

fn matches(line: &str, needle: &str) -> bool {
    !needle.is_empty() && line.to_lowercase().contains(needle)
}

impl Viewer {
    pub fn new(files: Vec<Shown>, index: usize) -> Self {
        Viewer {
            files,
            index,
            content: Content::Loading,
            bytes: None,
            fit: true,
            scale: 1.0,
            shown: 1.0,
            offset: Vec2::ZERO,
            search: String::new(),
            jump: false,
        }
    }

    pub fn file(&self) -> &Shown {
        &self.files[self.index]
    }

    pub fn reset(&mut self) {
        self.content = Content::Loading;
        self.bytes = None;
        self.fit = true;
        self.offset = Vec2::ZERO;
        self.search.clear();
    }

    pub fn loaded(&mut self, ctx: &egui::Context, got: Option<(Arc<Vec<u8>>, Decoded)>) {
        let Some((bytes, decoded)) = got else {
            self.content = Content::Failed;
            return;
        };
        self.content = match decoded {
            Decoded::Image(img) => Content::Image(ctx.load_texture(
                format!("viewer-{}", self.file().id),
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
        self.bytes = Some(bytes);
    }

    pub fn ready(&self) -> bool {
        !matches!(self.content, Content::Loading)
    }

    pub fn scale(&self) -> f32 {
        self.shown
    }

    pub fn truncated(&self) -> bool {
        matches!(self.content, Content::Text { truncated: true, .. })
    }

    pub fn text(&self) -> Option<&str> {
        match &self.content {
            Content::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    pub fn modal(
        &mut self,
        ctx: &egui::Context,
        md: &mut markdown::Cache,
        width: f32,
        opts: &Options,
    ) -> Pressed {
        let mut p = Pressed::default();
        let file = self.file().clone();
        let body_h = (ctx.content_rect().height() * 0.62).clamp(220.0, 720.0);
        let m = egui::Modal::new(egui::Id::unique("viewer"))
            .backdrop_color(egui::Color32::from_black_alpha(200))
            .show(ctx, |ui| {
                ui.set_width(width);
                modal_title(ui, &file.name);
                Line::new()
                    .value(size(file.size))
                    .size(11.0)
                    .gap(10.0)
                    .note(file.mime.clone())
                    .size(11.0)
                    .show(ui);
                ui.add_space(8.0);
                self.body(ui, md, &file, width, body_h, &mut p.step);
                if opts.runnable {
                    note(ui, "programs and scripts are not opened, save it instead", WARN);
                }
                footer(ui, |ui| {
                    if press(ui, "close", true, Key::new("close").solid(READOUT).min_width(100.0)) {
                        p.close = true;
                    }
                    let have = self.bytes.is_some();
                    if press(ui, "save", have, Key::new("save").outline(VALUE).min_width(100.0)) {
                        p.save = true;
                    }
                    if matches!(self.content, Content::Text { .. })
                        && press(ui, "copy", true, Key::new("copy").outline(VALUE).min_width(100.0))
                    {
                        p.copy = true;
                    }
                    let can_open = !opts.runnable && opts.can_open;
                    if press(
                        ui,
                        "open externally",
                        can_open,
                        Key::new("open externally").outline(VALUE).min_width(140.0),
                    ) {
                        p.external = true;
                    }
                    if opts.can_reveal
                        && press(
                            ui,
                            "reveal in folder",
                            true,
                            Key::new("reveal in folder").outline(VALUE).min_width(140.0),
                        )
                    {
                        p.reveal = true;
                    }
                });
            });
        if m.should_close() {
            p.close = true;
        }
        if self.files.len() > 1 && matches!(self.content, Content::Image(_)) {
            let (left, right) = ctx.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft),
                    i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight),
                )
            });
            p.step += right as i32 - left as i32;
        }
        p
    }

    pub fn step(&mut self, step: i32) -> bool {
        if step == 0 || self.files.len() < 2 {
            return false;
        }
        let n = self.files.len() as i32;
        self.index = (self.index as i32 + step).rem_euclid(n) as usize;
        self.reset();
        true
    }

    fn body(
        &mut self,
        ui: &mut Ui,
        md: &mut markdown::Cache,
        file: &Shown,
        width: f32,
        body_h: f32,
        step: &mut i32,
    ) {
        let ctx = ui.ctx().clone();
        match &self.content {
            Content::Loading => {
                let (r, _) = ui.allocate_exact_size(Vec2::new(width, body_h), Sense::hover());
                spinner(ui, r.center(), 12.0, LEGEND);
            }
            Content::Failed => {
                status(ui, false, "could not download this file");
            }
            Content::Info => {
                let top = file.mime.split('/').next().unwrap_or_default().to_ascii_lowercase();
                let why = if file.mime == "application/pdf" {
                    "pages are not rendered in the app, open it externally to read it"
                } else if matches!(top.as_str(), "audio" | "video") {
                    "play it by opening it externally"
                } else {
                    "no preview for this kind of file"
                };
                section(ui, "file", why, |ui| {
                    reading(ui, "name", &file.name);
                    reading(ui, "size", &size(file.size));
                    reading(ui, "type", &file.mime);
                });
            }
            Content::Image(tex) => {
                let (count, index, shown) = (self.files.len(), self.index, self.shown);
                image_tools(ui, &mut self.fit, &mut self.scale, shown, step, count, index);
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::new(width, body_h), Sense::click_and_drag());
                ui.painter().rect_filled(rect, 0.0, WELL);
                let tex_size = tex.size_vec2();
                let fit = (rect.width() / tex_size.x).min(rect.height() / tex_size.y).min(1.0);
                if resp.hovered() {
                    let (scroll, pinch) = ctx.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
                    let factor = pinch * (scroll / 400.0).exp();
                    if (factor - 1.0).abs() > 1e-4 {
                        self.fit = false;
                        self.scale = (self.shown * factor).clamp(ZOOM_MIN, ZOOM_MAX);
                    }
                }
                if resp.dragged() && !self.fit {
                    self.offset += resp.drag_delta();
                }
                if self.fit {
                    self.offset = Vec2::ZERO;
                }
                self.shown = if self.fit { fit } else { self.scale };
                let drawn =
                    egui::Rect::from_center_size(rect.center() + self.offset, tex_size * self.shown);
                ui.painter_at(rect).image(
                    tex.id(),
                    drawn,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            Content::Text { lines, text, truncated } => {
                let markdown_file = file.file().is_markdown();
                let needle = self.search.to_lowercase();
                let hits = lines.iter().filter(|l| matches(l, &needle)).count();
                let first = lines.iter().position(|l| matches(l, &needle));
                if !markdown_file {
                    ui.horizontal(|ui| {
                        let w = (width - 200.0).max(120.0);
                        ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
                            let before = self.search.clone();
                            field(ui, &mut self.search, "search in file");
                            self.jump = self.search != before;
                        });
                        if !needle.is_empty() {
                            Line::new().measured(format!("{hits} lines")).size(11.0).show(ui);
                        }
                    });
                    ui.add_space(4.0);
                }
                if *truncated {
                    note(
                        ui,
                        format!("truncated at {}, save to see all of it", size(TEXT_CAP as u64)),
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
                if self.jump
                    && let Some(i) = first
                {
                    area = area.vertical_scroll_offset(i as f32 * row_h);
                }
                if markdown_file {
                    area.show(ui, |ui| {
                        markdown::show(ui, md, text, file.id as usize + 7_000_000);
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

    #[test]
    fn stepping_wraps_round_the_pictures() {
        let f = |id| Shown { id, name: format!("{id}.png"), mime: "image/png".into(), size: 1 };
        let mut v = Viewer::new(vec![f(1), f(2), f(3)], 0);
        assert!(v.step(-1));
        assert_eq!(v.file().id, 3);
        assert!(v.step(2));
        assert_eq!(v.file().id, 2);
        let mut one = Viewer::new(vec![f(1)], 0);
        assert!(!one.step(1));
    }

    #[test]
    fn a_text_file_opens_with_its_lines_and_copy() {
        let shown = Shown { id: 1, name: "a.txt".into(), mime: "text/plain".into(), size: 3 };
        let state = (Viewer::new(vec![shown], 0), markdown::Cache::default(), Pressed::default());
        let mut h = egui_kittest::Harness::builder().with_size([1000.0, 800.0]).build_ui_state(
            |ui, (v, md, p): &mut (Viewer, markdown::Cache, Pressed)| {
                let ctx = &ui.ctx().clone();
                egui_bench::theme::install(ctx);
                if !v.ready() {
                    let (text, truncated) = decode_text(b"alpha\nbeta");
                    v.loaded(ctx, Some((Arc::new(b"alpha\nbeta".to_vec()), Decoded::Text { text, truncated })));
                }
                let opts = Options { runnable: false, can_open: true, can_reveal: false };
                let got = v.modal(ctx, md, 900.0, &opts);
                p.copy |= got.copy;
            },
            state,
        );
        h.run_steps(3);
        use egui_kittest::kittest::Queryable;
        assert_eq!(h.state().0.text(), Some("alpha\nbeta"));
        h.get_by_label("copy").click();
        h.run_steps(2);
        assert!(h.state().2.copy);
    }
}
