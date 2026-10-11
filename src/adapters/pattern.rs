//! Lite-mode perception (build guide section 6.5.1 / 6.5.2): region difference
//! hash and normalised cross-correlation template matching. Pure functions only —
//! no window handles, no OCR model — so they are unit-testable and cheap enough
//! to run on battery.

use image::RgbImage;

/// Pixels per side of the difference-hash grid (9x10 samples -> 8x8 = 64 bits).
pub const DHASH_SIZE: usize = 8;

/// Maximum Hamming distance that still counts as "no change" (section 6.5.1
/// uses >= 8/64 as "moving", so >= 8 means changed).
pub const DHASH_CHANGED_THRESHOLD: u32 = 8;

/// Convert an RGB image to a single-channel luminance buffer (Rec. 601).
pub fn rgb_to_gray(image: &RgbImage) -> Vec<u8> {
    image
        .pixels()
        .map(|px| {
            let (r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32);
            ((r * 299 + g * 587 + b * 114) / 1000) as u8
        })
        .collect()
}

/// Nearest-neighbour downscale of a luminance buffer.
pub fn resize_gray(
    gray: &[u8],
    width: usize,
    height: usize,
    out_w: usize,
    out_h: usize,
) -> Vec<u8> {
    if width == 0 || height == 0 || out_w == 0 || out_h == 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; out_w * out_h];
    for y in 0..out_h {
        let src_y = (y as u64 * height as u64 / out_h as u64).min(height as u64 - 1) as usize;
        for x in 0..out_w {
            let src_x = (x as u64 * width as u64 / out_w as u64).min(width as u64 - 1) as usize;
            out[y * out_w + x] = gray[src_y * width + src_x];
        }
    }
    out
}

/// 64-bit difference hash of an 8x8 luminance grid: each bit compares a pixel
/// with its right neighbour (9 columns sampled).
pub fn dhash_from_grid(grid: &[u8], row_len: usize) -> u64 {
    let mut hash = 0u64;
    if row_len < 2 {
        return 0;
    }
    for y in 0..DHASH_SIZE {
        for x in 0..DHASH_SIZE {
            let idx = y * row_len + x;
            let left = *grid.get(idx).unwrap_or(&0);
            let right = *grid.get(idx + 1).unwrap_or(&0);
            if left > right {
                hash |= 1u64 << (y * DHASH_SIZE + x);
            }
        }
    }
    hash
}

/// Screenshot region -> 64-bit dHash.
pub fn dhash(image: &RgbImage) -> u64 {
    let gray = rgb_to_gray(image);
    let small = resize_gray(
        &gray,
        image.width() as usize,
        image.height() as usize,
        DHASH_SIZE + 1,
        DHASH_SIZE,
    );
    dhash_from_grid(&small, DHASH_SIZE + 1)
}

/// Bits that differ between two hashes.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// True when the region moved: distance >= threshold (default 8/64).
pub fn region_changed(previous: u64, current: u64, threshold: u32) -> bool {
    hamming(previous, current) >= threshold
}

/// Normalised cross-correlation of `template` against `image` at the best
/// position, used by Lite mode to answer "did the credit area change" without
/// recognising digits. Returns -1.0 when the template does not fit.
pub fn ncc(
    image: &[u8],
    width: usize,
    height: usize,
    template: &[u8],
    tw: usize,
    th: usize,
) -> f64 {
    if tw == 0 || th == 0 || tw > width || th > height || image.len() < width * height {
        return -1.0;
    }
    let mut best = f64::NEG_INFINITY;
    for oy in 0..=(height - th) {
        for ox in 0..=(width - tw) {
            let score = ncc_at(image, width, ox, oy, template, tw, th);
            if score > best {
                best = score;
            }
        }
    }
    best
}

/// NCC for one offset; both patches need at least one pixel of variance.
pub fn ncc_at(
    image: &[u8],
    width: usize,
    ox: usize,
    oy: usize,
    template: &[u8],
    tw: usize,
    th: usize,
) -> f64 {
    let (t_mean, t_var) = moments(template);
    let mut patch = Vec::with_capacity(tw * th);
    for y in 0..th {
        for x in 0..tw {
            patch.push(image[(oy + y) * width + ox + x]);
        }
    }
    let (p_mean, p_var) = moments(&patch);
    if t_var <= f64::EPSILON || p_var <= f64::EPSILON {
        // Flat regions carry no information: treat identical means as a match.
        return if (t_mean - p_mean).abs() < 1.0 {
            0.0
        } else {
            -1.0
        };
    }
    let mut sum = 0f64;
    for (tp, pp) in template.iter().zip(patch.iter()) {
        sum += (*tp as f64 - t_mean) * (*pp as f64 - p_mean);
    }
    sum / (t_var.sqrt() * p_var.sqrt())
}

