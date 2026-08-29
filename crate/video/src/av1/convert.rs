//! RGB/RGBA to limited-range BT.709 YUV420 conversion kernels.

use std::simd::{
    cmp::SimdOrd,
    i32x4, i32x8,
    num::{SimdInt, SimdUint},
    simd_swizzle, u8x8, u8x16, u8x32,
};

const PIXELS_PER_VECTOR: usize = 8;

#[allow(clippy::too_many_arguments)]
pub(super) fn convert_simd(
    pixels: &[u8],
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    y_plane: &mut [u8],
    y_stride: usize,
    u_plane: &mut [u8],
    u_stride: usize,
    v_plane: &mut [u8],
    v_stride: usize,
) {
    debug_assert!(matches!(bytes_per_pixel, 3 | 4));
    debug_assert!(pixels.len() >= width * height * bytes_per_pixel);
    debug_assert!(y_stride >= width);
    let chroma_width = width.div_ceil(2);
    debug_assert!(u_stride >= chroma_width);
    debug_assert!(v_stride >= chroma_width);

    let vector_width = width / PIXELS_PER_VECTOR * PIXELS_PER_VECTOR;
    let paired_height = height / 2 * 2;
    for source_y in (0..paired_height).step_by(2) {
        let source_row_len = width * bytes_per_pixel;
        let top_offset = source_y * source_row_len;
        let top_source = &pixels[top_offset..top_offset + source_row_len];
        let bottom_source = &pixels[top_offset + source_row_len..top_offset + source_row_len * 2];

        let (top_rows, bottom_rows) = y_plane.split_at_mut((source_y + 1) * y_stride);
        let top_y = &mut top_rows[source_y * y_stride..][..width];
        let bottom_y = &mut bottom_rows[..width];
        let chroma_y = source_y / 2;
        let u_row = &mut u_plane[chroma_y * u_stride..][..chroma_width];
        let v_row = &mut v_plane[chroma_y * v_stride..][..chroma_width];

        for x in (0..vector_width).step_by(PIXELS_PER_VECTOR) {
            let byte_offset = x * bytes_per_pixel;
            let top_rgb = deinterleave_8(&top_source[byte_offset..], bytes_per_pixel);
            let bottom_rgb = deinterleave_8(&bottom_source[byte_offset..], bytes_per_pixel);
            let (top_luma, _, _) = rgb_to_limited_bt709_simd_8(top_rgb.0, top_rgb.1, top_rgb.2);
            let (bottom_luma, _, _) =
                rgb_to_limited_bt709_simd_8(bottom_rgb.0, bottom_rgb.1, bottom_rgb.2);
            top_luma.copy_to_slice(&mut top_y[x..x + PIXELS_PER_VECTOR]);
            bottom_luma.copy_to_slice(&mut bottom_y[x..x + PIXELS_PER_VECTOR]);

            let red = average_2x2(top_rgb.0, bottom_rgb.0);
            let green = average_2x2(top_rgb.1, bottom_rgb.1);
            let blue = average_2x2(top_rgb.2, bottom_rgb.2);
            let (_, chroma_u, chroma_v) = rgb_to_limited_bt709_simd_4(red, green, blue);
            let chroma_x = x / 2;
            chroma_u.copy_to_slice(&mut u_row[chroma_x..chroma_x + 4]);
            chroma_v.copy_to_slice(&mut v_row[chroma_x..chroma_x + 4]);
        }

        for x in vector_width..width {
            let byte_offset = x * bytes_per_pixel;
            top_y[x] = rgb_to_limited_bt709(
                top_source[byte_offset],
                top_source[byte_offset + 1],
                top_source[byte_offset + 2],
            )
            .0;
            bottom_y[x] = rgb_to_limited_bt709(
                bottom_source[byte_offset],
                bottom_source[byte_offset + 1],
                bottom_source[byte_offset + 2],
            )
            .0;
        }
        for chroma_x in vector_width / 2..chroma_width {
            let (_, u, v) =
                scalar_chroma_at(pixels, width, height, bytes_per_pixel, chroma_x, chroma_y);
            u_row[chroma_x] = u;
            v_row[chroma_x] = v;
        }
    }

    if paired_height != height {
        let source_y = height - 1;
        let source_row_len = width * bytes_per_pixel;
        let source_row = &pixels[source_y * source_row_len..][..source_row_len];
        let y_row = &mut y_plane[source_y * y_stride..][..width];
        for (source_pixel, target_y) in source_row
            .chunks_exact(bytes_per_pixel)
            .zip(y_row.iter_mut())
        {
            *target_y = rgb_to_limited_bt709(source_pixel[0], source_pixel[1], source_pixel[2]).0;
        }

        let chroma_y = source_y / 2;
        let u_row = &mut u_plane[chroma_y * u_stride..][..chroma_width];
        let v_row = &mut v_plane[chroma_y * v_stride..][..chroma_width];
        for chroma_x in 0..chroma_width {
            let (_, u, v) =
                scalar_chroma_at(pixels, width, height, bytes_per_pixel, chroma_x, chroma_y);
            u_row[chroma_x] = u;
            v_row[chroma_x] = v;
        }
    }
}

