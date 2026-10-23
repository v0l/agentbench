//! What the operating system keeps for itself on the screen.

/// How far down the window's content must start, in points: the height of the status bar and
/// camera cutout on a phone, which the app is drawn underneath. Nothing on other platforms.
#[cfg(target_os = "android")]
pub fn top_inset(ctx: &egui::Context) -> f32 {
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
        let name = env.new_string("status_bar_height")?;
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
        log::warn!("status bar height: {e}");
        0
    });
    static LAST: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
    if LAST.swap(px, std::sync::atomic::Ordering::Relaxed) != px {
        log::info!("status bar inset: {px}px at {} px per point", ctx.pixels_per_point());
    }
    px.max(0) as f32 / ctx.pixels_per_point()
}

#[cfg(not(target_os = "android"))]
pub fn top_inset(_ctx: &egui::Context) -> f32 {
    0.0
}
