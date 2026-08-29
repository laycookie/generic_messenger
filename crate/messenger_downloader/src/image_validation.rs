//! Supported image signature and decode validation.

use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};

use crate::DownloadError;

const MAX_IMAGE_DIMENSION: u32 = 4096;
const MAX_IMAGE_DECODE_BYTES: u64 = 96 * 1024 * 1024;

#[must_use]
pub(super) struct UnvalidatedImageBody(Vec<u8>);

impl UnvalidatedImageBody {
    pub(super) fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub(super) async fn validate(self) -> Result<ValidatedImageBody, DownloadError> {
        // Reject obvious non-images before scheduling blocking decoder work.
        if !self.has_supported_signature() {
            return Err(DownloadError::InvalidImageBody);
        }

        smol::unblock(move || -> Result<ValidatedImageBody, DownloadError> {
            if self.decodes_as_supported_image() {
                Ok(ValidatedImageBody(self.0))
            } else {
                Err(DownloadError::InvalidImageBody)
            }
        })
        .await
    }

    fn has_supported_signature(&self) -> bool {
        let bytes = self.0.as_slice();
        let total_len = bytes.len();
        let png = total_len >= 24
            && bytes.starts_with(b"\x89PNG\r\n\x1a\n")
            && bytes.get(12..16) == Some(b"IHDR");
        let jpeg = total_len >= 4 && bytes.starts_with(b"\xff\xd8\xff");
        let gif = total_len >= 13 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"));
        let webp = bytes.starts_with(b"RIFF")
            && bytes.get(8..12) == Some(b"WEBP")
            && bytes
                .get(4..8)
                .and_then(|length| <[u8; 4]>::try_from(length).ok())
                .map(u32::from_le_bytes)
                .and_then(|length| usize::try_from(length).ok())
                .and_then(|length| length.checked_add(8))
                == Some(total_len);
        png || jpeg || gif || webp
    }

    fn decodes_as_supported_image(&self) -> bool {
        let Ok(mut reader) = ImageReader::new(Cursor::new(&self.0)).with_guessed_format() else {
            return false;
        };
        if !matches!(
            reader.format(),
            Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP)
        ) {
            return false;
        }
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
        limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
        limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
        reader.limits(limits);
        reader.decode().is_ok()
    }
}

#[must_use]
pub(super) struct ValidatedImageBody(Vec<u8>);

impl ValidatedImageBody {
    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_validation_decodes_supported_images() {
        for format in [
            ImageFormat::Png,
            ImageFormat::Jpeg,
            ImageFormat::Gif,
            ImageFormat::WebP,
        ] {
            let mut encoded = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(1, 1)
                .write_to(&mut encoded, format)
                .unwrap();
            let encoded = encoded.into_inner();
            let body = UnvalidatedImageBody::new(encoded.clone());
            assert!(body.has_supported_signature());
            assert!(
                body.decodes_as_supported_image(),
                "failed to decode {format:?}"
            );

            let validated = futures::executor::block_on(body.validate()).unwrap();
            assert_eq!(validated.into_bytes(), encoded);
        }
    }

    #[test]
    fn cache_validation_rejects_html_and_truncated_signatures() {
        let html = UnvalidatedImageBody::new(b"<html>not an image</html>".to_vec());
        assert!(!html.has_supported_signature());
        assert!(matches!(
            futures::executor::block_on(html.validate()),
            Err(DownloadError::InvalidImageBody)
        ));

        for truncated in [
            b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR12345678".as_slice(),
            b"\xff\xd8\xff\xd9".as_slice(),
            b"GIF89a1234567".as_slice(),
            b"RIFF\x04\0\0\0WEBP".as_slice(),
        ] {
            let body = UnvalidatedImageBody::new(truncated.to_vec());
            assert!(body.has_supported_signature());
            assert!(!body.decodes_as_supported_image());
            assert!(matches!(
                futures::executor::block_on(body.validate()),
                Err(DownloadError::InvalidImageBody)
            ));
        }
    }
}
