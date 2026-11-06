//! Files sent with a message: picked, dropped or pasted into the composer, then shown in the
//! transcript. Reading, encoding and decoding all happen off the UI thread.

use agentbench_core::{Attachment, SessionId, Upload};
use base64::Engine;
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
        agentbench_core::media_kind(&self.mime) == Some("image")
    }
}

/// A file read in the background, on its way to the composer.
pub enum Arrived {
    File { name: String, mime: String, data: Vec<u8>, preview: Option<egui::ColorImage> },
    Failed(String),
    Saved(String),
    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    SavedTo { key: (SessionId, agentbench_core::Id), path: std::path::PathBuf, told: String },
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
    thumbs: HashMap<(SessionId, agentbench_core::Id), Thumb>,
    thumb_tx: Sender<((SessionId, agentbench_core::Id), Option<egui::ColorImage>)>,
    thumb_rx: Receiver<((SessionId, agentbench_core::Id), Option<egui::ColorImage>)>,
    texts: HashMap<Key, Body>,
    saved: HashMap<Key, std::path::PathBuf>,
    text_tx: Sender<(Key, Option<String>)>,
    text_rx: Receiver<(Key, Option<String>)>,
}

type Key = (SessionId, agentbench_core::Id);

pub enum Body {
    Loading,
    Ready(std::sync::Arc<str>),
    Failed,
}

const PREVIEW_LINES: usize = 40;
const PREVIEW_CHARS: usize = 6000;

pub fn excerpt(text: &str) -> (String, usize) {
    let mut out = String::new();
    let mut lines = text.lines();
    let mut shown = 0;
    for line in lines.by_ref().take(PREVIEW_LINES) {
        if out.len() + line.len() > PREVIEW_CHARS {
            let room = PREVIEW_CHARS.saturating_sub(out.len());
            out.push_str(&line[..line.floor_char_boundary(room)]);
            return (out, text.lines().count().saturating_sub(shown + 1));
        }
        out.push_str(line);
        out.push('\n');
        shown += 1;
    }
    (out.trim_end().to_string(), lines.count())
}

impl Default for Tray {
    fn default() -> Self {
        let (tx, rx) = channel();
        let (thumb_tx, thumb_rx) = channel();
        let (text_tx, text_rx) = channel();
        Tray {
            files: Vec::new(),
            tx,
            rx,
            thumbs: HashMap::new(),
            thumb_tx,
            thumb_rx,
            texts: HashMap::new(),
            saved: HashMap::new(),
            text_tx,
            text_rx,
        }
    }
}

/// Longest edge of a preview or thumbnail.
const THUMB_EDGE: u32 = 320;

impl Tray {
    pub fn saved_path(&self, key: Key) -> Option<std::path::PathBuf> {
        self.saved.get(&key).cloned()
    }

    pub fn sender(&self) -> Sender<Arrived> {
        self.tx.clone()
    }