#[cfg(any(test, feature = "bench-internals"))]
#[allow(clippy::too_many_arguments)]
pub(super) fn convert_scalar(
    pixels: &[u8],
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    y_plane: &mut [u8],
    y_stride: usize,
    u_plane: &mut [u8],
    u_stride: usize,
    v_plane: &mut [u8],
    v_stride: usize,
) {
    debug_assert!(matches!(bytes_per_pixel, 3 | 4));
    for (source_row, target_row) in pixels
        .chunks_exact(width * bytes_per_pixel)
        .zip(y_plane.chunks_exact_mut(y_stride))
        .take(height)
    {
        for (source_pixel, target_y) in source_row
            .chunks_exact(bytes_per_pixel)
            .zip(target_row.iter_mut())
        {
            *target_y = rgb_to_limited_bt709(source_pixel[0], source_pixel[1], source_pixel[2]).0;
        }
    }

    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    for chroma_y in 0..chroma_height {
        let u_row = &mut u_plane[chroma_y * u_stride..][..chroma_width];
        let v_row = &mut v_plane[chroma_y * v_stride..][..chroma_width];
        for chroma_x in 0..chroma_width {
            let (_, u, v) =
                scalar_chroma_at(pixels, width, height, bytes_per_pixel, chroma_x, chroma_y);
            u_row[chroma_x] = u;
            v_row[chroma_x] = v;
        }
    }
}

#[inline]
fn deinterleave_8(source: &[u8], bytes_per_pixel: usize) -> (i32x8, i32x8, i32x8) {
    let (red, green, blue) = if bytes_per_pixel == 4 {
        let packed = u8x32::from_slice(source);
        let red: u8x8 = simd_swizzle!(packed, [0, 4, 8, 12, 16, 20, 24, 28]);
        let green: u8x8 = simd_swizzle!(packed, [1, 5, 9, 13, 17, 21, 25, 29]);
        let blue: u8x8 = simd_swizzle!(packed, [2, 6, 10, 14, 18, 22, 26, 30]);
        (red, green, blue)
    } else {
        let first = u8x16::from_slice(source);
        let mut second = [0_u8; 16];
        second[..8].copy_from_slice(&source[16..24]);
        let second = u8x16::from_array(second);
        let red: u8x8 = simd_swizzle!(first, second, [0, 3, 6, 9, 12, 15, 18, 21]);
        let green: u8x8 = simd_swizzle!(first, second, [1, 4, 7, 10, 13, 16, 19, 22]);
        let blue: u8x8 = simd_swizzle!(first, second, [2, 5, 8, 11, 14, 17, 20, 23]);
        (red, green, blue)
    };
    (red.cast(), green.cast(), blue.cast())
}

#[inline]
fn average_2x2(top: i32x8, bottom: i32x8) -> i32x4 {
    let sum = top + bottom;
    let even: i32x4 = simd_swizzle!(sum, [0, 2, 4, 6]);
    let odd: i32x4 = simd_swizzle!(sum, [1, 3, 5, 7]);
    (even + odd + i32x4::splat(2)) >> 2
}

macro_rules! rgb_to_limited_bt709_simd {
    ($name:ident, $input:ty, $output:ty) => {
        #[inline]
        fn $name(red: $input, green: $input, blue: $input) -> ($output, $output, $output) {
            let rounding = <$input>::splat(32_768);
            let y = <$input>::splat(16)
                + ((<$input>::splat(11_966) * red
                    + <$input>::splat(40_254) * green
                    + <$input>::splat(4_064) * blue
                    + rounding)
                    >> 16);
            let u = <$input>::splat(128)
                + ((<$input>::splat(-6_596) * red
                    + <$input>::splat(-22_189) * green
                    + <$input>::splat(28_784) * blue
                    + rounding)
                    >> 16);
            let v = <$input>::splat(128)
                + ((<$input>::splat(28_784) * red
                    + <$input>::splat(-26_145) * green
                    + <$input>::splat(-2_639) * blue
                    + rounding)
                    >> 16);
            let minimum = <$input>::splat(0);
            let maximum = <$input>::splat(255);
            (
                y.simd_clamp(minimum, maximum).cast(),
                u.simd_clamp(minimum, maximum).cast(),
                v.simd_clamp(minimum, maximum).cast(),
            )
        }
    };
}

rgb_to_limited_bt709_simd!(rgb_to_limited_bt709_simd_8, i32x8, u8x8);
rgb_to_limited_bt709_simd!(rgb_to_limited_bt709_simd_4, i32x4, std::simd::u8x4);

