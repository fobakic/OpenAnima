//! High-performance pixel resampler for premultiplied BGRA frames.
//! Supports both Nearest Neighbor (pixel-perfect retro art) and Bilinear (smooth illustrations).

/// Resamples a premultiplied BGRA image directly into a destination buffer using Nearest Neighbor.
pub fn resample_nearest_into(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    let src_w = src_w as usize;
    let src_h = src_h as usize;
    let dst_w_u = dst_w as usize;
    let dst_h_u = dst_h as usize;

    for dy in 0..dst_h_u {
        let sy = (dy * src_h) / dst_h_u;
        let src_row = sy * src_w * 4;
        let dst_row = dy * dst_w_u * 4;

        for dx in 0..dst_w_u {
            let sx = (dx * src_w) / dst_w_u;
            let s_idx = src_row + sx * 4;
            let d_idx = dst_row + dx * 4;

            if s_idx + 4 <= src.len() && d_idx + 4 <= dst.len() {
                dst[d_idx..d_idx + 4].copy_from_slice(&src[s_idx..s_idx + 4]);
            }
        }
    }
}

/// Resamples a premultiplied BGRA image directly into a destination buffer using Bilinear interpolation.
pub fn resample_bilinear_into(src: &[u8], src_w: u32, src_h: u32, dst: &mut [u8], dst_w: u32, dst_h: u32) {
    let scale_x = src_w as f32 / dst_w as f32;
    let scale_y = src_h as f32 / dst_h as f32;
    let max_x = (src_w.saturating_sub(1)) as usize;
    let max_y = (src_h.saturating_sub(1)) as usize;
    let src_w_usize = src_w as usize;

    for dy in 0..dst_h as usize {
        let src_y = (dy as f32 + 0.5) * scale_y - 0.5;
        let y0 = (src_y.floor() as isize).clamp(0, max_y as isize) as usize;
        let y1 = (y0 + 1).min(max_y);
        let fy = (src_y - y0 as f32).clamp(0.0, 1.0);
        let ify = 1.0 - fy;

        let row0 = y0 * src_w_usize * 4;
        let row1 = y1 * src_w_usize * 4;
        let dst_row = dy * (dst_w as usize) * 4;

        for dx in 0..dst_w as usize {
            let src_x = (dx as f32 + 0.5) * scale_x - 0.5;
            let x0 = (src_x.floor() as isize).clamp(0, max_x as isize) as usize;
            let x1 = (x0 + 1).min(max_x);
            let fx = (src_x - x0 as f32).clamp(0.0, 1.0);
            let ifx = 1.0 - fx;

            let w00 = ifx * ify;
            let w10 = fx * ify;
            let w01 = ifx * fy;
            let w11 = fx * fy;

            let i00 = row0 + x0 * 4;
            let i10 = row0 + x1 * 4;
            let i01 = row1 + x0 * 4;
            let i11 = row1 + x1 * 4;

            let d_idx = dst_row + dx * 4;

            if i11 + 4 <= src.len() && d_idx + 4 <= dst.len() {
                for c in 0..4 {
                    let val = (src[i00 + c] as f32) * w00
                        + (src[i10 + c] as f32) * w10
                        + (src[i01 + c] as f32) * w01
                        + (src[i11 + c] as f32) * w11;
                    dst[d_idx + c] = val.round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

/// Allocating resamplers (convenience wrappers)
pub fn resample_nearest(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    resample_nearest_into(src, src_w, src_h, &mut dst, dst_w, dst_h);
    dst
}

pub fn resample_bilinear(src: &[u8], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Vec<u8> {
    if src_w == dst_w && src_h == dst_h {
        return src.to_vec();
    }
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    resample_bilinear_into(src, src_w, src_h, &mut dst, dst_w, dst_h);
    dst
}
