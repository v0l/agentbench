#[path = "../common/mod.rs"]
mod common;
mod scenes;

use agentbench::App;
use common::Cfg;
use egui_kittest::Harness;
use egui_kittest::kittest::NodeT as _;
use scenes::{Rich, SCENES};
use std::fmt::Write as _;
use std::path::Path;

const PHONES: [[f32; 2]; 4] = [[360.0, 800.0], [393.0, 852.0], [412.0, 915.0], [320.0, 640.0]];

const TAP: f32 = 44.0;
const TAP_SHORT: f32 = 30.0;

#[derive(Clone, Copy, Debug)]
struct Size {
    size: [f32; 2],
    scale: f32,
    insets: [f32; 4],
}

impl Size {
    fn cfg(self) -> Cfg {
        Cfg { size: self.size, scale: self.scale, insets: self.insets, ppp: 1.0 }
    }
}

fn phone(size: [f32; 2], landscape: bool, scale: f32) -> Size {
    if landscape {
        Size { size: [size[1], size[0]], scale, insets: [48.0, 24.0, 0.0, 20.0] }
    } else {
        Size { size, scale, insets: [0.0, 28.0, 0.0, 20.0] }
    }
}

fn parse_size(spec: &str) -> Size {
    let parts: Vec<&str> = spec.split(':').collect();
    let dims: Vec<f32> = parts[0].split('x').map(|v| v.parse().unwrap()).collect();
    let scale = parts.get(1).map_or(1.0, |s| s.parse().unwrap());
    let insets: Vec<f32> = parts
        .get(2)
        .map_or(vec![0.0; 4], |s| s.split(',').map(|v| v.parse().unwrap()).collect());
    Size { size: [dims[0], dims[1]], scale, insets: [insets[0], insets[1], insets[2], insets[3]] }
}

type Text = (egui::Rect, egui::Rect, String, usize);

fn texts(shape: &egui::epaint::Shape, clip: egui::Rect, out: &mut Vec<Text>) {
    match shape {
        egui::epaint::Shape::Vec(v) => v.iter().for_each(|s| texts(s, clip, out)),
        egui::epaint::Shape::Text(t) => {
            let rect = t.galley.rect.translate(t.pos.to_vec2());
            out.push((rect, clip, t.galley.job.text.clone(), t.galley.rows.len()));
        }
        _ => {}
    }
}

fn problems(h: &Harness<'static, App>, s: Size) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let [w, hgt] = s.size;
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(w, hgt));
    if std::env::var("AB_AREAS").is_ok() {
        h.ctx.memory(|m| {
            for l in m.areas().visible_layer_ids() {
                eprintln!("area {:?} {:?}", l, m.area_rect(l.id));
            }
        });
    }
    let mut shapes = Vec::new();
    for c in &h.output().shapes {
        texts(&c.shape, c.clip_rect, &mut shapes);
    }
    for (rect, clip, text, rows) in &shapes {
        if !rect.intersect(*clip).is_positive() {
            continue;
        }
        if rect.left() < -0.5 && clip.left() <= 0.5 {
            out.push(("left-clip", format!("{text:?} at {rect:?}")));
        }
        if *rows > 1 && text.chars().count() <= 24 && !text.contains('\n') && !text.contains(' ') {
            out.push(("key-wrap", format!("{text:?} in {rows} rows at {rect:?}")));
        }
    }
    let live: Vec<egui::Rect> = h
        .ctx
        .interactive_rects_last_pass()
        .into_iter()
        .filter(|r| r.width() < w - 1.0 || r.height() < hgt - 1.0)
        .collect();
    for r in &live {
        if !screen.expand(0.5).contains_rect(*r) {
            out.push(("off-screen", format!("{r:?}")));
        }
    }
    let keys: Vec<egui::Rect> = live
        .into_iter()
        .filter(|r| r.width().min(r.height()) > 12.0)
        .filter(|r| screen.expand(0.5).contains_rect(*r))
        .collect();
    for r in &keys {
        let big = r.width() >= TAP && r.height() >= TAP_SHORT || r.height() >= TAP && r.width() >= TAP_SHORT;
        if !big {
            out.push(("tap-target", format!("{:.0}x{:.0} at {r:?}", r.width(), r.height())));
        }
    }
    for (i, a) in keys.iter().enumerate() {
        for b in &keys[i + 1..] {
            let x = a.intersect(*b);
            if x.width() > 2.0 && x.height() > 2.0 && !a.contains_rect(*b) && !b.contains_rect(*a) {
                out.push(("overlap", format!("{a:?} and {b:?}")));
            }
        }
    }
    out
}

fn run(r: &Rich, s: Size, only: &dyn Fn(u32) -> bool, dir: Option<&Path>) -> String {
    let mut report = String::new();
    for scene in SCENES.iter().filter(|c| only(c.id)) {
        let mut b = (scene.build)(r, s.cfg());
        b.h.run_steps(3);
        if let Some(dir) = dir {
            let img = b.h.render().expect("rendered");
            img.save(dir.join(format!("{:02}-{}.png", scene.id, scene.name))).unwrap();
        }
        for (kind, detail) in problems(&b.h, s) {
            let _ = writeln!(report, "{:02} {} [{kind}] {detail}", scene.id, scene.name);
        }
    }
    report
}

fn only_from_env() -> impl Fn(u32) -> bool {
    let only: Vec<u32> = std::env::var("AB_ONLY")
        .unwrap_or_default()
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    move |id| only.is_empty() || only.contains(&id)
}

#[test]
fn render_the_sheets() {
    let Ok(out) = std::env::var("AB_OUT") else {
        return;
    };
    let specs = std::env::var("AB_SIZES").unwrap_or_else(|_| "393x852=393x852:1:0,28,0,20".into());
    let r = scenes::rich();
    let only = only_from_env();
    for spec in specs.split(';').filter(|s| !s.is_empty()) {
        let (name, rest) = spec.split_once('=').unwrap();
        let dir = Path::new(&out).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let report = run(r, parse_size(rest), &only, Some(&dir));
        std::fs::write(dir.join("problems.txt"), report).unwrap();
    }
}

fn the_scenes_fit(name: &str, size: Size) {
    let report = run(scenes::rich(), size, &only_from_env(), None);
    let bad: Vec<&str> = report.lines().collect();
    assert!(bad.is_empty(), "{name}: {} problems\n{}", bad.len(), bad.join("\n"));
}

macro_rules! fits {
    ($($test:ident: $size:expr, $landscape:expr, $scale:expr;)*) => {$(
        #[test]
        fn $test() {
            the_scenes_fit(stringify!($test), phone($size, $landscape, $scale));
        }
    )*};
}

fits! {
    every_scene_fits_360x800: PHONES[0], false, 1.0;
    every_scene_fits_393x852: PHONES[1], false, 1.0;
    every_scene_fits_412x915: PHONES[2], false, 1.0;
    every_scene_fits_320x640: PHONES[3], false, 1.0;
    every_scene_fits_393x852_landscape: PHONES[1], true, 1.0;
    every_scene_fits_360x800_landscape: PHONES[0], true, 1.0;
    every_scene_fits_393x852_large_text: PHONES[1], false, 1.3;
}
