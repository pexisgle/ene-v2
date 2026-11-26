//! Formats the workspace read tool can submit as image attachments.

use image::ImageFormat;

/// Checks whether the header identifies a PNG, JPEG, GIF, or WebP attachment.
pub fn is_attachment_image(bytes: &[u8]) -> bool {
    matches!(
        image::guess_format(bytes),
        Ok(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_four_supported_images_are_treated_as_images() {
        // The old check was four magic-byte tests; `guess_format` must accept
        // exactly the same inputs and still reject the formats it recognises
        // but the image pipeline never received.
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        let jpeg = b"\xff\xd8\xff\xe0\0\x10JFIF";
        let gif87 = b"GIF87a\0\0";
        let gif89 = b"GIF89a\0\0";
        let mut webp = Vec::from(*b"RIFF\x1a\0\0\0WEBPVP8 ");
        webp.push(0);
        for (name, bytes) in [
            ("png", png.as_slice()),
            ("jpeg", jpeg.as_slice()),
            ("gif87", gif87.as_slice()),
            ("gif89", gif89.as_slice()),
            ("webp", webp.as_slice()),
        ] {
            assert!(is_attachment_image(bytes), "{name} must still be an image");
        }

        // Formats `guess_format` knows but this tool must not forward.
        let bmp = b"BM\x1a\0\0\0\0\0\0\0";
        let tiff = b"II*\0\x08\0\0\0";
        let ico = b"\0\0\x01\0\x01\0";
        for (name, bytes) in [
            ("bmp", bmp.as_slice()),
            ("tiff", tiff.as_slice()),
            ("ico", ico.as_slice()),
            ("plain text", b"hello".as_slice()),
            ("empty", b"".as_slice()),
        ] {
            assert!(
                !is_attachment_image(bytes),
                "{name} must not be treated as an image"
            );
        }
    }
}
