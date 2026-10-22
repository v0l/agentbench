//! The window's typeface: any font installed on the machine, and a size.
//!
//! Nothing is bundled. The text uses the system's own sans-serif and monospace unless
//! the settings name another family, and the size scales every piece of text at once
//! (the sizes written through the app are relative to [`BASE`]).

use egui::{FontData, FontDefinitions, FontFamily};
use egui_bench::theme::{FIGURE_FONT, LEGEND_FONT};
use std::sync::Arc;

/// The body size the rest of the app is drawn for, in points.
pub const BASE: f32 = 13.0;
pub const MIN: f32 = 9.0;
pub const MAX: f32 = 24.0;

/// What the user chose. An empty family means the system default.
#[derive(Clone, Debug, PartialEq)]
pub struct Typeface {
    pub family: String,
    pub mono: String,
    pub size: f32,
}

impl Default for Typeface {
    fn default() -> Self {
        Typeface { family: String::new(), mono: String::new(), size: BASE }
    }
}

impl Typeface {
    pub fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        let get = |k: &str| storage.and_then(|s| s.get_string(k));
        Typeface {
            family: get("font").unwrap_or_default(),
            mono: get("font_mono").unwrap_or_default(),
            size: get("font_size")
                .and_then(|s| s.parse::<f32>().ok())
                .filter(|s| s.is_finite())
                .map_or(BASE, |s| s.clamp(MIN, MAX)),
        }
    }

    pub fn save(&self, storage: &mut dyn eframe::Storage) {
        storage.set_string("font", self.family.clone());
        storage.set_string("font_mono", self.mono.clone());
        storage.set_string("font_size", self.size.to_string());
    }

    fn scale(&self) -> f32 {
        self.size.clamp(MIN, MAX) / BASE
    }
}

/// The fonts installed on this machine, scanned once.
pub struct Installed {
    db: sys::Db,
    /// Every family, sorted.
    pub all: Vec<String>,
    /// The families whose faces are fixed width.
    pub mono: Vec<String>,
}

impl Installed {
    pub fn load() -> Self {
        let db = sys::Db::load();
        let (all, mono) = (db.families(false), db.families(true));
        Installed { db, all, mono }
    }

    /// Nothing installed, as in a browser: the text keeps egui's own fonts.
    pub fn none() -> Self {
        Installed { db: sys::Db::none(), all: Vec::new(), mono: Vec::new() }
    }

    /// Whether there is a list of families to choose from.
    pub fn can_choose(&self) -> bool {
        !self.all.is_empty()
    }
}