    /// Take in whatever finished reading. A message for the user comes back when one failed.
    pub fn poll(&mut self, ctx: &egui::Context) -> Option<(bool, String)> {
        let mut problem = None;
        for got in self.rx.try_iter().collect::<Vec<_>>() {
            match got {
                Arrived::File { name, mime, data, preview } => {
                    if self.files.len() >= agentbench_core::ATTACHMENTS_MAX {
                        problem = Some((
                            false,
                            format!(
                                "at most {} files go with one message",
                                agentbench_core::ATTACHMENTS_MAX
                            ),
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
                Arrived::Failed(e) => problem = Some((false, e)),
                Arrived::Saved(e) => problem = Some((true, e)),
                #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
                Arrived::SavedTo { key, path, told } => {
                    self.saved.insert(key, path);
                    problem = Some((true, told));
                }
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
        for (key, text) in self.text_rx.try_iter().collect::<Vec<_>>() {
            let body = match text {
                Some(t) => Body::Ready(t.into()),
                None => Body::Failed,
            };
            self.texts.insert(key, body);
        }
        problem
    }

    pub fn body(
        &mut self,
        session: SessionId,
        a: &Attachment,
        fetch: impl FnOnce(Sender<(Key, Option<String>)>),
    ) -> &Body {
        let tx = self.text_tx.clone();
        self.texts.entry((session, a.id)).or_insert_with(|| {
            fetch(tx);
            Body::Loading
        })
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
        fetch: impl FnOnce(Sender<((SessionId, agentbench_core::Id), Option<egui::ColorImage>)>),
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
    if data.len() > agentbench_core::ATTACHMENT_MAX {
        return Arrived::Failed(format!(
            "{name} is over {} MB",
            agentbench_core::ATTACHMENT_MAX >> 20
        ));
    }
    let mime = agentbench_core::mime_for(&name)
        .map(str::to_string)
        .or_else(|| sniff(&data))
        .unwrap_or_else(|| "application/octet-stream".into());
    let preview =
        (agentbench_core::media_kind(&mime) == Some("image")).then(|| decode(&data)).flatten();
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

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn save_keyed(
    tx: Sender<Arrived>,
    ctx: egui::Context,
    key: (SessionId, agentbench_core::Id),
    name: String,
    _mime: String,
    data: Vec<u8>,
) {
    save_as(tx, ctx, key, name, data);
}

#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub fn save_keyed(
    tx: Sender<Arrived>,
    ctx: egui::Context,
    _key: (SessionId, agentbench_core::Id),
    name: String,
    mime: String,
    data: Vec<u8>,
) {
    save(tx, ctx, name, mime, data, false);
}

/// Hand a file to the system's default app: downloaded to the cache on a computer, to Downloads
/// and viewed on Android.
#[cfg(target_os = "android")]
pub fn open_external(
    client: &crate::client::Client,
    tx: Sender<Arrived>,
    ctx: egui::Context,
    session: SessionId,
    file: &Attachment,
) {
    let (name, mime) = (file.name.clone(), file.mime.clone());
    if is_runnable(&name) {
        let _ = tx.send(Arrived::Failed(format!(
            "could not open {name}: programs and scripts are not opened, save it instead"
        )));
        ctx.request_repaint();
        return;
    }
    client.attachment(session, file.id, move |bytes| match bytes {
        Some(data) => save(tx, ctx, name, mime, data, true),
        None => {
            let _ = tx.send(Arrived::Failed(format!("could not download {name}")));
            ctx.request_repaint();
        }
    });
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub use open_attachment as open_external;

#[cfg(target_arch = "wasm32")]
pub fn open_external(
    _client: &crate::client::Client,
    tx: Sender<Arrived>,
    ctx: egui::Context,
    _session: SessionId,
    file: &Attachment,
) {
    let _ = tx.send(Arrived::Failed(format!("{} cannot be opened from the browser app", file.name)));
    ctx.request_repaint();
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn save_as(
    tx: Sender<Arrived>,
    ctx: egui::Context,
    key: (SessionId, agentbench_core::Id),
    name: String,
    data: Vec<u8>,
) {
    crate::spawn(async move {
        let Some(target) = rfd::AsyncFileDialog::new().set_file_name(&name).save_file().await
        else {
            return;
        };
        let told = match target.write(&data).await {
            Ok(()) => Arrived::SavedTo {
                key,
                path: target.path().to_path_buf(),
                told: format!("saved {name}"),
            },
            Err(e) => Arrived::Failed(format!("could not save {name}: {e}")),
        };
        let _ = tx.send(told);
        ctx.request_repaint();
    });
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub type OpenFn = std::sync::Arc<dyn Fn(&std::path::Path) -> Result<(), String> + Send + Sync>;

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
static OPENER: std::sync::Mutex<Option<OpenFn>> = std::sync::Mutex::new(None);

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
static CACHE: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Replace what opens a file, so tests never start a real program.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn set_opener(f: OpenFn) {
    *OPENER.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn set_cache_dir(dir: std::path::PathBuf) {
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn system_open(path: &std::path::Path) -> Result<(), String> {
    opener::open(path).map_err(|e| format!("no default app could open it ({e})"))
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn current_opener() -> OpenFn {
    let set = OPENER.lock().unwrap_or_else(|e| e.into_inner()).clone();
    set.unwrap_or_else(|| std::sync::Arc::new(system_open))
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
const CACHE_DAYS: u64 = 7;

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn default_cache() -> std::path::PathBuf {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
    let base = if cfg!(windows) {
        env("LOCALAPPDATA")
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library").join("Caches"))
    } else {
        env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|h| h.join(".cache")))
    };
    base.unwrap_or_else(std::env::temp_dir).join("agentbench").join("attachments")
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn cache_dir() -> std::path::PathBuf {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    let set = CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let dir = set.unwrap_or_else(default_cache);
    let swept = dir.clone();
    SWEPT.call_once(move || sweep(&swept, CACHE_DAYS));
    dir
}

/// Remove downloaded files not touched for `days` days.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn sweep(dir: &std::path::Path, days: u64) {
    let limit = std::time::Duration::from_secs(days * 86400);
    let Ok(sessions) = std::fs::read_dir(dir) else { return };
    for entry in sessions.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > limit);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// A file name that stays inside its folder: no separators, no traversal, never empty.
pub fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = last.chars().filter(|c| !c.is_control() && !"<>:\"|?*".contains(*c)).collect();
    let clean = clean.trim().trim_matches('.').trim().to_string();
    if clean.is_empty() { "file".to_string() } else { clean }
}

const RUNNABLE: &[&str] = &[
    "sh", "bash", "zsh", "fish", "csh", "ksh", "desktop", "exe", "bat", "cmd", "com", "scr", "msi",
    "ps1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "lnk", "reg", "appimage", "jar", "run",
    "app", "command", "pkg", "dmg", "deb", "rpm", "apk", "py", "pl", "rb", "hta", "cpl", "url",
    "workflow", "action", "terminal",
];

/// Files that would run something, or install it, when handed to the system to open.
pub fn is_runnable(name: &str) -> bool {
    let name = name.to_lowercase();
    let ext = name.rsplit_once('.').map(|(_, e)| e.trim());
    ext.is_some_and(|e| RUNNABLE.contains(&e))
}

/// Where an attachment is kept after download.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn cached_path(dir: &std::path::Path, key: (SessionId, agentbench_core::Id), name: &str) -> std::path::PathBuf {
    dir.join(key.0.to_string()).join(key.1.to_string()).join(safe_name(name))
}

/// The downloaded copy, when it is there and the right size.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn already_downloaded(
    dir: &std::path::Path,
    key: (SessionId, agentbench_core::Id),
    name: &str,
    size: u64,
) -> Option<std::path::PathBuf> {
    let path = cached_path(dir, key, name);
    (std::fs::metadata(&path).ok()?.len() == size).then_some(path)
}

/// Write the bytes under the cache (unless already there) and open them. Never runs the file.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn open_bytes(
    dir: &std::path::Path,
    open: &dyn Fn(&std::path::Path) -> Result<(), String>,
    key: (SessionId, agentbench_core::Id),
    name: &str,
    data: &[u8],
) -> Result<std::path::PathBuf, String> {
    if is_runnable(name) {
        return Err("programs and scripts are not opened, save it instead".into());
    }
    let path = match already_downloaded(dir, key, name, data.len() as u64) {
        Some(p) => p,
        None => {
            let path = cached_path(dir, key, name);
            let parent = path.parent().ok_or("no folder")?;
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            std::fs::write(&path, data).map_err(|e| e.to_string())?;
            path
        }
    };
    open(&path)?;
    Ok(path)
}

/// Open a file with the system's default app, downloading it first when needed.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn open_attachment(
    client: &crate::client::Client,
    tx: Sender<Arrived>,
    ctx: egui::Context,
    session: SessionId,
    file: &Attachment,
) {
    let key = (session, file.id);
    let (name, size) = (file.name.clone(), file.size);
    let fail = {
        let (tx, ctx, name) = (tx.clone(), ctx.clone(), name.clone());
        move |why: String| {
            let _ = tx.send(Arrived::Failed(format!("could not open {name}: {why}")));
            ctx.request_repaint();
        }
    };
    if is_runnable(&name) {
        return fail("programs and scripts are not opened, save it instead".into());
    }
    let dir = cache_dir();
    if let Some(path) = already_downloaded(&dir, key, &name, size) {
        let open = current_opener();
        crate::spawn_blocking(move || {
            if let Err(e) = open(&path) {
                fail(e);
            }
        });
        return;
    }
    client.attachment(session, file.id, move |bytes| match bytes {
        None => fail("the download failed".into()),
        Some(data) => {
            let open = current_opener();
            crate::spawn_blocking(move || {
                if let Err(e) = open_bytes(&dir, &*open, key, &name, &data) {
                    fail(e);
                }
            });
        }
    });
}

/// Show the folder holding a saved file.
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn reveal(tx: Sender<Arrived>, ctx: egui::Context, path: std::path::PathBuf) {
    let open = current_opener();
    crate::spawn_blocking(move || {
        let folder = path.parent().map(|p| p.to_path_buf()).unwrap_or(path.clone());
        if let Err(e) = open(&folder) {
            let _ = tx.send(Arrived::Failed(format!("could not open the folder: {e}")));
            ctx.request_repaint();
        }
    });
}

#[cfg(target_os = "android")]
pub fn save(tx: Sender<Arrived>, ctx: egui::Context, name: String, mime: String, data: Vec<u8>, view: bool) {
    crate::spawn_blocking(move || {
        let told = match crate::voice::android::save_download(&name, &mime, &data, view) {
            Ok(()) => Arrived::Saved(format!("saved {name} to Downloads")),
            Err(e) => Arrived::Failed(format!("could not save {name}: {e}")),
        };
        let _ = tx.send(told);
        ctx.request_repaint();
    });
}

#[cfg(target_arch = "wasm32")]
pub fn save(tx: Sender<Arrived>, ctx: egui::Context, name: String, _mime: String, _data: Vec<u8>, _view: bool) {
    let _ = tx.send(Arrived::Failed(format!("{name} cannot be saved from the browser app")));
    ctx.request_repaint();
}

#[cfg(target_os = "android")]
pub fn pick(tx: Sender<Arrived>, ctx: egui::Context) {
    use crate::voice::android::{open_file_picker, picker_busy, take_picked};
    if let Err(e) = open_file_picker() {
        let _ = tx.send(Arrived::Failed(format!("could not open the file picker: {e}")));
        ctx.request_repaint();
        return;
    }
    crate::spawn_blocking(move || {
        let give_up = web_time::Instant::now() + web_time::Duration::from_secs(600);
        loop {
            match take_picked() {
                Some(f) => match f.data {
                    Some(data) => arrive(tx.clone(), ctx.clone(), f.name, data),
                    None => {
                        let _ = tx.send(Arrived::Failed(format!("could not read {}", f.name)));
                        ctx.request_repaint();
                    }
                },
                None if !picker_busy() || web_time::Instant::now() > give_up => break,
                None => std::thread::sleep(std::time::Duration::from_millis(150)),
            }
        }
    });
}

#[cfg(target_arch = "wasm32")]
pub fn pick(_tx: Sender<Arrived>, _ctx: egui::Context) {}

pub fn can_pick() -> bool {
    cfg!(not(target_arch = "wasm32"))
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
    fn an_excerpt_stops_at_forty_lines_and_counts_the_rest() {
        let text = (0..100).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        let (shown, more) = excerpt(&text);
        assert_eq!((shown.lines().count(), more), (40, 60));
        assert_eq!(excerpt("a\nb"), ("a\nb".to_string(), 0));
    }

    #[test]
    fn a_file_name_cannot_leave_its_folder() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("..\\..\\evil.txt"), "evil.txt");
        assert_eq!(safe_name("a/b/report.pdf"), "report.pdf");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("re:port?.pdf"), "report.pdf");
    }

    #[test]
    fn programs_and_scripts_are_not_opened() {
        for n in ["a.sh", "x.EXE", "run.bat", "t.desktop", "App.AppImage", "x.jar", "m.msi", "p.ps1"] {
            assert!(is_runnable(n), "{n}");
        }
        for n in ["a.txt", "pic.png", "report.pdf", "notes", "sh", "archive.tar.gz"] {
            assert!(!is_runnable(n), "{n}");
        }
    }

    #[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
    #[test]
    fn open_writes_the_file_once_and_hands_its_path_to_the_opener() {
        let dir = std::env::temp_dir().join(format!("ab-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sid = SessionId::default();
        let seen = std::sync::Mutex::new(Vec::new());
        let open = |p: &std::path::Path| {
            seen.lock().unwrap().push(p.to_path_buf());
            Ok(())
        };
        let path = open_bytes(&dir, &open, (sid, 9), "../r.txt", b"hello").unwrap();
        assert_eq!(path, dir.join(sid.to_string()).join("9").join("r.txt"));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        std::fs::write(&path, b"HELLO").unwrap();
        open_bytes(&dir, &open, (sid, 9), "../r.txt", b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"HELLO", "same size file is reused");
        assert_eq!(*seen.lock().unwrap(), vec![path.clone(), path.clone()]);
        assert!(open_bytes(&dir, &open, (sid, 9), "x.sh", b"#!").is_err());
        assert_eq!(seen.lock().unwrap().len(), 2);
        let failing = |_: &std::path::Path| Err("no handler".to_string());
        assert_eq!(open_bytes(&dir, &failing, (sid, 9), "r.txt", b"hello").unwrap_err(), "no handler");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_too_big_is_refused() {
        let big = vec![0u8; agentbench_core::ATTACHMENT_MAX + 1];
        assert!(matches!(check("a.bin".into(), big), Arrived::Failed(e) if e.contains("MB")));
    }
}
