//! Perceptual hashing used by the trigger engine.
//!
//! The Python engine ran a full ORB `detectAndCompute` on every live frame
//! of every region on every tick. That is the single most expensive part of
//! the trigger loop. Perceptual hashes give a comparable "does the screen
//! still look like the reference" answer in microseconds, with no feature
//! extraction at all.

/// Average hash: 8x8 grayscale, thresholded against the mean.
pub fn ahash(gray: &[u8], width: u32, height: u32) -> u64 {
    let small = area_resize_gray(gray, width, height, 8, 8);
    let mean: u32 = small.iter().map(|v| *v as u32).sum::<u32>() / 64;
    let mut hash: u64 = 0;
    for (i, value) in small.iter().enumerate() {
        if *value as u32 > mean {
            hash |= 1u64 << i;
        }
    }
    hash
}

/// Difference hash: compares neighbouring pixels horizontally.
pub fn dhash(gray: &[u8], width: u32, height: u32) -> u64 {
    let small = area_resize_gray(gray, width, height, 9, 8);
    let mut hash: u64 = 0;
    let mut bit = 0usize;
    for y in 0..8usize {
        for x in 0..8usize {
            let left = small[y * 9 + x] as i32;
            let right = small[y * 9 + x + 1] as i32;
            if left > right {
                hash |= 1u64 << bit;
            }
            bit += 1;
        }
    }
    hash
}

/// Combined aHash + dHash similarity in the range 0.0..=1.0.
pub fn similarity(a: (u64, u64), b: (u64, u64)) -> f32 {
    let a_bits = (a.0 ^ b.0).count_ones() as f32;
    let d_bits = (a.1 ^ b.1).count_ones() as f32;
    let a_score = 1.0 - a_bits / 64.0;
    let d_score = 1.0 - d_bits / 64.0;
    (a_score + d_score) * 0.5
}

/// Area-average downscale of a grayscale image.
pub fn area_resize_gray(
    src: &[u8],
    width: u32,
    height: u32,
    out_w: usize,
    out_h: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; out_w * out_h];
    if width == 0 || height == 0 {
        return out;
    }
    for oy in 0..out_h {
        let y0 = (oy * height as usize) / out_h;
        let y1 = (((oy + 1) * height as usize) / out_h).max(y0 + 1).min(height as usize);
        for ox in 0..out_w {
            let x0 = (ox * width as usize) / out_w;
            let x1 = (((ox + 1) * width as usize) / out_w).max(x0 + 1).min(width as usize);
            let mut sum: u32 = 0;
            let mut count: u32 = 0;
            for y in y0..y1 {
                let row = y * width as usize;
                for x in x0..x1 {
                    let idx = row + x;
                    if idx < src.len() {
                        sum += src[idx] as u32;
                        count += 1;
                    }
                }
            }
            out[oy * out_w + ox] = if count > 0 { (sum / count) as u8 } else { 0 };
        }
    }
    out
}

/// Converts straight RGB into grayscale using the BT.601 luma weights.
#[allow(dead_code)]
pub fn rgb_to_gray(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    let pixels = (width as usize) * (height as usize);
    let mut out = vec![0u8; pixels];
    for i in 0..pixels {
        let s = i * 3;
        if s + 3 > rgb.len() {
            break;
        }
        let luma = (rgb[s] as u32 * 77 + rgb[s + 1] as u32 * 150 + rgb[s + 2] as u32 * 29) >> 8;
        out[i] = luma.min(255) as u8;
    }
    out
}

/// Hash pair of a grayscale buffer.
pub fn hash_pair(gray: &[u8], width: u32, height: u32) -> (u64, u64) {
    (ahash(gray, width, height), dhash(gray, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_images_hash_the_same() {
        let img: Vec<u8> = (0..64 * 64).map(|i| (i % 256) as u8).collect();
        let a = hash_pair(&img, 64, 64);
        let b = hash_pair(&img, 64, 64);
        assert_eq!(a, b);
        assert_eq!(similarity(a, b), 1.0);
    }

    #[test]
    fn brightness_shift_keeps_hash_stable() {
        let img: Vec<u8> = (0..64 * 64).map(|i| (i % 256) as u8).collect();
        let bright: Vec<u8> = img.iter().map(|v| v.saturating_add(10)).collect();
        let a = hash_pair(&img, 64, 64);
        let b = hash_pair(&bright, 64, 64);
        assert!(similarity(a, b) > 0.9, "similarity was {}", similarity(a, b));
    }

    #[test]
    fn resize_shape_is_correct() {
        let img = vec![7u8; 32 * 32];
        let out = area_resize_gray(&img, 32, 32, 9, 8);
        assert_eq!(out.len(), 72);
        assert!(out.iter().all(|v| *v == 7));
    }
}
