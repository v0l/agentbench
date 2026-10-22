//! Reading pictures, sound and video for the model.
//!
//! A chat completions tool result is text only, so `read_file` on a media file answers with a
//! short note and hands back content parts. The caller sends the parts in the user message that
//! follows the tool results. Video has no common wire format, so it is sampled into frames.
//! ffmpeg does the converting; files the model takes as they are never need it.

use super::files;
use base64::{Engine, engine::general_purpose::STANDARD};
use egui_bench::agent::grants::Access;
use egui_bench::agent::workspace::Workspace;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Largest picture sent as it is; a bigger one is shrunk first.
const IMAGE_MAX: u64 = 5 << 20;
/// Longest edge of a picture or video frame after shrinking.
const EDGE: u32 = 2048;
/// Largest sound file sent as it is.
const AUDIO_MAX: u64 = 15 << 20;
/// Seconds of sound kept when a file has to be converted.
const AUDIO_SECONDS: u32 = 300;
const FRAMES: usize = 8;
const FRAMES_MAX: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Image,
    Audio,
    Video,
}

#[derive(Debug)]
pub struct Media {
    /// What the tool answers with: the file, its size and what was attached.
    pub note: String,
    /// Content parts for the user message that carries the media.
    pub parts: Vec<Value>,
}

pub fn kind(path: &str) -> Option<Kind> {
    let ext = Path::new(path.trim()).extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "avif" | "heic" => {
            Kind::Image
        }
        "wav" | "mp3" | "ogg" | "oga" | "opus" | "flac" | "m4a" | "aac" | "wma" => Kind::Audio,
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "mpg" | "mpeg" | "flv" | "wmv" => {
            Kind::Video
        }
        _ => return None,
    })
}

/// Reads the file `read_file` was asked for if it is a picture, sound or video, and `None` if it
/// is something else and should be read as text.
pub fn read(ws: &Workspace, args: &Value) -> Option<Result<Media, String>> {
    let path = files::text(args, &files::PATH)?;
    let kind = kind(path)?;
    Some(load(ws, path, kind, args))
}

fn load(ws: &Workspace, path: &str, kind: Kind, args: &Value) -> Result<Media, String> {
    let file = ws.resolve_for(path, Access::Read)?;
    let shown = ws.shown(&file);
    let meta = fs::metadata(&file).map_err(|e| format!("{shown}: {e}"))?;
    if meta.is_dir() {
        return Err(format!("{shown} is a folder, use list_files"));
    }
    match kind {
        Kind::Image => image(&file, &shown, meta.len()),
        Kind::Audio => audio(&file, &shown, meta.len()),
        Kind::Video => {
            let frames = files::number(args, &["frames"]).unwrap_or(FRAMES).clamp(1, FRAMES_MAX);
            video(&file, &shown, meta.len(), frames)
        }
    }
}

fn ext(file: &Path) -> String {
    file.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase()
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{b} bytes"),
    }
}

fn picture(mime: &str, bytes: &[u8]) -> Value {
    json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{mime};base64,{}", STANDARD.encode(bytes)) },
    })
}

fn label(text: String) -> Value {
    json!({ "type": "text", "text": text })
}

fn image(file: &Path, shown: &str, bytes: u64) -> Result<Media, String> {
    let ext = ext(file);
    let mime = match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    };
    let (data, mime, converted) = match mime {
        Some(mime) if bytes <= IMAGE_MAX => {
            (fs::read(file).map_err(|e| format!("{shown}: {e}"))?, mime, false)
        }
        _ => {
            let scratch = Scratch::new()?;
            let out = scratch.file("image.jpg");
            ffmpeg(
                &["-i".as_ref(), file.as_os_str()],
                &["-frames:v", "1", "-vf", &shrink(), "-q:v", "3"],
                &out,
                shown,
            )?;
            (fs::read(&out).map_err(|e| format!("{shown}: {e}"))?, "image/jpeg", true)
        }
    };
    let how = if converted {
        format!("shrunk to a JPEG of {}", size(data.len() as u64))
    } else {
        "attached as it is".to_string()
    };
    let note = format!("{shown}: image, {}, {how}", size(bytes));
    Ok(Media { parts: vec![label(format!("{shown}:")), picture(mime, &data)], note })
}

