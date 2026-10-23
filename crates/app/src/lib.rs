pub mod app;
pub mod attach;
pub mod client;
pub mod fonts;
mod form;
mod notify;
mod picker;
pub mod scan;
mod system;
pub mod voice;

pub use app::{App, LocalServer, status_counts};

#[cfg(not(target_arch = "wasm32"))]
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("foreman-bg")
            .build()
            .expect("tokio runtime")
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn(task: impl std::future::Future<Output = ()> + Send + 'static) {
    runtime().spawn(task);
}

#[cfg(target_arch = "wasm32")]
pub fn spawn(task: impl std::future::Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(task);
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_blocking(work: impl FnOnce() + Send + 'static) {
    runtime().spawn_blocking(work);
}

#[cfg(target_arch = "wasm32")]
pub fn spawn_blocking(work: impl FnOnce() + 'static) {
    work();
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn local_server(home: Option<std::path::PathBuf>) -> LocalServer {
    LocalServer {
        start: Box::new(move || {
            let local = foreman_server::local::Local::open(home.clone())?;
            let me = std::env::current_exe().map_err(|e| e.to_string())?;
            local.ensure(&me, &["serve"])?;
            Ok(client::Server { url: local.url(), token: local.token.clone() })
        }),
    }
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub fn run_native(home: Option<std::path::PathBuf>) -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([420.0, 480.0])
        .with_title("foreman")
        .with_app_id("foreman")
        .with_icon(
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png"))
                .unwrap_or_default(),
        );
    let opts = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "foreman",
        opts,
        Box::new(move |cc| {
            Ok(Box::new(App::with_local(&cc.egui_ctx, cc.storage, None, Some(local_server(home)))))
        }),
    )
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    voice::android::remember_activity(app.activity_as_ptr());
    // eframe has no place to keep state on Android, so it is put in the app's private files.
    let persistence_path = app.internal_data_path().map(|d| d.join("app.ron"));
    let opts =
        eframe::NativeOptions { android_app: Some(app), persistence_path, ..Default::default() };
    let _ = eframe::run_native(
        "foreman",
        opts,
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx, cc.storage, None)))),
    );
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub async fn start() -> Result<(), wasm_bindgen::JsValue> {
    use wasm_bindgen::JsCast;
    let document = web_sys::window().and_then(|w| w.document()).ok_or("no document")?;
    let canvas = document
        .get_element_by_id("foreman")
        .ok_or("no canvas with id foreman")?
        .dyn_into::<web_sys::HtmlCanvasElement>()?;
    let origin = web_sys::window().and_then(|w| w.location().origin().ok());
    eframe::WebRunner::new()
        .start(
            canvas,
            eframe::WebOptions::default(),
            Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, cc.storage, origin)))),
        )
        .await
}
