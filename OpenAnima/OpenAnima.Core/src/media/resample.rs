//! High-performance pixel resampler for premultiplied BGRA frames.
//! Supports both Nearest Neighbor (pixel-perfect retro art) and Bilinear (smooth illustrations).

use std::cell::RefCell;

#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
struct HorizontalWeight {
    x0_byte: u32,
    x1_byte: u32,
    fx: f32,
    ifx: f32,
}

thread_local! {
    static HORIZONTAL_WEIGHTS: RefCell<Vec<HorizontalWeight>> = const { RefCell::new(Vec::new()) };
    static NEAREST_OFFSETS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// Resamples a premultiplied BGRA image directly into a destination buffer using Nearest Neighbor.
pub fn resample_nearest_into(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return;
    }
    let src_w = src_w as usize;
    let src_h = src_h as usize;
    let dst_w_u = dst_w as usize;
    let dst_h_u = dst_h as usize;

    if src.len() < src_w * src_h * 4 || dst.len() < dst_w_u * dst_h_u * 4 {
        return;
    }

    NEAREST_OFFSETS.with(|cell| {
        let mut x_offsets = cell.borrow_mut();
        x_offsets.clear();
        x_offsets.reserve(dst_w_u);
        for dx in 0..dst_w_u {
            x_offsets.push((dx * src_w) / dst_w_u);
        }

        let src_u32: &[u32] = unsafe {
            std::slice::from_raw_parts(src.as_ptr() as *const u32, src_w * src_h)
        };
        let dst_u32: &mut [u32] = unsafe {
            std::slice::from_raw_parts_mut(dst.as_mut_ptr() as *mut u32, dst_w_u * dst_h_u)
        };

        for dy in 0..dst_h_u {
            let sy = (dy * src_h) / dst_h_u;
            let src_row = sy * src_w;
            let dst_row = dy * dst_w_u;

            for (dx, &sx) in x_offsets.iter().enumerate() {
                dst_u32[dst_row + dx] = src_u32[src_row + sx];
            }
        }
    });
}

/// Resamples a premultiplied BGRA image directly into a destination buffer using Bilinear interpolation.
/// Clamps color channels to the interpolated alpha channel to strictly preserve the Premultiplied Alpha invariant.
pub fn resample_bilinear_into(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    if dst_w == 0 || dst_h == 0 || src_w == 0 || src_h == 0 {
        return;
    }

    let scale_x = src_w as f32 / dst_w as f32;
    let scale_y = src_h as f32 / dst_h as f32;
    let max_x = (src_w.saturating_sub(1)) as usize;
    let max_y = (src_h.saturating_sub(1)) as usize;
    let src_w_usize = src_w as usize;
    let dst_w_usize = dst_w as usize;
    let dst_h_usize = dst_h as usize;

    if src.len() < (max_y * src_w_usize + max_x + 1) * 4 || dst.len() < dst_w_usize * dst_h_usize * 4 {
        return;
    }

    HORIZONTAL_WEIGHTS.with(|cell| {
        let mut x_weights = cell.borrow_mut();
        x_weights.clear();
        x_weights.reserve(dst_w_usize);

        // 1. Precompute 1D horizontal interpolation weights once for the entire frame O(W)
        for dx in 0..dst_w_usize {
            let src_x = (dx as f32 + 0.5) * scale_x - 0.5;
            let x0 = (src_x.floor() as isize).clamp(0, max_x as isize) as usize;
            let x1 = (x0 + 1).min(max_x);
            let fx = (src_x - x0 as f32).clamp(0.0, 1.0);
            let ifx = 1.0 - fx;
            x_weights.push(HorizontalWeight {
                x0_byte: (x0 * 4) as u32,
                x1_byte: (x1 * 4) as u32,
                fx,
                ifx,
            });
        }

        // 2. Fast scanline loop O(H) with precomputed byte offsets and no inner-loop allocations
        for dy in 0..dst_h_usize {
            let src_y = (dy as f32 + 0.5) * scale_y - 0.5;
            let y0 = (src_y.floor() as isize).clamp(0, max_y as isize) as usize;
            let y1 = (y0 + 1).min(max_y);
            let fy = (src_y - y0 as f32).clamp(0.0, 1.0);
            let ify = 1.0 - fy;

            let row0 = y0 * src_w_usize * 4;
            let row1 = y1 * src_w_usize * 4;
            let dst_row = dy * dst_w_usize * 4;

            for (dx, hw) in x_weights.iter().enumerate() {
                let w00 = hw.ifx * ify;
                let w10 = hw.fx * ify;
                let w01 = hw.ifx * fy;
                let w11 = hw.fx * fy;

                let x0_b = hw.x0_byte as usize;
                let x1_b = hw.x1_byte as usize;

                let i00 = row0 + x0_b;
                let i10 = row0 + x1_b;
                let i01 = row1 + x0_b;
                let i11 = row1 + x1_b;

                let d_idx = dst_row + dx * 4;

                let val_a = (src[i00 + 3] as f32) * w00
                    + (src[i10 + 3] as f32) * w10
                    + (src[i01 + 3] as f32) * w01
                    + (src[i11 + 3] as f32) * w11;
                let a = val_a.round().clamp(0.0, 255.0) as u8;

                let val_b = (src[i00] as f32) * w00
                    + (src[i10] as f32) * w10
                    + (src[i01] as f32) * w01
                    + (src[i11] as f32) * w11;
                let b = (val_b.round().clamp(0.0, 255.0) as u8).min(a);

                let val_g = (src[i00 + 1] as f32) * w00
                    + (src[i10 + 1] as f32) * w10
                    + (src[i01 + 1] as f32) * w01
                    + (src[i11 + 1] as f32) * w11;
                let g = (val_g.round().clamp(0.0, 255.0) as u8).min(a);

                let val_r = (src[i00 + 2] as f32) * w00
                    + (src[i10 + 2] as f32) * w10
                    + (src[i01 + 2] as f32) * w01
                    + (src[i11 + 2] as f32) * w11;
                let r = (val_r.round().clamp(0.0, 255.0) as u8).min(a);

                dst[d_idx] = b;
                dst[d_idx + 1] = g;
                dst[d_idx + 2] = r;
                dst[d_idx + 3] = a;
            }
        }
    });
}

