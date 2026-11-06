//! What the operating system keeps for itself on the screen.

/// How tall one of the system's bars is, in points: a size the system keeps as a resource.
#[cfg(target_os = "android")]
fn bar_height(ctx: &egui::Context, resource: &str) -> f32 {
    use jni::{jni_sig, jni_str};
    let px = crate::voice::android::with_activity(|env, context| {
        // The context handed over is not always an Activity, so there is no window to ask for
        // insets; the status bar's height is a resource of the system that any Context can read.
        let resources = env
            .call_method(
                context,
                jni_str!("getResources"),
                jni_sig!("()Landroid/content/res/Resources;"),
                &[],
            )?
            .l()?;
        let name = env.new_string(resource)?;
        let kind = env.new_string("dimen")?;
        let package = env.new_string("android")?;
        let id = env
            .call_method(
                &resources,
                jni_str!("getIdentifier"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I"),
                &[(&name).into(), (&kind).into(), (&package).into()],
            )?
            .i()?;
        if id == 0 {
            return Ok(0);
        }
        env.call_method(
            &resources,
            jni_str!("getDimensionPixelSize"),
            jni_sig!("(I)I"),
            &[id.into()],
        )?
        .i()
    })
    .unwrap_or_else(|e| {
        log::warn!("{resource}: {e}");
        0
    });
    px.max(0) as f32 / ctx.pixels_per_point()
}

/// How far down the window's content must start, in points: the height of the status bar and
/// camera cutout on a phone, which the app is drawn underneath. Nothing on other platforms.
#[cfg(target_os = "android")]
pub fn top_inset(ctx: &egui::Context) -> f32 {
    bar_height(ctx, "status_bar_height")
}

/// How far up from the bottom the content must stop, in points: the gesture or button bar.
#[cfg(target_os = "android")]
pub fn bottom_inset(ctx: &egui::Context) -> f32 {
    bar_height(ctx, "navigation_bar_gesture_height")
}

#[cfg(not(target_os = "android"))]
pub fn bottom_inset(_ctx: &egui::Context) -> f32 {
    0.0
}

#[cfg(not(target_os = "android"))]
pub fn top_inset(_ctx: &egui::Context) -> f32 {
    0.0
}

/// The link the app was opened with, if it was opened from one: a scanned pairing code.
#[cfg(target_os = "android")]
pub fn launch_link() -> Option<String> {
    use jni::{jni_sig, jni_str};
    crate::voice::android::with_activity(|env, activity| {
        let intent = env
            .call_method(
                activity,
                jni_str!("getIntent"),
                jni_sig!("()Landroid/content/Intent;"),
                &[],
            )?
            .l()?;
        if intent.is_null() {
            return Ok(None);
        }
        let data = env
            .call_method(&intent, jni_str!("getDataString"), jni_sig!("()Ljava/lang/String;"), &[])?
            .l()?;
        if data.is_null() {
            return Ok(None);
        }
        let data = env.cast_local::<jni::objects::JString>(data)?;
        Ok(Some(data.try_to_string(env)?))
    })
    .ok()
    .flatten()
}

#[cfg(not(target_os = "android"))]
pub fn launch_link() -> Option<String> {
    None
}

/// How tall the on screen keyboard is, in points, or 0 when it is away. The window is not
/// resized for it, so the app leaves that much room at the bottom itself.
#[cfg(target_os = "android")]
pub fn keyboard_height(ctx: &egui::Context) -> f32 {
    use jni::{jni_sig, jni_str};
    /// `WindowInsets.Type.ime()`.
    const IME: i32 = 8;
    let px = crate::voice::android::with_activity(|env, activity| {
        let window = env
            .call_method(activity, jni_str!("getWindow"), jni_sig!("()Landroid/view/Window;"), &[])?
            .l()?;
        let decor = env
            .call_method(&window, jni_str!("getDecorView"), jni_sig!("()Landroid/view/View;"), &[])?
            .l()?;
        let insets = env
            .call_method(
                &decor,
                jni_str!("getRootWindowInsets"),
                jni_sig!("()Landroid/view/WindowInsets;"),
                &[],
            )?
            .l()?;
        if insets.is_null() {
            return Ok(0);
        }
        let ime = env
            .call_method(
                &insets,
                jni_str!("getInsets"),
                jni_sig!("(I)Landroid/graphics/Insets;"),
                &[IME.into()],
            )?
            .l()?;
        env.get_field(&ime, jni_str!("bottom"), jni_sig!("I"))?.i()
    })
    .unwrap_or(0);
    px.max(0) as f32 / ctx.pixels_per_point()
}

#[cfg(not(target_os = "android"))]
pub fn keyboard_height(_ctx: &egui::Context) -> f32 {
    0.0
}

static STAND_IN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static PENDING_PASTE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[doc(hidden)]
pub fn stand_in_clipboard(text: Option<&str>) {
    if let Ok(mut held) = STAND_IN.lock() {
        *held = text.map(str::to_string);
    }
}

fn stand_in() -> Option<String> {
    STAND_IN.lock().ok()?.clone()
}

pub fn owns_clipboard() -> bool {
    cfg!(target_os = "android") || stand_in().is_some()
}

pub fn read_clipboard() -> Option<String> {
    if let Some(text) = stand_in() {
        return Some(text);
    }
    system_clipboard_text()
}

pub fn write_clipboard(text: &str) {
    if stand_in().is_some() {
        stand_in_clipboard(Some(text));
    }
    set_system_clipboard_text(text);
}

pub fn queue_paste(ctx: &egui::Context) {
    let Some(text) = read_clipboard().map(|t| t.replace("\r\n", "\n")) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    if let Ok(mut pending) = PENDING_PASTE.lock() {
        *pending = Some(text);
    }
    ctx.request_repaint();
}

pub fn deliver_paste(ctx: &egui::Context) {
    let Some(text) = PENDING_PASTE.lock().ok().and_then(|mut p| p.take()) else {
        return;
    };
    ctx.input_mut(|i| i.events.push(egui::Event::Paste(text)));
}

pub fn copy_what_egui_copied(ctx: &egui::Context) {
    if !owns_clipboard() {
        return;
    }
    let copied = ctx.output(|o| {
        o.commands.iter().rev().find_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        })
    });
    if let Some(text) = copied {
        write_clipboard(&text);
    }
}