fn moments(values: &[u8]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let mean = values.iter().map(|v| *v as f64).sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|v| (*v as f64 - mean) * (*v as f64 - mean))
        .sum::<f64>();
    (mean, variance)
}

/// Stable textual form for JSON output and golden files.
pub fn hash_hex(hash: u64) -> String {
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            let value = ((x + y) % 256) as u8;
            image::Rgb([value, value, value])
        })
    }

    fn noise(width: u32, height: u32, seed: u32) -> RgbImage {
        let mut state = seed;
        RgbImage::from_fn(width, height, |_, _| {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            let value = ((state >> 16) & 0xff) as u8;
            image::Rgb([value, value, value])
        })
    }

    #[test]
    fn identical_image_has_zero_distance() {
        let image = gradient(64, 32);
        let hash = dhash(&image);
        assert_eq!(hamming(hash, hash), 0);
        assert!(!region_changed(hash, hash, DHASH_CHANGED_THRESHOLD));
        assert_eq!(hash_hex(hash).len(), 16);
    }

    #[test]
    fn different_images_exceed_the_threshold() {
        let a = gradient(64, 32);
        let b = noise(64, 32, 7);
        let distance = hamming(dhash(&a), dhash(&b));
        assert!(
            distance >= DHASH_CHANGED_THRESHOLD,
            "expected >= {DHASH_CHANGED_THRESHOLD}, got {distance}"
        );
        assert!(region_changed(
            dhash(&a),
            dhash(&b),
            DHASH_CHANGED_THRESHOLD
        ));
    }

    #[test]
    fn tiny_shift_counts_as_stable() {
        let mut a = gradient(64, 32);
        let b = a.clone();
        // one pixel changed in a corner of a large image keeps the hash close
        a.put_pixel(0, 0, image::Rgb([255, 255, 255]));
        let distance = hamming(dhash(&a), dhash(&b));
        assert!(distance < DHASH_CHANGED_THRESHOLD, "{distance}");
    }

    #[test]
    fn gray_conversion_uses_luma_weights() {
        let image = RgbImage::from_pixel(1, 1, image::Rgb([255, 0, 0]));
        assert_eq!(rgb_to_gray(&image), vec![76]);
        let green = RgbImage::from_pixel(1, 1, image::Rgb([0, 255, 0]));
        assert_eq!(rgb_to_gray(&green), vec![149]);
    }

    #[test]
    fn resize_is_stable_and_bounded() {
        let gray: Vec<u8> = (0..64).map(|i| i as u8).collect();
        let small = resize_gray(&gray, 8, 8, 4, 4);
        assert_eq!(small.len(), 16);
        assert_eq!(resize_gray(&gray, 8, 8, 0, 4).len(), 0);
        assert_eq!(resize_gray(&[], 0, 0, 4, 4).len(), 0);
    }

    #[test]
    fn ncc_finds_an_embedded_template() {
        let mut image = vec![10u8; 32 * 32];
        for y in 8..16 {
            for x in 8..16 {
                // a checkerboard has variance; a flat block carries no signal
                image[y * 32 + x] = if (x + y) % 2 == 0 { 200 } else { 40 };
            }
        }
        let mut template = Vec::with_capacity(8 * 8);
        for y in 8..16 {
            template.extend_from_slice(&image[y * 32 + 8..y * 32 + 16]);
        }
        let flat = vec![10u8; 8 * 8];
        assert!(ncc(&image, 32, 32, &template, 8, 8) > 0.99);
        assert!(ncc(&image, 32, 32, &flat, 8, 8) <= 0.0);
        assert!((ncc(&image, 32, 32, &template, 40, 40) + 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dhash_from_grid_is_zero_for_small_inputs() {
        assert_eq!(dhash_from_grid(&[1, 2], 1), 0);
        assert_eq!(dhash_from_grid(&[], 8), 0);
    }
}
