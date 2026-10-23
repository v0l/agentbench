//! Telling the Android background service which server to watch, so a chat that needs you or has
//! finished raises a notification while the app is closed. The service itself is Kotlin, under
//! `android/src`; everything here is a no-op on other platforms.

#[cfg(target_os = "android")]
mod android {
    use crate::voice::android::{allowed, ask, with_activity};
    use jni::objects::JObject;
    use jni::{Env, jni_sig, jni_str};

    const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";
    const SERVICE: &str = "io.v0l.foreman.WatchService";

    fn prefs<'j>(env: &mut Env<'j>, ctx: &JObject<'j>) -> jni::errors::Result<JObject<'j>> {
        let name = env.new_string("foreman-sync")?;
        env.call_method(
            ctx,
            jni_str!("getSharedPreferences"),
            jni_sig!("(Ljava/lang/String;I)Landroid/content/SharedPreferences;"),
            &[(&name).into(), 0.into()],
        )?
        .l()
    }

    fn intent<'j>(env: &mut Env<'j>, ctx: &JObject<'j>) -> jni::errors::Result<JObject<'j>> {
        let package = env
            .call_method(ctx, jni_str!("getPackageName"), jni_sig!("()Ljava/lang/String;"), &[])?
            .l()?;
        let class = env.new_string(SERVICE)?;
        let intent = env.new_object(jni_str!("android/content/Intent"), jni_sig!("()V"), &[])?;
        env.call_method(
            &intent,
            jni_str!("setClassName"),
            jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;"),
            &[(&package).into(), (&class).into()],
        )?;
        Ok(intent)
    }

    /// Point the service at a server, or stop it when `on` is false or there is no server.
    pub fn watch(url: &str, token: &str, on: bool) {
        let on = on && !url.trim().is_empty() && !token.trim().is_empty();
        if on && !allowed(POST_NOTIFICATIONS) {
            // Android 13 and later ask first; the answer arrives later, and the next call (the
            // next launch, or toggling the setting) starts the service.
            let _ = ask(POST_NOTIFICATIONS);
        }
        let result = with_activity(|env, ctx| {
            let p = prefs(env, ctx)?;
            let old_url = env.new_string("url")?;
            let none = env.new_string("")?;
            let before = env
                .call_method(
                    &p,
                    jni_str!("getString"),
                    jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                    &[(&old_url).into(), (&none).into()],
                )?
                .l()?;
            let before = env.cast_local::<jni::objects::JString>(before)?.try_to_string(env)?;
            let edit = env
                .call_method(
                    &p,
                    jni_str!("edit"),
                    jni_sig!("()Landroid/content/SharedPreferences$Editor;"),
                    &[],
                )?
                .l()?;
            let put = |env: &mut Env<'_>, k: &str, v: &str| -> jni::errors::Result<()> {
                let (k, v) = (env.new_string(k)?, env.new_string(v)?);
                env.call_method(
                    &edit,
                    jni_str!("putString"),
                    jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Landroid/content/SharedPreferences$Editor;"),
                    &[(&k).into(), (&v).into()],
                )?;
                Ok(())
            };
            put(env, "url", url.trim())?;
            put(env, "token", token.trim())?;
            if before != url.trim() {
                // A different server: its change numbers and chats are not the old one's.
                put(env, "known", "")?;
                let k = env.new_string("cursor")?;
                env.call_method(
                    &edit,
                    jni_str!("putLong"),
                    jni_sig!("(Ljava/lang/String;J)Landroid/content/SharedPreferences$Editor;"),
                    &[(&k).into(), 0i64.into()],
                )?;
            }
            let k = env.new_string("on")?;
            env.call_method(
                &edit,
                jni_str!("putBoolean"),
                jni_sig!("(Ljava/lang/String;Z)Landroid/content/SharedPreferences$Editor;"),
                &[(&k).into(), on.into()],
            )?;
            env.call_method(&edit, jni_str!("apply"), jni_sig!("()V"), &[])?;

            let i = intent(env, ctx)?;
            if on {
                env.call_method(
                    ctx,
                    jni_str!("startForegroundService"),
                    jni_sig!("(Landroid/content/Intent;)Landroid/content/ComponentName;"),
                    &[(&i).into()],
                )?;
            } else {
                env.call_method(
                    ctx,
                    jni_str!("stopService"),
                    jni_sig!("(Landroid/content/Intent;)Z"),
                    &[(&i).into()],
                )?;
            }
            Ok(())
        });
        if let Err(e) = result {
            log::warn!("background watch: {e}");
        }
    }
}

/// Watch `url` in the background, or stop watching.
pub fn watch(url: &str, token: &str, on: bool) {
    #[cfg(target_os = "android")]
    android::watch(url, token, on);
    #[cfg(not(target_os = "android"))]
    let _ = (url, token, on);
}

/// Whether this device can raise notifications while the app is closed.
pub const fn available() -> bool {
    cfg!(target_os = "android")
}
