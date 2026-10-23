//! Reading a pairing code with the camera. Only a phone has one worth pointing at a screen, so
//! everywhere else there is nothing to scan with.

#[cfg(target_os = "android")]
mod camera;

/// What a scan is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scan {
    /// The camera is asking for permission or opening.
    Starting,
    /// Looking for a code.
    Looking,
    /// Found one. The text is what it holds.
    Found(String),
    /// Could not scan, and why.
    Failed(String),
}

/// A running scan. Dropping it stops the camera.
pub struct Scanner {
    #[cfg(target_os = "android")]
    inner: camera::Camera,
}

/// Whether this device can scan at all.
pub const fn available() -> bool {
    cfg!(target_os = "android")
}

impl Scanner {
    /// Start looking for a code, asking for the camera first when it is needed.
    pub fn start(wake: impl Fn() + Send + Sync + 'static) -> Scanner {
        #[cfg(target_os = "android")]
        {
            Scanner { inner: camera::Camera::start(std::sync::Arc::new(wake)) }
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = wake;
            Scanner {}
        }
    }

    pub fn state(&self) -> Scan {
        #[cfg(target_os = "android")]
        {
            self.inner.state()
        }
        #[cfg(not(target_os = "android"))]
        {
            Scan::Failed("this device has no camera to scan with".into())
        }
    }

    /// The latest picture from the camera, to show while looking: grey pixels, width and height.
    pub fn preview(&self) -> Option<(Vec<u8>, usize, usize)> {
        #[cfg(target_os = "android")]
        {
            self.inner.preview()
        }
        #[cfg(not(target_os = "android"))]
        {
            None
        }
    }
}

/// A camera picture turned a quarter clockwise and halved, for showing on a portrait screen: a
/// phone's sensor is mounted on its side. Returns the grey pixels, width and height.
pub fn upright(grey: &[u8], width: usize, height: usize) -> (Vec<u8>, usize, usize) {
    let (nw, nh) = (height / 2, width / 2);
    let mut out = Vec::with_capacity(nw * nh);
    for y in 0..nh {
        for x in 0..nw {
            out.push(grey[(height - 1 - x * 2) * width + y * 2]);
        }
    }
    (out, nw, nh)
}

/// Finds a QR code in a grey image and returns what it holds.
pub fn decode(grey: &[u8], width: usize, height: usize) -> Option<String> {
    let mut img =
        rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| grey[y * width + x]);
    img.detect_grids().into_iter().find_map(|g| g.decode().ok().map(|(_, text)| text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_read_from_a_grey_picture_of_it() {
        let link = foreman_core::pairing::Pairing {
            url: "https://foreman.harkin.network".into(),
            token: "k3Y/with+odd=chars".into(),
        }
        .link();
        let code = qrcode_rows(&link);
        let (scale, quiet) = (5usize, 4usize);
        let side = (code.len() + 2 * quiet) * scale;
        let grey: Vec<u8> = (0..side * side)
            .map(|i| {
                let (mx, my) = ((i % side) / scale, (i / side) / scale);
                let dark = mx >= quiet
                    && my >= quiet
                    && code
                        .get(my - quiet)
                        .and_then(|r| r.get(mx - quiet))
                        .copied()
                        .unwrap_or(false);
                if dark { 20 } else { 235 }
            })
            .collect();
        assert_eq!(decode(&grey, side, side), Some(link));
    }

    #[test]
    fn a_picture_is_turned_a_quarter_clockwise_and_halved() {
        // 4 wide, 2 tall: the left column becomes the top row, read bottom to top.
        let grey = [1, 2, 3, 4, 5, 6, 7, 8];
        let (out, w, h) = upright(&grey, 4, 2);
        assert_eq!((w, h), (1, 2));
        assert_eq!(out, [5, 7]);
    }

    /// The modules of a code: rows of dark (true) and light (false).
    fn qrcode_rows(data: &str) -> Vec<Vec<bool>> {
        let code = qrcode::QrCode::new(data.as_bytes()).unwrap();
        let n = code.width();
        let dark: Vec<bool> =
            code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
        dark.chunks(n).map(|r| r.to_vec()).collect()
    }
}
