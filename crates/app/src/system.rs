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
            .call_method(activity, jni_str!("getIntent"), jni_sig!("()Landroid/content/Intent;"), &[])?
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
