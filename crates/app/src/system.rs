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

/// What the app asked of the on screen keyboard, kept for tests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KeyboardCall {
    Show,
    Hide,
    Quiet,
    Selection,
    Restart,
}

/// How the keyboard learns that the focused field now holds `now`: nothing when it already
/// holds it, its copy of the text alone while it is away, the selection when only that moved,
/// and a restart only while it is up and the text itself changed.
pub fn keyboard_sync(shown: bool, held: Option<&ImeText>, now: &ImeText) -> Option<KeyboardCall> {
    if held == Some(now) {
        return None;
    }
    if !shown {
        return Some(KeyboardCall::Quiet);
    }
    if held.is_some_and(|k| k.text == now.text) {
        Some(KeyboardCall::Selection)
    } else {
        Some(KeyboardCall::Restart)
    }
}

fn record(ctx: &egui::Context, call: KeyboardCall) {
    with_ime(ctx, |ime| {
        if ime.calls.len() >= 256 {
            ime.calls.remove(0);
        }
        ime.calls.push(call);
    });
}

/// What the app asked of the keyboard since the last call.
#[doc(hidden)]
pub fn keyboard_calls(ctx: &egui::Context) -> Vec<KeyboardCall> {
    with_ime(ctx, |ime| std::mem::take(&mut ime.calls))
}

/// Whether the on screen keyboard is up, as the system says when that changes.
#[doc(hidden)]
pub fn keyboard_visible(ctx: &egui::Context, visible: bool) {
    with_ime(ctx, |ime| ime.visible = visible);
}

fn want_text_input(ctx: &egui::Context, on: bool) {
    with_ime(ctx, |ime| {
        ime.asked = on;
        ime.visible = on;
    });
    record(ctx, if on { KeyboardCall::Show } else { KeyboardCall::Hide });
    ask_for_text_input(on);
}

#[cfg(target_os = "android")]
fn ask_for_text_input(on: bool) {
    use jni::{jni_sig, jni_str};
    let result = crate::voice::android::with_activity(|env, activity| {
        env.call_method(activity, jni_str!("wantTextInput"), jni_sig!("(Z)V"), &[on.into()])?;
        Ok(())
    });
    if let Err(e) = result {
        log::warn!("text input: {e}");
    }
}

#[cfg(not(target_os = "android"))]
fn ask_for_text_input(_on: bool) {}

/// The text the keyboard holds or should hold, with its selection in characters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImeText {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

impl ImeText {
    pub fn parse(line: &str) -> Option<Self> {
        let (head, text) = line.split_once('\n')?;
        let (start, end) = head.split_once(' ')?;
        Some(Self { text: text.to_string(), start: start.parse().ok()?, end: end.parse().ok()? })
    }

    fn line(&self) -> String {
        format!("{} {}\n{}", self.start, self.end, self.text)
    }
}

/// Characters `start..end` of the old text become `insert`.
#[derive(Clone, Debug, PartialEq)]
pub struct ImeEdit {
    pub start: usize,
    pub end: usize,
    pub insert: String,
}

pub fn ime_edit(old: &ImeText, new: &ImeText) -> Option<ImeEdit> {
    let a: Vec<char> = old.text.chars().collect();
    let b: Vec<char> = new.text.chars().collect();
    let cap = old.start.min(old.end).min(new.start).min(new.end).min(a.len()).min(b.len());
    let mut prefix = 0;
    while prefix < cap && a[prefix] == b[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < a.len() - prefix
        && suffix < b.len() - prefix
        && a[a.len() - 1 - suffix] == b[b.len() - 1 - suffix]
    {
        suffix += 1;
    }
    if a.len() == b.len() && prefix + suffix == a.len() {
        return None;
    }
    Some(ImeEdit {
        start: prefix,
        end: a.len() - suffix,
        insert: b[prefix..b.len() - suffix].iter().collect(),
    })
}

fn key(key: egui::Key) -> [egui::Event; 2] {
    [true, false].map(|pressed| egui::Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    })
}

/// What a field whose selection is already `start..end` of the edit gets told.
pub fn ime_events(edit: &ImeEdit) -> Vec<egui::Event> {
    if edit.insert.is_empty() {
        key(egui::Key::Backspace).to_vec()
    } else if edit.insert == "\n" && edit.start == edit.end {
        key(egui::Key::Enter).to_vec()
    } else if edit.insert.contains('\n') {
        vec![egui::Event::Paste(edit.insert.clone())]
    } else {
        vec![egui::Event::Text(edit.insert.clone())]
    }
}

