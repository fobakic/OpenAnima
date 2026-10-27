//! High-performance real-time Chroma Key (solid background removal).
//!
//! Supports:
//!   * Any custom RGB key color (e.g. White, Green, Black, Magenta)
//!   * Adjustable tolerance threshold (Euclidean RGB distance)
//!   * "Contiguous / Flood Fill from borders" mode to protect internal white/keyed details
//!     (e.g. white text "FISH", white teeth, specular highlights)
//!   * Smooth edge feathering with proper Win32 Premultiplied BGRA math

use super::decode::{Frame, FrameSet};
use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq)]
pub struct ChromaKeyConfig {
    pub enabled: bool,
    pub color_rgb: [u8; 3], // [R, G, B]
    pub tolerance: f32,     // 0.0 .. 1.0 (e.g. 0.15 = 15%)
    pub contiguous: bool,   // true = only flood-fill from border ("Только снаружи")
    pub feather: f32,       // 0.0 .. 0.1 (smooth edge transition)
}

impl Default for ChromaKeyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            color_rgb: [255, 255, 255], // default white
            tolerance: 0.15,
            contiguous: true,
            feather: 0.04,
        }
    }
}

/// Computes normalized Euclidean color distance in RGB space (0.0 .. 1.0).
#[inline(always)]
fn color_dist(b: u8, g: u8, r: u8, a: u8, tr: u8, tg: u8, tb: u8) -> f32 {
    if a == 0 {
        return 0.0; // Already transparent counts as background match
    }
    // Un-premultiply if semi-transparent
    let (ur, ug, ub) = if a < 255 {
        let inv_a = 255.0 / (a as f32);
        (
            ((r as f32) * inv_a).min(255.0),
            ((g as f32) * inv_a).min(255.0),
            ((b as f32) * inv_a).min(255.0),
        )
    } else {
        (r as f32, g as f32, b as f32)
    };

    let dr = ur - (tr as f32);
    let dg = ug - (tg as f32);
    let db = ub - (tb as f32);
    // sqrt(3 * 255^2) ~= 441.67295
    (dr * dr + dg * dg + db * db).sqrt() / 441.67295
}

/// Applies Chroma Key to all frames in the FrameSet, returning a new FrameSet.
pub fn apply_chroma_key(raw: &FrameSet, cfg: &ChromaKeyConfig) -> FrameSet {
    if !cfg.enabled {
        return raw.clone();
    }

    let frames: Vec<Frame> = raw
        .frames
        .iter()
        .map(|f| apply_to_frame(f, cfg))
        .collect();

    FrameSet {
        frames,
        width: raw.width,
        height: raw.height,
        source: raw.source.clone(),
    }
}