#[inline]
fn scalar_chroma_at(
    pixels: &[u8],
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    chroma_x: usize,
    chroma_y: usize,
) -> (u8, u8, u8) {
    let mut red = 0_u32;
    let mut green = 0_u32;
    let mut blue = 0_u32;
    let mut count = 0_u32;
    for y in chroma_y * 2..(chroma_y * 2 + 2).min(height) {
        for x in chroma_x * 2..(chroma_x * 2 + 2).min(width) {
            let offset = (y * width + x) * bytes_per_pixel;
            red += u32::from(pixels[offset]);
            green += u32::from(pixels[offset + 1]);
            blue += u32::from(pixels[offset + 2]);
            count += 1;
        }
    }
    let red = ((red + count / 2) / count) as u8;
    let green = ((green + count / 2) / count) as u8;
    let blue = ((blue + count / 2) / count) as u8;
    rgb_to_limited_bt709(red, green, blue)
}

pub(super) fn rgb_to_limited_bt709(red: u8, green: u8, blue: u8) -> (u8, u8, u8) {
    let red = i32::from(red);
    let green = i32::from(green);
    let blue = i32::from(blue);
    let y = 16 + ((11_966 * red + 40_254 * green + 4_064 * blue + 32_768) >> 16);
    let u = 128 + ((-6_596 * red - 22_189 * green + 28_784 * blue + 32_768) >> 16);
    let v = 128 + ((28_784 * red - 26_145 * green - 2_639 * blue + 32_768) >> 16);
    (
        y.clamp(0, 255) as u8,
        u.clamp(0, 255) as u8,
        v.clamp(0, 255) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next_random(state: &mut u64) -> u8 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (*state >> 56) as u8
    }

    #[test]
    fn simd_matches_scalar_for_formats_strides_edges_and_alignments() {
        let dimensions = [
            (1, 1),
            (2, 2),
            (7, 3),
            (8, 2),
            (9, 5),
            (15, 4),
            (16, 16),
            (17, 7),
            (31, 9),
        ];
        let mut state = 0x0c01_a709_u64;
        for bytes_per_pixel in [3, 4] {
            for (width, height) in dimensions {
                for source_offset in 0..8 {
                    let source_len = width * height * bytes_per_pixel;
                    let mut source_storage = vec![0_u8; source_offset + source_len + 8];
                    for byte in &mut source_storage {
                        *byte = next_random(&mut state);
                    }
                    let source = &source_storage[source_offset..source_offset + source_len];
                    let chroma_width = width.div_ceil(2);
                    let chroma_height = height.div_ceil(2);
                    let y_stride = width + source_offset % 3;
                    let u_stride = chroma_width + (source_offset + 1) % 3;
                    let v_stride = chroma_width + (source_offset + 2) % 3;
                    let plane_offset = (source_offset * 3) % 8;
                    let y_len = y_stride * height;
                    let u_len = u_stride * chroma_height;
                    let v_len = v_stride * chroma_height;
                    let mut scalar_y = vec![0xa5; plane_offset + y_len + 8];
                    let mut scalar_u = vec![0xb6; plane_offset + u_len + 8];
                    let mut scalar_v = vec![0xc7; plane_offset + v_len + 8];
                    let mut simd_y = scalar_y.clone();
                    let mut simd_u = scalar_u.clone();
                    let mut simd_v = scalar_v.clone();

                    convert_scalar(
                        source,
                        width,
                        height,
                        bytes_per_pixel,
                        &mut scalar_y[plane_offset..plane_offset + y_len],
                        y_stride,
                        &mut scalar_u[plane_offset..plane_offset + u_len],
                        u_stride,
                        &mut scalar_v[plane_offset..plane_offset + v_len],
                        v_stride,
                    );
                    convert_simd(
                        source,
                        width,
                        height,
                        bytes_per_pixel,
                        &mut simd_y[plane_offset..plane_offset + y_len],
                        y_stride,
                        &mut simd_u[plane_offset..plane_offset + u_len],
                        u_stride,
                        &mut simd_v[plane_offset..plane_offset + v_len],
                        v_stride,
                    );

                    assert_eq!(simd_y, scalar_y, "Y {width}x{height} bpp={bytes_per_pixel}");
                    assert_eq!(simd_u, scalar_u, "U {width}x{height} bpp={bytes_per_pixel}");
                    assert_eq!(simd_v, scalar_v, "V {width}x{height} bpp={bytes_per_pixel}");
                }
            }
        }
    }

    #[test]
    fn vector_color_transform_matches_scalar_at_boundaries() {
        let colors = [
            [0, 0, 0],
            [255, 255, 255],
            [255, 0, 0],
            [0, 255, 0],
            [0, 0, 255],
            [1, 2, 3],
            [254, 253, 252],
            [16, 128, 235],
        ];
        let red = i32x8::from_array(colors.map(|color| color[0]));
        let green = i32x8::from_array(colors.map(|color| color[1]));
        let blue = i32x8::from_array(colors.map(|color| color[2]));
        let (y, u, v) = rgb_to_limited_bt709_simd_8(red, green, blue);

        for (index, color) in colors.into_iter().enumerate() {
            let expected = rgb_to_limited_bt709(color[0] as u8, color[1] as u8, color[2] as u8);
            assert_eq!((y[index], u[index], v[index]), expected);
        }
    }
}