/// What a widget that is not a known field gets told: the cursor is taken to sit at the end of
/// the edit, as it does while typing.
pub fn ime_blind_events(edit: &ImeEdit) -> Vec<egui::Event> {
    let deleted = (edit.end - edit.start).min(64);
    let mut events: Vec<_> = (0..deleted).flat_map(|_| key(egui::Key::Backspace)).collect();
    if !edit.insert.is_empty() {
        events.extend(ime_events(&ImeEdit { start: 0, end: 0, insert: edit.insert.clone() }));
    }
    events
}

#[derive(Clone, Default)]
struct Ime {
    keyboard: Option<ImeText>,
    field: Option<(egui::Id, ImeText)>,
    select: Option<(usize, usize)>,
    drawn: Option<(egui::Id, String)>,
    inbox: Option<String>,
    asked: bool,
    visible: bool,
    tapped: bool,
    calls: Vec<KeyboardCall>,
}

static IME_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static IME_CTX: std::sync::Mutex<Option<egui::Context>> = std::sync::Mutex::new(None);

fn remember_ime_ctx(ctx: &egui::Context) {
    let mut slot = IME_CTX.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref() != Some(ctx) {
        *slot = Some(ctx.clone());
    }
}

fn ime_id() -> egui::Id {
    egui::Id::new("system-ime")
}

fn with_ime<R>(ctx: &egui::Context, f: impl FnOnce(&mut Ime) -> R) -> R {
    ctx.data_mut(|d| f(d.get_temp_mut_or_default::<Ime>(ime_id())))
}

/// Hand the app a keyboard snapshot as the Android activity would, `start end\ntext`.
#[doc(hidden)]
pub fn ime_from_keyboard(ctx: &egui::Context, line: &str) {
    with_ime(ctx, |ime| ime.inbox = Some(line.to_string()));
    ctx.request_repaint();
}

/// What the keyboard was last told the focused field holds.
#[doc(hidden)]
pub fn ime_keyboard(ctx: &egui::Context) -> Option<ImeText> {
    with_ime(ctx, |ime| ime.keyboard.clone())
}

fn ime_wake() {
    IME_PENDING.store(true, std::sync::atomic::Ordering::Release);
    let ctx = IME_CTX.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(ctx) = ctx {
        ctx.request_repaint();
    }
}

/// A text field calls this each frame with what it shows. Only a tap on it brings up the
/// keyboard; focus it gets from the app, as when speech lands in it, leaves the keyboard be.
pub fn ime_field(response: &egui::Response, text: &str) {
    if !response.has_focus() {
        return;
    }
    let drawn = Some((response.id, text.to_string()));
    let tapped = response.clicked()
        || (response.contains_pointer() && response.ctx.input(|i| i.pointer.primary_released()));
    with_ime(&response.ctx, |ime| {
        ime.drawn = drawn;
        ime.tapped |= tapped;
    });
}

fn set_selection(ctx: &egui::Context, id: egui::Id, start: usize, end: usize) {
    use egui::text::{CCursor, CCursorRange};
    let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(start), CCursor::new(end))));
    state.store(ctx, id);
}

fn selection(ctx: &egui::Context, id: egui::Id, len: usize) -> (usize, usize) {
    egui::text_edit::TextEditState::load(ctx, id)
        .and_then(|s| s.cursor.char_range())
        .map(|r| {
            let [a, b] = r.sorted_cursors();
            (a.index.0.min(len), b.index.0.min(len))
        })
        .unwrap_or((len, len))
}

fn take_keyboard(ime: &mut Ime) -> Option<ImeText> {
    let line = ime.inbox.take().or_else(|| {
        IME_PENDING.swap(false, std::sync::atomic::Ordering::AcqRel).then(take_ime).flatten()
    });
    line.as_deref().and_then(ImeText::parse)
}

/// Turn what the keyboard typed since the last frame into input for the focused field. Runs at
/// the start of a frame, before any field is drawn.
pub fn ime_before_frame(ctx: &egui::Context) {
    if cfg!(target_os = "android") {
        remember_ime_ctx(ctx);
    }
    let mut ime = with_ime(ctx, std::mem::take);
    ime_apply(ctx, &mut ime);
    with_ime(ctx, |slot| *slot = ime);
}