#[cfg(target_os = "android")]
fn clipboard_manager<'j>(
    env: &mut jni::Env<'j>,
    activity: &jni::objects::JObject<'j>,
) -> jni::errors::Result<jni::objects::JObject<'j>> {
    use jni::{jni_sig, jni_str};
    let name = env.new_string("clipboard")?;
    env.call_method(
        activity,
        jni_str!("getSystemService"),
        jni_sig!("(Ljava/lang/String;)Ljava/lang/Object;"),
        &[(&name).into()],
    )?
    .l()
}

#[cfg(target_os = "android")]
fn system_clipboard_text() -> Option<String> {
    use jni::{jni_sig, jni_str};
    crate::voice::android::with_activity(|env, activity| {
        let manager = clipboard_manager(env, activity)?;
        let clip = env
            .call_method(
                &manager,
                jni_str!("getPrimaryClip"),
                jni_sig!("()Landroid/content/ClipData;"),
                &[],
            )?
            .l()?;
        if clip.is_null() {
            return Ok(None);
        }
        let count = env.call_method(&clip, jni_str!("getItemCount"), jni_sig!("()I"), &[])?.i()?;
        if count < 1 {
            return Ok(None);
        }
        let item = env
            .call_method(
                &clip,
                jni_str!("getItemAt"),
                jni_sig!("(I)Landroid/content/ClipData$Item;"),
                &[0.into()],
            )?
            .l()?;
        let text = env
            .call_method(
                &item,
                jni_str!("coerceToText"),
                jni_sig!("(Landroid/content/Context;)Ljava/lang/CharSequence;"),
                &[activity.into()],
            )?
            .l()?;
        if text.is_null() {
            return Ok(None);
        }
        let text = env
            .call_method(&text, jni_str!("toString"), jni_sig!("()Ljava/lang/String;"), &[])?
            .l()?;
        let text = env.cast_local::<jni::objects::JString>(text)?;
        Ok(Some(text.try_to_string(env)?))
    })
    .ok()
    .flatten()
}