fn audio(file: &Path, shown: &str, bytes: u64) -> Result<Media, String> {
    let ext = ext(file);
    let (data, format, converted) = if matches!(ext.as_str(), "wav" | "mp3") && bytes <= AUDIO_MAX {
        (fs::read(file).map_err(|e| format!("{shown}: {e}"))?, ext, false)
    } else {
        let scratch = Scratch::new()?;
        let out = scratch.file("audio.wav");
        let seconds = AUDIO_SECONDS.to_string();
        ffmpeg(
            &["-i".as_ref(), file.as_os_str()],
            &["-vn", "-t", &seconds, "-ac", "1", "-ar", "16000"],
            &out,
            shown,
        )?;
        (fs::read(&out).map_err(|e| format!("{shown}: {e}"))?, "wav".to_string(), true)
    };
    let how = if converted {
        format!("converted to mono 16 kHz wav, the first {AUDIO_SECONDS} seconds at most")
    } else {
        "attached as it is".to_string()
    };
    let note = format!("{shown}: audio, {}, {how}", size(bytes));
    let part = json!({
        "type": "input_audio",
        "input_audio": { "data": STANDARD.encode(&data), "format": format },
    });
    Ok(Media { parts: vec![label(format!("{shown}:")), part], note })
}

fn video(file: &Path, shown: &str, bytes: u64, frames: usize) -> Result<Media, String> {
    let length = duration(file).unwrap_or(0.0);
    let count = if length > 0.0 { frames } else { 1 };
    let scratch = Scratch::new()?;
    let mut parts = vec![label(format!("{shown}: {count} frames sampled evenly from the video:"))];
    let mut times = Vec::new();
    for i in 0..count {
        let at = if length > 0.0 { length * (i as f64 + 0.5) / count as f64 } else { 0.0 };
        let out = scratch.file(&format!("frame{i}.jpg"));
        let seek = format!("{at:.3}");
        ffmpeg(
            &["-ss".as_ref(), seek.as_ref(), "-i".as_ref(), file.as_os_str()],
            &["-frames:v", "1", "-vf", &shrink(), "-q:v", "4"],
            &out,
            shown,
        )?;
        let data = fs::read(&out).map_err(|e| format!("{shown}: no frame at {at:.1}s ({e})"))?;
        parts.push(label(format!("frame {} at {at:.1}s", i + 1)));
        parts.push(picture("image/jpeg", &data));
        times.push(format!("{at:.1}s"));
    }
    let note = format!(
        "{shown}: video, {}, {}. {count} frames attached, at {}. The sound is not included; \
         to hear it, extract it with ffmpeg and read the audio file. Ask for more frames with `frames` (up to {FRAMES_MAX}).",
        size(bytes),
        if length > 0.0 { format!("{length:.1}s long") } else { "length unknown".into() },
        times.join(", "),
    );
    Ok(Media { note, parts })
}

fn shrink() -> String {
    format!(
        "scale='min({EDGE},iw)':'min({EDGE},ih)':force_original_aspect_ratio=decrease:force_divisible_by=2"
    )
}

fn duration(file: &Path) -> Option<f64> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(file)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok().filter(|d: &f64| d.is_finite())
}

fn ffmpeg(
    input: &[&std::ffi::OsStr],
    options: &[&str],
    out: &Path,
    shown: &str,
) -> Result<(), String> {
    let run = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(input)
        .args(options)
        .arg(out)
        .output();
    match run {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("{shown} needs ffmpeg to be read, and ffmpeg is not installed"))
        }
        Err(e) => Err(format!("ffmpeg: {e}")),
        Ok(o) if !o.status.success() => Err(format!(
            "ffmpeg could not read {shown}: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Ok(_) => Ok(()),
    }
}