fn ime_apply(ctx: &egui::Context, ime: &mut Ime) {
    if let (Some((start, end)), Some((id, _))) = (ime.select.take(), &ime.field) {
        set_selection(ctx, *id, start, end);
    }
    let Some(new) = take_keyboard(ime) else { return };
    let base = match &ime.field {
        Some((_, field)) => field.clone(),
        None => ime.keyboard.clone().unwrap_or_default(),
    };
    ime.keyboard = Some(new.clone());
    let edit = ime_edit(&base, &new);
    let Some((id, _)) = ime.field else {
        if let Some(edit) = edit {
            ctx.input_mut(|i| i.events.extend(ime_blind_events(&edit)));
        }
        return;
    };
    let Some(edit) = edit else {
        if (new.start, new.end) != (base.start, base.end) {
            set_selection(ctx, id, new.start, new.end);
        }
        return;
    };
    set_selection(ctx, id, edit.start, edit.end);
    ctx.input_mut(|i| i.events.extend(ime_events(&edit)));
    let after = edit.start + edit.insert.chars().count();
    if (new.start, new.end) != (after, after) {
        ime.select = Some((new.start, new.end));
        ctx.request_repaint();
    }
}

/// Tell the keyboard what the focused field holds when that is not what it typed: a field just
/// focused, a draft sent or recalled, a tap that moved the cursor.
pub fn ime_after_frame(ctx: &egui::Context) {
    if cfg!(target_os = "android") {
        ctx.output_mut(|o| o.ime = None);
    }
    let Some((id, text)) = with_ime(ctx, |ime| ime.drawn.take()) else {
        let asked = with_ime(ctx, |ime| {
            ime.field = None;
            ime.tapped = false;
            ime.asked
        });
        if asked {
            want_text_input(ctx, false);
        }
        return;
    };
    let len = text.chars().count();
    let (start, end) = selection(ctx, id, len);
    let now = ImeText { text, start, end };
    let (call, tapped) = with_ime(ctx, |ime| {
        ime.field = Some((id, now.clone()));
        let tapped = std::mem::take(&mut ime.tapped) && !(ime.asked && ime.visible);
        if ime.select.is_some() {
            return (None, tapped);
        }
        let call = keyboard_sync(ime.asked && ime.visible, ime.keyboard.as_ref(), &now);
        ime.keyboard = Some(now.clone());
        (call, tapped)
    });
    if let Some(call) = call {
        record(ctx, call);
        sync_keyboard(&now, call);
    }
    if tapped {
        want_text_input(ctx, true);
    }
}

#[cfg(target_os = "android")]
fn take_ime() -> Option<String> {
    use jni::{jni_sig, jni_str};
    crate::voice::android::with_activity(|env, activity| {
        let got = env
            .call_method(activity, jni_str!("takeIme"), jni_sig!("()Ljava/lang/String;"), &[])?
            .l()?;
        if got.is_null() {
            return Ok(None);
        }
        let got = env.cast_local::<jni::objects::JString>(got)?;
        Ok(Some(got.try_to_string(env)?))
    })
    .ok()
    .flatten()
}

#[cfg(not(target_os = "android"))]
fn take_ime() -> Option<String> {
    None
}

#[cfg(target_os = "android")]
fn sync_keyboard(now: &ImeText, call: KeyboardCall) {
    use jni::{jni_sig, jni_str};
    let mode = match call {
        KeyboardCall::Selection => 1,
        KeyboardCall::Restart => 2,
        _ => 0,
    };
    let result = crate::voice::android::with_activity(|env, activity| {
        let line = env.new_string(now.line())?;
        env.call_method(
            activity,
            jni_str!("syncIme"),
            jni_sig!("(Ljava/lang/String;I)V"),
            &[(&line).into(), mode.into()],
        )?;
        Ok(())
    });
    if let Err(e) = result {
        log::warn!("keyboard sync: {e}");
    }
}

#[cfg(not(target_os = "android"))]
fn sync_keyboard(_now: &ImeText, _call: KeyboardCall) {}

#[cfg(target_os = "android")]
pub fn network_generation() -> i64 {
    use jni::{jni_sig, jni_str};
    crate::voice::android::with_activity(|env, activity| {
        env.call_method(activity, jni_str!("networkGeneration"), jni_sig!("()J"), &[])?.j()
    })
    .unwrap_or(0)
}

