//! A QR code for the pairing link, drawn with the painter so there is no image to upload.

use egui::{Color32, Rect, Sense, Ui, Vec2};
use qrcode::{Color, EcLevel, QrCode};

/// The code for `data` as rows of dark (true) and light modules, without the quiet zone.
pub fn matrix(data: &str) -> Option<Vec<Vec<bool>>> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M).ok()?;
    let n = code.width();
    Some(code.to_colors().chunks(n).map(|r| r.iter().map(|c| *c == Color::Dark).collect()).collect())
}

/// Draws the code at `size` points square on a white card, which scanners need: a code on a dark
/// background reads inverted and many phones will not scan it.
pub fn show(ui: &mut Ui, data: &str, size: f32) {
    let Some(rows) = matrix(data) else {
        ui.label("that is too long for a code");
        return;
    };
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let quiet = 2.0;
    let unit = size / (rows.len() as f32 + 2.0 * quiet);
    let p = ui.painter();
    p.rect_filled(rect, 4.0, Color32::WHITE);
    for (y, row) in rows.iter().enumerate() {
        for (x, dark) in row.iter().enumerate() {
            if *dark {
                let at = rect.min + Vec2::new((x as f32 + quiet) * unit, (y as f32 + quiet) * unit);
                p.rect_filled(Rect::from_min_size(at, Vec2::splat(unit)).expand(0.25), 0.0, Color32::BLACK);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foreman_core::pairing::Pairing;

    /// Renders the modules the way they are drawn, then reads them back with a decoder.
    #[test]
    fn the_code_a_phone_scans_holds_the_link() {
        let link = Pairing {
            url: "https://foreman.harkin.network".into(),
            token: "k3Y/with+odd=chars".into(),
        }
        .link();
        let rows = matrix(&link).unwrap();
        let (scale, quiet) = (6usize, 4usize);
        let side = (rows.len() + 2 * quiet) * scale;
        let img = image::GrayImage::from_fn(side as u32, side as u32, |x, y| {
            let (mx, my) = (x as usize / scale, y as usize / scale);
            let dark = mx >= quiet
                && my >= quiet
                && rows.get(my - quiet).and_then(|r| r.get(mx - quiet)).copied().unwrap_or(false);
            image::Luma([if dark { 0 } else { 255 }])
        });
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert_eq!(grids.len(), 1);
        let (_, text) = grids[0].decode().unwrap();
        assert_eq!(text, link);
        assert_eq!(Pairing::parse(&text).unwrap().url, "https://foreman.harkin.network");
    }
}