/// A folder for ffmpeg's output that goes away when it is dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch, String> {
        let dir = std::env::temp_dir().join(format!("foreman-media-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        Ok(Scratch(dir))
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    fn make(dir: &Path, name: &str, args: &[&str]) -> bool {
        Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y"])
            .args(args)
            .arg(dir.join(name))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn open(dir: &tempfile::TempDir, args: Value) -> Option<Result<Media, String>> {
        read(&Workspace::open(dir.path()).unwrap(), &args)
    }

    #[test]
    fn kinds_come_from_the_extension() {
        assert_eq!(kind("a/b.PNG"), Some(Kind::Image));
        assert_eq!(kind("x.mp3"), Some(Kind::Audio));
        assert_eq!(kind("x.mkv"), Some(Kind::Video));
        assert_eq!(kind("x.rs"), None);
        assert_eq!(kind("png"), None);
    }

    #[test]
    fn text_files_are_left_to_read_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open(&dir, json!({"path": "a.rs"})).is_none());
    }

    #[test]
    fn a_small_picture_is_attached_as_it_is() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.png"), b"not really a png").unwrap();
        let m = open(&dir, json!({"path": "a.png"})).unwrap().unwrap();
        assert!(m.note.contains("a.png: image") && m.note.contains("as it is"), "{}", m.note);
        let url = m.parts[1]["image_url"]["url"].as_str().unwrap();
        assert_eq!(url, format!("data:image/png;base64,{}", STANDARD.encode(b"not really a png")));
    }

    #[test]
    fn a_wav_is_attached_as_audio() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.wav"), b"RIFF").unwrap();
        let m = open(&dir, json!({"path": "a.wav"})).unwrap().unwrap();
        assert_eq!(m.parts[1]["type"], "input_audio");
        assert_eq!(m.parts[1]["input_audio"]["format"], "wav");
        assert_eq!(m.parts[1]["input_audio"]["data"], STANDARD.encode(b"RIFF"));
    }

    #[test]
    fn missing_files_and_folders_are_explained() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("d.png")).unwrap();
        assert!(open(&dir, json!({"path": "nope.png"})).unwrap().unwrap_err().contains("nope.png"));
        assert!(open(&dir, json!({"path": "d.png"})).unwrap().unwrap_err().contains("folder"));
    }

    #[test]
    fn paths_outside_the_worktree_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let e = open(&dir, json!({"path": "/etc/hostname.png"})).unwrap().unwrap_err();
        assert!(!e.is_empty());
    }

    #[test]
    fn other_picture_formats_and_big_ones_are_converted() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(make(
            dir.path(),
            "a.bmp",
            &["-f", "lavfi", "-i", "color=red:s=64x48", "-frames:v", "1"]
        ));
        let m = open(&dir, json!({"path": "a.bmp"})).unwrap().unwrap();
        assert!(m.note.contains("shrunk to a JPEG"), "{}", m.note);
        assert!(
            m.parts[1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,")
        );
    }

    #[test]
    fn other_sound_formats_become_wav() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(make(
            dir.path(),
            "a.flac",
            &["-f", "lavfi", "-i", "sine=frequency=440:duration=1"]
        ));
        let m = open(&dir, json!({"path": "a.flac"})).unwrap().unwrap();
        assert!(m.note.contains("converted"), "{}", m.note);
        assert_eq!(m.parts[1]["input_audio"]["format"], "wav");
        let wav = STANDARD.decode(m.parts[1]["input_audio"]["data"].as_str().unwrap()).unwrap();
        assert_eq!(&wav[..4], b"RIFF");
    }

    #[test]
    fn a_video_becomes_frames() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        assert!(make(
            dir.path(),
            "a.mp4",
            &["-f", "lavfi", "-i", "testsrc=duration=4:size=128x96:rate=10", "-pix_fmt", "yuv420p"]
        ));
        let m = open(&dir, json!({"path": "a.mp4", "frames": 3})).unwrap().unwrap();
        assert!(m.note.contains("3 frames attached"), "{}", m.note);
        let pictures = m.parts.iter().filter(|p| p["type"] == "image_url").count();
        assert_eq!(pictures, 3);
        assert!(m.note.contains("4.0s long"), "{}", m.note);
    }

    #[test]
    fn a_broken_video_reports_ffmpeg() {
        if !have_ffmpeg() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.mp4"), b"junk").unwrap();
        let e = open(&dir, json!({"path": "a.mp4"})).unwrap().unwrap_err();
        assert!(e.contains("ffmpeg could not read a.mp4"), "{e}");
    }
}
