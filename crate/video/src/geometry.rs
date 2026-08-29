#[cfg(any(feature = "h264", feature = "av1", test))]
use crate::VideoError;

#[cfg(any(feature = "h264", feature = "av1", test))]
pub(crate) const MAX_CODEC_DIMENSION: usize = 4096;
#[cfg(any(feature = "h264", feature = "av1", test))]
pub(crate) const MAX_CODEC_PIXELS: usize = 4096 * 2160;

#[cfg(any(feature = "h264", feature = "av1", test))]
pub(crate) fn checked_codec_pixels(width: usize, height: usize) -> Result<usize, VideoError> {
    (width <= MAX_CODEC_DIMENSION && height <= MAX_CODEC_DIMENSION)
        .then(|| width.checked_mul(height))
        .flatten()
        .filter(|pixels| *pixels > 0 && *pixels <= MAX_CODEC_PIXELS)
        .ok_or(VideoError::InvalidDimensions)
}

/// Fit within non-zero bounds while preserving aspect ratio. Geometry already
/// inside the bounds is preserved exactly; dimensions changed by scaling are
/// rounded down to even values when they are larger than one.
pub(crate) fn fitted_dimensions(
    width: u32,
    height: u32,
    max_width: u32,
    max_height: u32,
) -> (u32, u32) {
    if width <= max_width && height <= max_height {
        return (width, height);
    }

    let (mut width, mut height) =
        if u64::from(max_width) * u64::from(height) <= u64::from(max_height) * u64::from(width) {
            (
                max_width,
                (u64::from(height) * u64::from(max_width) / u64::from(width)) as u32,
            )
        } else {
            (
                (u64::from(width) * u64::from(max_height) / u64::from(height)) as u32,
                max_height,
            )
        };
    width = width.max(1);
    height = height.max(1);
    if width > 1 {
        width &= !1;
    }
    if height > 1 {
        height &= !1;
    }
    (width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitted_dimensions_stay_bounded_and_encoder_compatible() {
        assert_eq!(fitted_dimensions(1920, 1080, 1280, 720), (1280, 720));
        assert_eq!(fitted_dimensions(1920, 1080, 853, 480), (852, 478));
        assert_eq!(fitted_dimensions(4, 2, 2, 2), (2, 1));
    }

    #[test]
    fn codec_pixel_check_rejects_zero_overflow_and_excessive_geometry() {
        assert!(matches!(
            checked_codec_pixels(0, 1),
            Err(VideoError::InvalidDimensions)
        ));
        assert!(matches!(
            checked_codec_pixels(MAX_CODEC_DIMENSION + 1, 1),
            Err(VideoError::InvalidDimensions)
        ));
        assert!(checked_codec_pixels(1920, 1080).is_ok());
    }
}
