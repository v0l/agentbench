//! The large preview of a file a chat carried: fetched from the server here, drawn by agent-ui.

use super::*;
use agent_ui::viewer::{Decoded, Options, Shown};
use agentbench_core::Attachment;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};

const IMAGE_EDGE: u32 = 4096;

type Loaded = Option<(Arc<Vec<u8>>, Decoded)>;

pub(super) struct Viewer {
    session: SessionId,
    shared: agent_ui::viewer::Viewer,
    rx: Receiver<Loaded>,
}

pub use agent_ui::viewer::decode_text;

fn decode(file: &Shown, bytes: &[u8]) -> Decoded {
    if file.mime.starts_with("image/") || agentbench_core::media_kind(&file.mime) == Some("image") {
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
    if file.wants_text() {
        let (text, truncated) = decode_text(bytes);
        return Decoded::Text { text, truncated };
    }
    Decoded::Info
}

fn shown(a: &Attachment) -> Shown {
    Shown { id: a.id, name: a.name.clone(), mime: a.mime.clone(), size: a.size }
}

fn attachment(s: &Shown) -> Attachment {
    Attachment { id: s.id, name: s.name.clone(), mime: s.mime.clone(), size: s.size }
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
            shared: agent_ui::viewer::Viewer::new(files.iter().map(shown).collect(), index),
            rx: channel().1,
        };
        self.load_viewer(ctx, &mut viewer);
        self.viewer = Some(viewer);
    }

    fn load_viewer(&self, ctx: &egui::Context, v: &mut Viewer) {
        let (tx, rx) = channel();
        v.rx = rx;
        v.shared.reset();
        let Some(c) = &self.client else {
            v.shared.loaded(ctx, None);
            return;
        };
        let file = v.shared.file().clone();
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
        self.viewer.as_ref().is_some_and(|v| v.shared.ready())
    }

    pub fn viewer_scale(&self) -> f32 {
        self.viewer.as_ref().map_or(0.0, |v| v.shared.scale())
    }

    pub fn viewer_truncated(&self) -> bool {
        self.viewer.as_ref().is_some_and(|v| v.shared.truncated())
    }

    pub fn viewer_index(&self) -> Option<(usize, usize)> {
        self.viewer.as_ref().map(|v| (v.shared.index, v.shared.files.len()))
    }

    pub(super) fn viewer_modal(&mut self, ctx: &egui::Context) {
        let Some(mut v) = self.viewer.take() else { return };
        if let Ok(got) = v.rx.try_recv() {
            v.shared.loaded(ctx, got);
        }
        let file = attachment(v.shared.file());
        let key = (v.session, file.id);
        let saved = self.tray.saved_path(key);
        let opts = Options {
            runnable: crate::attach::is_runnable(&file.name),
            can_open: cfg!(not(target_arch = "wasm32")),
            can_reveal: saved.is_some(),
        };
        let width = modal_width(ctx, 960.0);
        let p = v.shared.modal(ctx, &mut self.markdown, width, &opts);
        let (tx, here) = (self.tray.sender(), ctx.clone());
        if p.save
            && let Some(bytes) = &v.shared.bytes
        {
            crate::attach::save_keyed(
                tx.clone(),
                here.clone(),
                key,
                file.name.clone(),
                file.mime.clone(),
                bytes.to_vec(),
            );
        }
        if p.copy
            && let Some(text) = v.shared.text()
        {
            ctx.copy_text(text.to_string());
            self.notice = Some((true, format!("copied {}", file.name)));
        }
        if p.external
            && let Some(c) = &self.client
        {
            crate::attach::open_external(c, tx.clone(), here.clone(), v.session, &file);
        }
        #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
        if p.reveal
            && let Some(path) = saved
        {
            crate::attach::reveal(tx, here, path);
        }
        #[cfg(any(target_arch = "wasm32", target_os = "android"))]
        let _ = (saved, tx, here);
        if p.close {
            return;
        }
        if v.shared.step(p.step) {
            self.load_viewer(ctx, &mut v);
        }
        self.viewer = Some(v);
    }
}