#[cfg(not(target_os = "android"))]
fn system_clipboard_text() -> Option<String> {
    None
}

#[cfg(target_os = "android")]
fn set_system_clipboard_text(text: &str) {
    use jni::{jni_sig, jni_str};
    let result = crate::voice::android::with_activity(|env, activity| {
        let manager = clipboard_manager(env, activity)?;
        let label = env.new_string("agentbench")?;
        let body = env.new_string(text)?;
        let clip = env
            .call_static_method(
                jni_str!("android/content/ClipData"),
                jni_str!("newPlainText"),
                jni_sig!("(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;"),
                &[(&label).into(), (&body).into()],
            )?
            .l()?;
        env.call_method(
            &manager,
            jni_str!("setPrimaryClip"),
            jni_sig!("(Landroid/content/ClipData;)V"),
            &[(&clip).into()],
        )?;
        Ok(())
    });
    if let Err(e) = result {
        log::warn!("copy: {e}");
    }
}

#[cfg(not(target_os = "android"))]
fn set_system_clipboard_text(_text: &str) {}

#[cfg(target_os = "android")]
pub fn want_text_input(on: bool) {
    use jni::{jni_sig, jni_str};
    static LAST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if LAST.swap(on, std::sync::atomic::Ordering::Relaxed) == on {
        return;
    }
    let result = crate::voice::android::with_activity(|env, activity| {
        env.call_method(activity, jni_str!("wantTextInput"), jni_sig!("(Z)V"), &[on.into()])?;
        Ok(())
    });
    if let Err(e) = result {
        log::warn!("text input: {e}");
    }
}

#[cfg(not(target_os = "android"))]
pub fn want_text_input(_on: bool) {}

#[cfg(target_os = "android")]
fn take_ime_lines() -> Vec<String> {
    use jni::{jni_sig, jni_str};
    let mut lines = Vec::new();
    loop {
        let line = crate::voice::android::with_activity(|env, activity| {
            let got = env
                .call_method(
                    activity,
                    jni_str!("takeImeLine"),
                    jni_sig!("()Ljava/lang/String;"),
                    &[],
                )?
                .l()?;
            if got.is_null() {
                return Ok(None);
            }
            let got = env.cast_local::<jni::objects::JString>(got)?;
            Ok(Some(got.try_to_string(env)?))
        })
        .ok()
        .flatten();
        match line {
            Some(line) => lines.push(line),
            None => return lines,
        }
    }
}

#[cfg(not(target_os = "android"))]
fn take_ime_lines() -> Vec<String> {
    Vec::new()
}

pub fn feed_ime(raw: &mut egui::RawInput) {
    for line in take_ime_lines() {
        raw.events.extend(ime_events(&line));
    }
}

pub fn ime_events(line: &str) -> Vec<egui::Event> {
    let Some((kind, rest)) = line.split_once(':') else {
        return Vec::new();
    };
    match kind {
        "preedit" => vec![egui::Event::Ime(egui::ImeEvent::Preedit {
            text: rest.to_string(),
            active_range_chars: None,
        })],
        "commit" => vec![egui::Event::Ime(egui::ImeEvent::Commit(rest.to_string()))],
        "delete" => {
            let count = rest.parse::<usize>().unwrap_or(0).min(64);
            (0..count)
                .flat_map(|_| {
                    [true, false].map(|pressed| egui::Event::Key {
                        key: egui::Key::Backspace,
                        physical_key: None,
                        pressed,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    })
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clipboard_chip_commit_from_the_keyboard_becomes_a_commit_event() {
        assert_eq!(
            ime_events("commit:chip text 42"),
            vec![egui::Event::Ime(egui::ImeEvent::Commit("chip text 42".into()))]
        );
    }

    #[test]
    fn text_with_a_colon_keeps_it_and_a_delete_is_that_many_backspaces() {
        assert_eq!(
            ime_events("commit:a:b"),
            vec![egui::Event::Ime(egui::ImeEvent::Commit("a:b".into()))]
        );
        assert_eq!(ime_events("delete:2").len(), 4);
    }
}
