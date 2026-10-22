pub mod app;
pub mod client;

pub use app::App;

#[cfg(not(target_arch = "wasm32"))]
pub fn run_native(url: Option<String>) -> eframe::Result<()> {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([420.0, 480.0])
        .with_title("foreman")
        .with_app_id("foreman");
    let opts = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "foreman",
        opts,
        Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, cc.storage, url)))),
    )
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: android_activity::AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default().with_max_level(log::LevelFilter::Info),
    );
    let opts = eframe::NativeOptions { android_app: Some(app), ..Default::default() };
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