/// Bind the chosen fonts to egui's families and scale them to the chosen size.
///
/// The system face goes first and egui's own fonts stay behind it, so a symbol the
/// system font lacks (arrows, emoji) still draws.
pub fn apply(ctx: &egui::Context, face: &Typeface, installed: &Installed) {
    let mut defs = FontDefinitions::default();
    let put = |defs: &mut FontDefinitions, key: &str, data: Option<FontData>| {
        data.map(|d| defs.font_data.insert(key.to_owned(), Arc::new(d))).is_some()
    };
    let regular = put(&mut defs, "system-sans", installed.db.face(&face.family, false, 400));
    let bold = put(&mut defs, "system-sans-bold", installed.db.face(&face.family, false, 600));
    let mono = put(&mut defs, "system-mono", installed.db.face(&face.mono, true, 400));
    let mono_bold = put(&mut defs, "system-mono-bold", installed.db.face(&face.mono, true, 600));

    if regular {
        defs.families.entry(FontFamily::Proportional).or_default().insert(0, "system-sans".into());
    }
    if mono {
        defs.families.entry(FontFamily::Monospace).or_default().insert(0, "system-mono".into());
    }
    let proportional = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let monospace = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
    for (name, own, base) in [
        (LEGEND_FONT, bold.then_some("system-sans-bold"), &proportional),
        (FIGURE_FONT, mono_bold.then_some("system-mono-bold"), &monospace),
    ] {
        let mut stack: Vec<String> = own.map(str::to_owned).into_iter().collect();
        stack.extend(base.iter().cloned());
        defs.families.insert(FontFamily::Name(name.into()), stack);
    }

    let scale = face.scale();
    for data in defs.font_data.values_mut() {
        Arc::make_mut(data).tweak.scale *= scale;
    }
    ctx.set_fonts(defs);
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
mod sys {
    use fontdb::{Database, Family, Query, Weight};

    pub struct Db(Database);

    impl Db {
        pub fn load() -> Self {
            let mut db = Database::new();
            db.load_system_fonts();
            Db(db)
        }

        pub fn none() -> Self {
            Db(Database::new())
        }

        pub fn families(&self, mono: bool) -> Vec<String> {
            let mut names: Vec<String> = self
                .0
                .faces()
                .filter(|f| !mono || f.monospaced)
                .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
                .collect();
            names.sort_by_key(|n| n.to_lowercase());
            names.dedup();
            names
        }

        /// The face of `family` nearest `weight`, or of the system's generic family when
        /// `family` is empty or not installed.
        pub fn face(&self, family: &str, mono: bool, weight: u16) -> Option<egui::FontData> {
            let generic = if mono { Family::Monospace } else { Family::SansSerif };
            let both = [Family::Name(family), generic];
            let families = if family.is_empty() { &both[1..] } else { &both[..] };
            let id =
                self.0.query(&Query { families, weight: Weight(weight), ..Query::default() })?;
            self.0.with_face_data(id, |data, index| {
                let mut font = egui::FontData::from_owned(data.to_vec());
                font.index = index;
                font
            })
        }
    }
}

#[cfg(any(target_arch = "wasm32", target_os = "android"))]
mod sys {
    pub struct Db;

    impl Db {
        pub fn load() -> Self {
            Db
        }

        pub fn none() -> Self {
            Db
        }

        pub fn families(&self, _mono: bool) -> Vec<String> {
            Vec::new()
        }

        pub fn face(&self, _family: &str, _mono: bool, _weight: u16) -> Option<egui::FontData> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(size: f32) -> f32 {
        let ctx = egui::Context::default();
        egui_bench::install(&ctx);
        apply(&ctx, &Typeface { size, ..Typeface::default() }, &Installed::none());
        let mut h = 0.0;
        // The fonts are swapped in at the start of the first pass.
        for _ in 0..2 {
            let _ = ctx.run_ui(Default::default(), |ui| {
                let g = ui.painter().layout_no_wrap(
                    "Hg".into(),
                    egui::FontId::proportional(BASE),
                    egui::Color32::WHITE,
                );
                h = g.size().y;
            });
        }
        h
    }

    #[test]
    fn the_size_scales_the_text() {
        let (small, big) = (height(BASE), height(BASE * 2.0));
        assert!(big > small * 1.6, "{small} -> {big}");
    }

    #[test]
    fn every_family_the_app_names_is_bound() {
        let ctx = egui::Context::default();
        egui_bench::install(&ctx);
        apply(&ctx, &Typeface::default(), &Installed::load());
        for _ in 0..2 {
            let _ = ctx.run_ui(Default::default(), |ui| {
                for f in [
                    egui_bench::theme::legend_font(12.0),
                    egui_bench::theme::figure(12.0),
                    egui_bench::theme::mono(12.0),
                    egui::FontId::proportional(12.0),
                ] {
                    let _ = ui.painter().layout_no_wrap("0".into(), f, egui::Color32::WHITE);
                }
            });
        }
    }

    #[test]
    fn a_stored_size_is_kept_inside_the_range() {
        #[derive(Default)]
        struct Mem(std::collections::HashMap<String, String>);
        impl eframe::Storage for Mem {
            fn get_string(&self, key: &str) -> Option<String> {
                self.0.get(key).cloned()
            }
            fn set_string(&mut self, key: &str, value: String) {
                self.0.insert(key.into(), value);
            }
            fn remove_string(&mut self, key: &str) {
                self.0.remove(key);
            }
            fn flush(&mut self) {}
        }
        use eframe::Storage;
        let mut m = Mem::default();
        Typeface { family: "Inter".into(), mono: "Hack".into(), size: 15.5 }.save(&mut m);
        let back = Typeface::load(Some(&m));
        assert_eq!((back.family.as_str(), back.mono.as_str(), back.size), ("Inter", "Hack", 15.5));
        m.set_string("font_size", "400".into());
        assert_eq!(Typeface::load(Some(&m)).size, MAX);
        m.set_string("font_size", "nope".into());
        assert_eq!(Typeface::load(Some(&m)).size, BASE);
        assert_eq!(Typeface::load(None), Typeface::default());
    }
}