#[cfg(not(target_os = "android"))]
pub fn network_generation() -> i64 {
    0
}

#[cfg(target_os = "android")]
extern "system" fn ime_wake_from_java(_env: *mut jni::sys::JNIEnv, _this: jni::sys::jobject) {
    ime_wake();
}

/// Let the activity wake the frame loop when the keyboard has typed something.
#[cfg(target_os = "android")]
pub fn register_ime_wake() {
    use jni::{NativeMethod, jni_str};
    let result = crate::voice::android::with_activity(|env, activity| {
        let class = env.get_object_class(activity)?;
        let method = unsafe {
            NativeMethod::from_raw_parts(
                jni_str!("imeWake"),
                jni_str!("()V"),
                ime_wake_from_java as *mut std::ffi::c_void,
            )
        };
        unsafe { env.register_native_methods(&class, &[method]) }
    });
    if let Err(e) = result {
        log::warn!("keyboard wake: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, cursor: usize) -> ImeText {
        ImeText { text: text.into(), start: cursor, end: cursor }
    }

    fn edit(start: usize, end: usize, insert: &str) -> Option<ImeEdit> {
        Some(ImeEdit { start, end, insert: insert.into() })
    }

    #[test]
    fn the_keyboard_wakes_the_newest_window_after_a_relaunch() {
        let woken = |ctx: &egui::Context| {
            let hit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = hit.clone();
            ctx.set_request_repaint_callback(move |_| {
                flag.store(true, std::sync::atomic::Ordering::Relaxed)
            });
            hit
        };
        let first = egui::Context::default();
        let first_woken = woken(&first);
        remember_ime_ctx(&first);
        let second = egui::Context::default();
        let second_woken = woken(&second);
        remember_ime_ctx(&second);
        ime_wake();
        assert!(second_woken.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!first_woken.load(std::sync::atomic::Ordering::Relaxed));
        IME_PENDING.store(false, std::sync::atomic::Ordering::Release);
    }

    #[test]
    fn text_from_outside_the_keyboard_restarts_it_only_while_it_is_up() {
        let held = at("fix", 3);
        let spoken = at("fix the bug", 11);
        let moved = at("fix", 1);
        assert_eq!(keyboard_sync(false, Some(&held), &spoken), Some(KeyboardCall::Quiet));
        assert_eq!(keyboard_sync(false, Some(&held), &moved), Some(KeyboardCall::Quiet));
        assert_eq!(keyboard_sync(false, None, &spoken), Some(KeyboardCall::Quiet));
        assert_eq!(keyboard_sync(true, Some(&held), &moved), Some(KeyboardCall::Selection));
        assert_eq!(keyboard_sync(true, Some(&held), &spoken), Some(KeyboardCall::Restart));
        assert_eq!(keyboard_sync(true, Some(&spoken), &spoken), None);
        assert_eq!(keyboard_sync(false, Some(&spoken), &spoken), None);
    }

    #[test]
    fn an_echo_of_what_the_keyboard_typed_asks_nothing_of_it() {
        let ctx = egui::Context::default();
        let mut text = String::new();
        for n in 0..3 {
            if n == 1 {
                ime_from_keyboard(&ctx, "3 3\nhey");
            }
            ctx.run_ui(egui::RawInput::default(), |ui| {
                ime_before_frame(ui.ctx());
                let r = ui.add(egui::TextEdit::singleline(&mut text).id_salt("field"));
                if n == 0 {
                    r.request_focus();
                }
                ime_field(&r, &text);
                ime_after_frame(ui.ctx());
            })
            .textures_delta
            .clear();
        }
        assert_eq!(text, "hey");
        let calls = keyboard_calls(&ctx);
        assert!(
            !calls.contains(&KeyboardCall::Show),
            "focus from the app shows nothing: {calls:?}"
        );
        assert!(!calls.contains(&KeyboardCall::Restart), "{calls:?}");
    }

    #[test]
    fn a_keyboard_snapshot_keeps_newlines_and_colons_in_its_text() {
        assert_eq!(
            ImeText::parse("3 5\na:b\nc"),
            Some(ImeText { text: "a:b\nc".into(), start: 3, end: 5 })
        );
        assert_eq!(ImeText::parse("0 0\n"), Some(at("", 0)));
        assert_eq!(ImeText::parse("junk"), None);
        let round = ImeText { text: "x\ny".into(), start: 1, end: 2 };
        assert_eq!(ImeText::parse(&round.line()), Some(round));
    }

    #[test]
    fn a_clipboard_chip_commit_inserts_its_text_at_the_cursor() {
        let e = ime_edit(&at("see ", 4), &at("see chip text 42", 16));
        assert_eq!(e, edit(4, 4, "chip text 42"));
        assert_eq!(ime_events(&e.unwrap()), vec![egui::Event::Text("chip text 42".into())]);
    }

    #[test]
    fn delete_surrounding_text_before_the_cursor_is_one_backspace_over_that_range() {
        let e = ime_edit(&at("hello", 5), &at("hel", 3)).unwrap();
        assert_eq!(e, ImeEdit { start: 3, end: 5, insert: String::new() });
        assert_eq!(ime_events(&e), key(egui::Key::Backspace).to_vec());
    }

    #[test]
    fn delete_in_code_points_removes_a_whole_emoji() {
        assert_eq!(ime_edit(&at("ok 👍", 4), &at("ok ", 3)), edit(3, 4, ""));
    }

    #[test]
    fn the_del_key_in_the_middle_deletes_the_character_before_the_cursor() {
        assert_eq!(ime_edit(&at("hello", 3), &at("helo", 2)), edit(2, 3, ""));
        assert_eq!(ime_edit(&at("hello", 2), &at("hllo", 1)), edit(1, 2, ""));
    }

    #[test]
    fn forward_delete_removes_the_character_after_the_cursor() {
        assert_eq!(ime_edit(&at("abcc", 2), &at("abc", 2)), edit(2, 3, ""));
    }

    #[test]
    fn backspace_while_composing_shrinks_the_word_one_letter_at_a_time() {
        let mut field = at("say hel", 7);
        for (next, cursor) in [("say he", 6), ("say h", 5), ("say ", 4)] {
            let e = ime_edit(&field, &at(next, cursor)).unwrap();
            assert_eq!(e, ImeEdit { start: cursor, end: cursor + 1, insert: String::new() });
            field = at(next, cursor);
        }
    }

    #[test]
    fn repeated_deletes_from_a_held_key_each_take_one_character() {
        let mut field = at("abcdef", 6);
        while !field.text.is_empty() {
            let shorter: String = field.text.chars().take(field.end - 1).collect();
            let e = ime_edit(&field, &at(&shorter, field.end - 1)).unwrap();
            assert_eq!((e.start, e.end), (field.end - 1, field.end));
            field = at(&shorter, field.end - 1);
        }
    }

    #[test]
    fn deleting_a_selection_replaces_exactly_the_selected_range() {
        let selected = ImeText { text: "one two three".into(), start: 4, end: 7 };
        assert_eq!(ime_edit(&selected, &at("one  three", 4)), edit(4, 7, ""));
        assert_eq!(ime_edit(&selected, &at("one 2 three", 5)), edit(4, 7, "2"));
    }

    #[test]
    fn typing_a_letter_that_repeats_the_next_one_inserts_at_the_cursor() {
        assert_eq!(ime_edit(&at("hell", 3), &at("helll", 4)), edit(3, 3, "l"));
    }

    #[test]
    fn an_autocorrect_swaps_only_the_letters_that_changed() {
        assert_eq!(ime_edit(&at("teh ", 4), &at("the ", 4)), edit(1, 3, "he"));
    }

    #[test]
    fn the_enter_key_is_a_key_press_and_pasted_lines_stay_a_paste() {
        assert_eq!(ime_events(&edit(2, 2, "\n").unwrap()), key(egui::Key::Enter).to_vec());
        assert_eq!(
            ime_events(&edit(2, 2, "a\nb").unwrap()),
            vec![egui::Event::Paste("a\nb".into())]
        );
    }

    #[test]
    fn no_change_and_a_cursor_move_are_not_edits() {
        assert_eq!(ime_edit(&at("same", 2), &at("same", 2)), None);
        assert_eq!(ime_edit(&at("same", 2), &at("same", 4)), None);
    }

    #[test]
    fn a_widget_that_is_not_a_field_gets_backspaces_then_the_text() {
        let events = ime_blind_events(&edit(3, 5, "p").unwrap());
        assert_eq!(events.len(), 5);
        assert_eq!(events[4], egui::Event::Text("p".into()));
    }
}
