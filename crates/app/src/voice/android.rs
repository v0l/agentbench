//! The microphone permission, which Android asks for at run time and cpal does not.
//!
//! `RECORD_AUDIO` is listed in the manifest, but a "dangerous" permission is only granted once the
//! user says yes in a dialog the activity opens. Until then opening the microphone gives silence
//! or an error, so the speak key checks first and asks.

use jni::objects::JObject;
use jni::{Env, JavaVM, jni_sig, jni_str};

const PERMISSION: &str = "android.permission.RECORD_AUDIO";
/// `PackageManager.PERMISSION_GRANTED`.
const GRANTED: i32 = 0;

/// The running Activity, kept when the app starts. `ndk_context` hands out the Application,
/// which cannot ask for permissions or say what launched the app.
static ACTIVITY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Remember the Activity for the rest of the process. The pointer is a global reference that
/// `android-activity` keeps alive until the app is dropped, which is after `run_native` returns.
pub(crate) fn remember_activity(raw: *mut std::ffi::c_void) {
    ACTIVITY.store(raw as usize, std::sync::atomic::Ordering::Release);
}

pub(crate) fn with_activity<R>(
    f: impl for<'j> FnOnce(&mut Env<'j>, &JObject<'j>) -> jni::errors::Result<R>,
) -> Result<R, String> {
    let ctx = ndk_context::android_context();
    let raw = ACTIVITY.load(std::sync::atomic::Ordering::Acquire) as jni::sys::jobject;
    if ctx.vm().is_null() || raw.is_null() {
        return Err("no Android activity yet".into());
    }
    let vm = unsafe { JavaVM::from_raw(ctx.vm().cast()) };
    vm.attach_current_thread(|env: &mut Env<'_>| {
        let activity = unsafe { JObject::from_raw(env, raw) };
        f(env, &activity)
    })
    .map_err(|e| e.to_string())
}

/// Whether the user has let this app use the microphone.
pub fn microphone_allowed() -> bool {
    with_activity(|env, activity| {
        let name = env.new_string(PERMISSION)?;
        env.call_method(
            activity,
            jni_str!("checkSelfPermission"),
            jni_sig!("(Ljava/lang/String;)I"),
            &[(&name).into()],
        )?
        .i()
    })
    .is_ok_and(|r| r == GRANTED)
}

/// Open the system dialog that asks for the microphone. The answer arrives later; the next press
/// of the speak key finds out what it was.
pub fn ask_for_microphone() -> Result<(), String> {
    with_activity(|env, activity| {
        let string = env.find_class(jni_str!("java/lang/String"))?;
        let name = env.new_string(PERMISSION)?;
        let list = env.new_object_array(1, &string, &name)?;
        env.call_method(
            activity,
            jni_str!("requestPermissions"),
            jni_sig!("([Ljava/lang/String;I)V"),
            &[(&list).into(), 1.into()],
        )?;
        Ok(())
    })
}
