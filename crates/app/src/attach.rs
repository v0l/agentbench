//! Files sent with a message: picked, dropped or pasted into the composer, then shown in the
//! transcript. Reading, encoding and decoding all happen off the UI thread.

use base64::Engine;
use foreman_core::{Attachment, SessionId, Upload};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};

/// A file waiting in the composer.
pub struct Pending {
    pub name: String,
    pub mime: String,
    pub size: usize,
    pub data: std::sync::Arc<Vec<u8>>,
    pub preview: Option<egui::TextureHandle>,
}

impl Pending {
    pub fn upload(&self) -> Upload {
        Upload {
            name: self.name.clone(),
            mime: self.mime.clone(),
            data: base64::engine::general_purpose::STANDARD.encode(self.data.as_slice()),
        }
    }

    pub fn is_image(&self) -> bool {
        foreman_core::media_kind(&self.mime) == Some("image")
    }
}

/// A file read in the background, on its way to the composer.
pub enum Arrived {
    File { name: String, mime: String, data: Vec<u8>, preview: Option<egui::ColorImage> },
    Failed(String),
}

/// A picture from the transcript, fetched and decoded in the background.
pub enum Thumb {
    Loading,
    Ready(egui::TextureHandle),
    Failed,
}

pub struct Tray {
    pub files: Vec<Pending>,
    tx: Sender<Arrived>,
    rx: Receiver<Arrived>,
    thumbs: HashMap<(SessionId, foreman_core::Id), Thumb>,
    thumb_tx: Sender<((SessionId, foreman_core::Id), Option<egui::ColorImage>)>,
    thumb_rx: Receiver<((SessionId, foreman_core::Id), Option<egui::ColorImage>)>,
}

impl Default for Tray {
    fn default() -> Self {
        let (tx, rx) = channel();
        let (thumb_tx, thumb_rx) = channel();
        Tray { files: Vec::new(), tx, rx, thumbs: HashMap::new(), thumb_tx, thumb_rx }
    }
}

/// Longest edge of a preview or thumbnail.
const THUMB_EDGE: u32 = 320;

impl Tray {
    pub fn sender(&self) -> Sender<Arrived> {
        self.tx.clone()
    }

    /// Take in whatever finished reading. A message for the user comes back when one failed.
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<String> {
        let mut problem = None;
        for got in self.rx.try_iter().collect::<Vec<_>>() {
            match got {
                Arrived::File { name, mime, data, preview } => {
                    if self.files.len() >= foreman_core::ATTACHMENTS_MAX {
                        problem = Some(format!(
                            "at most {} files go with one message",
                            foreman_core::ATTACHMENTS_MAX
                        ));
                        continue;
                    }
                    let preview = preview.map(|img| {
                        ctx.load_texture(format!("pending-{name}"), img, Default::default())
                    });
                    self.files.push(Pending {
                        size: data.len(),
                        name,
                        mime,
                        data: std::sync::Arc::new(data),
                        preview,
                    });
                }
                Arrived::Failed(e) => problem = Some(e),
            }
        }
        for (key, img) in self.thumb_rx.try_iter().collect::<Vec<_>>() {
            let thumb = match img {
                Some(img) => Thumb::Ready(ctx.load_texture(
                    format!("attachment-{}-{}", key.0, key.1),
                    img,
                    Default::default(),
                )),
                None => Thumb::Failed,
            };
            self.thumbs.insert(key, thumb);
        }
        problem
    }

    pub fn ready(&self) -> usize {
        self.thumbs.values().filter(|t| matches!(t, Thumb::Ready(_))).count()
    }

    pub fn take(&mut self) -> Vec<Upload> {
        self.files.drain(..).map(|p| p.upload()).collect()
    }

