use anyhow::{Context, Result};
use image::{DynamicImage, ImageFormat, Luma};
use qrcode::QrCode;

pub fn render_qr_png(content: &str) -> Result<Vec<u8>> {
    let code = QrCode::new(content.as_bytes()).context("invalid QR payload")?;
    let image = code.render::<Luma<u8>>().build();
    let mut bytes = std::io::Cursor::new(Vec::new());
    DynamicImage::ImageLuma8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .context("failed to encode PNG")?;
    Ok(bytes.into_inner())
}
