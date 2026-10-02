//! Window, dock, and tray icon.

use eframe::egui;
use std::sync::OnceLock;

pub const PNG: &[u8] = include_bytes!("../linux/strix-lc-screen.png");

pub fn window_icon() -> egui::IconData {
    let image = decoded();
    let rgba = image.to_rgba8();
    egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}

#[cfg(target_os = "linux")]
pub fn tray_pixmaps() -> Vec<ksni::Icon> {
    static ICONS: OnceLock<Vec<ksni::Icon>> = OnceLock::new();
    ICONS
        .get_or_init(|| {
            let image = decoded();
            [48u32, 32, 22]
                .into_iter()
                .map(|size| {
                    let resized =
                        image.resize_exact(size, size, image::imageops::FilterType::Triangle);
                    let mut data = resized.into_rgba8().into_raw();
                    for pixel in data.as_chunks_mut::<4>().0 {
                        pixel.rotate_right(1);
                    }
                    ksni::Icon {
                        width: size as i32,
                        height: size as i32,
                        data,
                    }
                })
                .collect()
        })
        .clone()
}

fn decoded() -> image::DynamicImage {
    image::load_from_memory(PNG).expect("strix-lc-screen.png")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_png_is_a_square_with_an_opaque_center() {
        let icon = window_icon();
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        let mid = (128 * 256 + 128) * 4;
        assert_eq!(icon.rgba[mid + 3], 255);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tray_pixmap_stores_alpha_first() {
        let icons = tray_pixmaps();
        let icon = icons.iter().find(|icon| icon.width == 32).unwrap();
        let mid = (16 * 32 + 16) * 4;
        assert_eq!(icon.data[mid], 255);
    }
}
