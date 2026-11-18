use std::sync::OnceLock;

use eframe::egui;

const ICON_PNG_SIDE: u32 = 256;

fn icon_image() -> &'static image::RgbaImage {
    static IMAGE: OnceLock<image::RgbaImage> = OnceLock::new();
    IMAGE.get_or_init(|| {
        let source = image::load_from_memory(include_bytes!("../assets/icon.jpg"))
            .expect("内置图标无法读取");
        source
            .resize_exact(
                ICON_PNG_SIDE,
                ICON_PNG_SIDE,
                image::imageops::FilterType::Lanczos3,
            )
            .into_rgba8()
    })
}

pub fn icon_data() -> egui::IconData {
    let image = icon_image();
    egui::IconData {
        rgba: image.as_raw().clone(),
        width: image.width(),
        height: image.height(),
    }
}

pub fn color_image() -> egui::ColorImage {
    let image = icon_image();
    egui::ColorImage::from_rgba_unmultiplied(
        [image.width() as usize, image.height() as usize],
        image.as_raw(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_square() {
        let icon = icon_data();
        assert_eq!(icon.width, 256);
        assert_eq!(icon.height, 256);
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
    }
}