/// Allocating resamplers (convenience wrappers)
#[allow(dead_code)]
pub fn resample_nearest(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    resample_nearest_into(src, src_w, src_h, &mut dst, dst_w, dst_h);
    dst
}

#[allow(dead_code)]
pub fn resample_bilinear(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    resample_bilinear_into(src, src_w, src_h, &mut dst, dst_w, dst_h);
    dst
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bilinear_resample_premultiplied_alpha_invariant() {
        let src = vec![
            0, 0, 0, 0,       0, 0, 128, 128,
            100, 0, 0, 100,   0, 255, 0, 255,
        ];
        let mut dst = vec![0u8; 4 * 4 * 4];
        resample_bilinear_into(&src, 2, 2, &mut dst, 4, 4);

        for y in 0..4 {
            for x in 0..4 {
                let idx = (y * 4 + x) * 4;
                let b = dst[idx];
                let g = dst[idx + 1];
                let r = dst[idx + 2];
                let a = dst[idx + 3];
                assert!(b <= a, "B ({}) > A ({}) at ({}, {})", b, a, x, y);
                assert!(g <= a, "G ({}) > A ({}) at ({}, {})", g, a, x, y);
                assert!(r <= a, "R ({}) > A ({}) at ({}, {})", r, a, x, y);
            }
        }
    }

    #[test]
    fn test_nearest_resample_basic() {
        let src = vec![
            10, 20, 30, 40,   50, 60, 70, 80,
            90, 80, 70, 60,   50, 40, 30, 20,
        ];
        let mut dst = vec![0u8; 4 * 4 * 4];
        resample_nearest_into(&src, 2, 2, &mut dst, 4, 4);
        assert_eq!(&dst[0..4], &[10, 20, 30, 40]);
    }

    #[test]
    fn test_bilinear_empty_dimensions_no_panic() {
        let src = vec![0u8; 16];
        let mut dst = vec![0u8; 16];
        resample_bilinear_into(&src, 0, 0, &mut dst, 2, 2);
        resample_bilinear_into(&src, 2, 2, &mut dst, 0, 0);
        resample_nearest_into(&src, 0, 0, &mut dst, 2, 2);
        resample_nearest_into(&src, 2, 2, &mut dst, 0, 0);
    }
}
