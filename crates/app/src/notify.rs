//! Telling the Android background service which server to watch, so a chat that needs you or has
//! finished raises a notification while the app is closed. The service itself is Kotlin, under
//! `android/src`; everything here is a no-op on other platforms.

#[cfg(target_os = "android")]
mod android {
    use crate::voice::android::{allowed, ask, with_activity};
    use jni::objects::JObject;
    use jni::{Env, jni_sig, jni_str};

    const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";
    const SERVICE: &str = "io.v0l.agentbench.WatchService";

    fn prefs<'j>(env: &mut Env<'j>, ctx: &JObject<'j>) -> jni::errors::Result<JObject<'j>> {
        let name = env.new_string("agentbench-sync")?;
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
    pub fn watch(url: &str, token: &str, servers: &str, on: bool, managed: bool) {
        let on = on && !url.trim().is_empty() && !token.trim().is_empty();
        if on && !allowed(POST_NOTIFICATIONS) {
            // Android 13 and later ask first; the answer arrives later, and the next call (the
            // next launch, or toggling the setting) starts the service.
            let _ = ask(POST_NOTIFICATIONS);
        }
        let result = with_activity(|env, ctx| {
            let p = prefs(env, ctx)?;
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
            put(env, "servers", servers)?;
            for (key, value) in [("on", on), ("alert_managed", managed)] {
                let k = env.new_string(key)?;
                env.call_method(
                    &edit,
                    jni_str!("putBoolean"),
                    jni_sig!("(Ljava/lang/String;Z)Landroid/content/SharedPreferences$Editor;"),
                    &[(&k).into(), value.into()],
                )?;
            }
            env.call_method(&edit, jni_str!("commit"), jni_sig!("()Z"), &[])?;

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

/// One workspace the background service watches.
#[derive(Clone, Debug, PartialEq)]
pub struct Watched {
    pub name: String,
    pub url: String,
    pub token: String,
}

/// The workspaces to watch: the first, then each other, skipping any without a url or token and
/// any repeat of a server already listed.
pub fn watch_list(first: Watched, others: impl IntoIterator<Item = Watched>) -> Vec<Watched> {
    let mut out: Vec<Watched> = Vec::new();
    for w in std::iter::once(first).chain(others) {
        let (url, token) = (w.url.trim().trim_end_matches('/'), w.token.trim());
        if url.is_empty() || token.is_empty() || out.iter().any(|o| o.url == url && o.token == token) {
            continue;
        }
        out.push(Watched { name: w.name.trim().to_string(), url: url.to_string(), token: token.to_string() });
    }
    out
}

pub fn watch_json(list: &[Watched]) -> String {
    let items: Vec<_> = list
        .iter()
        .map(|w| serde_json::json!({ "name": w.name, "url": w.url, "token": w.token }))
        .collect();
    serde_json::Value::Array(items).to_string()
}

/// Watch every server in `list` in the background, or stop watching.
pub fn watch(list: &[Watched], on: bool, managed: bool) {
    #[cfg(target_os = "android")]
    {
        let (url, token) = list.first().map(|w| (w.url.as_str(), w.token.as_str())).unwrap_or(("", ""));
        android::watch(url, token, &watch_json(list), on, managed);
    }
    #[cfg(not(target_os = "android"))]
    let _ = (list, on, managed);
}

/// Whether this device can raise notifications while the app is closed.
pub const fn available() -> bool {
    cfg!(target_os = "android")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(name: &str, url: &str, token: &str) -> Watched {
        Watched { name: name.into(), url: url.into(), token: token.into() }
    }

    #[test]
    fn every_workspace_is_in_the_watch_list_with_its_name() {
        let list = watch_list(w("desktop", "http://a:1/", "ta"), [w("workstation", "http://b:2", "tb")]);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].url, "http://a:1");
        assert_eq!(list[1].name, "workstation");
        let json: serde_json::Value = serde_json::from_str(&watch_json(&list)).unwrap();
        assert_eq!(json[1]["name"], "workstation");
        assert_eq!(json[1]["token"], "tb");
    }

    #[test]
    fn the_watch_list_follows_added_and_removed_workspaces() {
        let first = || w("desktop", "http://a:1", "ta");
        let two = watch_list(first(), [w("b", "http://b:2", "tb")]);
        assert_eq!(two.len(), 2);
        let one = watch_list(first(), []);
        assert_eq!(one.len(), 1);
        assert_eq!(watch_json(&one).matches("http").count(), 1);
    }

    #[test]
    fn incomplete_and_repeated_servers_are_left_out() {
        let list = watch_list(
            w("a", "http://a:1", "ta"),
            [w("empty", "", "t"), w("notoken", "http://c:3", " "), w("again", "http://a:1/", "ta")],
        );
        assert_eq!(list.len(), 1);
    }
}