    /// The thumbnail of a transcript picture, starting its download the first time it is asked for.
    pub fn thumb(
        &mut self,
        session: SessionId,
        a: &Attachment,
        fetch: impl FnOnce(Sender<((SessionId, foreman_core::Id), Option<egui::ColorImage>)>),
    ) -> &Thumb {
        let key = (session, a.id);
        let tx = self.thumb_tx.clone();
        self.thumbs.entry(key).or_insert_with(|| {
            fetch(tx);
            Thumb::Loading
        })
    }
}

/// Read a file the user dropped or picked, off the UI thread.
pub fn arrive(tx: Sender<Arrived>, ctx: egui::Context, name: String, data: Vec<u8>) {
    crate::spawn_blocking(move || {
        let _ = tx.send(check(name, data));
        ctx.request_repaint();
    });
}

fn check(name: String, data: Vec<u8>) -> Arrived {
    if data.len() > foreman_core::ATTACHMENT_MAX {
        return Arrived::Failed(format!(
            "{name} is over {} MB",
            foreman_core::ATTACHMENT_MAX >> 20
        ));
    }
    let mime = foreman_core::mime_for(&name)
        .map(str::to_string)
        .or_else(|| sniff(&data))
        .unwrap_or_else(|| "application/octet-stream".into());
    let preview =
        (foreman_core::media_kind(&mime) == Some("image")).then(|| decode(&data)).flatten();
    Arrived::File { name, mime, data, preview }
}

fn sniff(data: &[u8]) -> Option<String> {
    image::guess_format(data).ok().map(|f| f.to_mime_type().to_string())
}

/// A picture decoded and shrunk for the screen.
pub fn decode(data: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(data).ok()?;
    let img = if img.width() > THUMB_EDGE || img.height() > THUMB_EDGE {
        img.thumbnail(THUMB_EDGE, THUMB_EDGE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// A picture on the clipboard, as a PNG, read off the UI thread.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn paste(tx: Sender<Arrived>, ctx: egui::Context) {
    crate::spawn_blocking(move || {
        let got = clipboard_png().map(|png| check(format!("pasted-{}.png", stamp()), png));
        if let Some(got) = got {
            let _ = tx.send(got);
            ctx.request_repaint();
        }
    });
}

#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn paste(_tx: Sender<Arrived>, _ctx: egui::Context) {}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn clipboard_png() -> Option<Vec<u8>> {
    let img = arboard::Clipboard::new().ok()?.get_image().ok()?;
    let rgba =
        image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())?;
    let mut out = std::io::Cursor::new(Vec::new());
    rgba.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn stamp() -> String {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

/// Let the user pick files with the system's dialog.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn pick(tx: Sender<Arrived>, ctx: egui::Context) {
    crate::spawn(async move {
        let Some(files) = rfd::AsyncFileDialog::new().set_title("Attach files").pick_files().await
        else {
            return;
        };
        for f in files {
            let name = f.file_name();
            let data = f.read().await;
            arrive(tx.clone(), ctx.clone(), name, data);
        }
    });
}

#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn pick(_tx: Sender<Arrived>, _ctx: egui::Context) {}

pub fn can_pick() -> bool {
    cfg!(not(any(target_arch = "wasm32", target_os = "android")))
}

pub fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn a_dropped_picture_gets_its_type_and_a_small_preview() {
        let Arrived::File { mime, preview, .. } = check("shot.png".into(), png(1200, 600)) else {
            panic!("refused");
        };
        assert_eq!(mime, "image/png");
        let p = preview.expect("a preview");
        assert_eq!(p.size, [THUMB_EDGE as usize, THUMB_EDGE as usize / 2]);
    }

    #[test]
    fn a_nameless_picture_is_known_by_its_bytes() {
        let Arrived::File { mime, .. } = check("clipboard".into(), png(4, 4)) else {
            panic!("refused");
        };
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn a_file_too_big_is_refused() {
        let big = vec![0u8; foreman_core::ATTACHMENT_MAX + 1];
        assert!(matches!(check("a.bin".into(), big), Arrived::Failed(e) if e.contains("MB")));
    }
}