pub fn apply_to_frame(frame: &Frame, cfg: &ChromaKeyConfig) -> Frame {
    let w = frame.width as usize;
    let h = frame.height as usize;
    if w == 0 || h == 0 || frame.bgra.len() < w * h * 4 {
        return frame.clone();
    }

    let total = w * h;
    let mut out_bgra = frame.bgra.clone();

    let tr = cfg.color_rgb[0];
    let tg = cfg.color_rgb[1];
    let tb = cfg.color_rgb[2];
    let tol = cfg.tolerance.clamp(0.001, 1.0);
    let feather = cfg.feather.max(0.001);

    if !cfg.contiguous {
        // Global mode: remove all matching pixels across the whole image
        for i in 0..total {
            let offset = i * 4;
            let a = out_bgra[offset + 3];
            if a == 0 {
                continue;
            }
            let b = out_bgra[offset];
            let g = out_bgra[offset + 1];
            let r = out_bgra[offset + 2];

            let d = color_dist(b, g, r, a, tr, tg, tb);
            if d <= tol {
                out_bgra[offset] = 0;
                out_bgra[offset + 1] = 0;
                out_bgra[offset + 2] = 0;
                out_bgra[offset + 3] = 0;
            } else if d < tol + feather {
                let factor = ((d - tol) / feather).clamp(0.0, 1.0);
                let new_a = ((a as f32) * factor).round() as u8;
                out_bgra[offset] = ((b as f32) * factor).round() as u8;
                out_bgra[offset + 1] = ((g as f32) * factor).round() as u8;
                out_bgra[offset + 2] = ((r as f32) * factor).round() as u8;
                out_bgra[offset + 3] = new_a;
            }
        }
        return Frame {
            width: frame.width,
            height: frame.height,
            delay_ms: frame.delay_ms,
            bgra: out_bgra,
        };
    }

    // Contiguous mode: Flood Fill (BFS) starting strictly from the outer borders.
    // This protects letters, eyes, and details inside the character.
    let mut is_bg = vec![false; total];
    let mut visited = vec![false; total];
    let mut queue = VecDeque::with_capacity(w * 2 + h * 2);

    let check_and_enqueue = |idx: usize,
                                 q: &mut VecDeque<usize>,
                                 vis: &mut [bool],
                                 bg: &mut [bool]| {
        if vis[idx] {
            return;
        }
        vis[idx] = true;
        let offset = idx * 4;
        let a = frame.bgra[offset + 3];
        if a == 0 {
            // Already transparent
            bg[idx] = true;
            q.push_back(idx);
            return;
        }
        let b = frame.bgra[offset];
        let g = frame.bgra[offset + 1];
        let r = frame.bgra[offset + 2];
        let d = color_dist(b, g, r, a, tr, tg, tb);
        if d <= tol {
            bg[idx] = true;
            q.push_back(idx);
        }
    };

    // Border rows (top & bottom)
    for x in 0..w {
        check_and_enqueue(x, &mut queue, &mut visited, &mut is_bg);
        check_and_enqueue((h - 1) * w + x, &mut queue, &mut visited, &mut is_bg);
    }
    // Border columns (left & right)
    for y in 0..h {
        check_and_enqueue(y * w, &mut queue, &mut visited, &mut is_bg);
        check_and_enqueue(y * w + (w - 1), &mut queue, &mut visited, &mut is_bg);
    }

    // 4-way BFS expansion
    while let Some(idx) = queue.pop_front() {
        let x = idx % w;
        let y = idx / w;

        let neighbors = [
            if x > 0 { Some(idx - 1) } else { None },
            if x + 1 < w { Some(idx + 1) } else { None },
            if y > 0 { Some(idx - w) } else { None },
            if y + 1 < h { Some(idx + w) } else { None },
        ];

        for n_opt in neighbors.into_iter().flatten() {
            if !visited[n_opt] {
                visited[n_opt] = true;
                let offset = n_opt * 4;
                let a = frame.bgra[offset + 3];
                if a == 0 {
                    is_bg[n_opt] = true;
                    queue.push_back(n_opt);
                } else {
                    let b = frame.bgra[offset];
                    let g = frame.bgra[offset + 1];
                    let r = frame.bgra[offset + 2];
                    let d = color_dist(b, g, r, a, tr, tg, tb);
                    if d <= tol {
                        is_bg[n_opt] = true;
                        queue.push_back(n_opt);
                    }
                }
            }
        }
    }

    // Apply transparency to connected background, feather edges
    for idx in 0..total {
        let offset = idx * 4;
        if is_bg[idx] {
            out_bgra[offset] = 0;
            out_bgra[offset + 1] = 0;
            out_bgra[offset + 2] = 0;
            out_bgra[offset + 3] = 0;
        } else {
            let x = idx % w;
            let y = idx / w;
            let touches_bg = (x > 0 && is_bg[idx - 1])
                || (x + 1 < w && is_bg[idx + 1])
                || (y > 0 && is_bg[idx - w])
                || (y + 1 < h && is_bg[idx + w]);

            if touches_bg {
                let a = out_bgra[offset + 3];
                if a > 0 {
                    let b = out_bgra[offset];
                    let g = out_bgra[offset + 1];
                    let r = out_bgra[offset + 2];
                    let d = color_dist(b, g, r, a, tr, tg, tb);
                    if d < tol + feather {
                        let factor = ((d - tol) / feather).clamp(0.0, 1.0);
                        let new_a = ((a as f32) * factor).round() as u8;
                        out_bgra[offset] = ((b as f32) * factor).round() as u8;
                        out_bgra[offset + 1] = ((g as f32) * factor).round() as u8;
                        out_bgra[offset + 2] = ((r as f32) * factor).round() as u8;
                        out_bgra[offset + 3] = new_a;
                    }
                }
            }
        }
    }

    Frame {
        width: frame.width,
        height: frame.height,
        delay_ms: frame.delay_ms,
        bgra: out_bgra,
    }
}
