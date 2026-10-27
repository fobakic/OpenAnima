//! Decodes animated media into a list of premultiplied BGRA frames.
//!
//! Supported inputs: GIF, PNG, APNG, WEBP. The output layout matches what
//! `UpdateLayeredWindow` expects for a 32-bit BI_RGB DIB: premultiplied
//! alpha, blue-first, top-down rows.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, ImageDecoder};

/// One decoded frame, ready to blit.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub delay_ms: u32,
    /// Premultiplied BGRA, top-down, 4 bytes per pixel.
    pub bgra: Vec<u8>,
}

/// A decoded animation.
#[derive(Clone)]
pub struct FrameSet {
    pub frames: Vec<Frame>,
    pub width: u32,
    pub height: u32,
    pub source: String,
}

impl FrameSet {
    /// Total size of the decoded pixel data in bytes.
    pub fn bytes(&self) -> usize {
        self.frames.iter().map(|f| f.bgra.len()).sum()
    }

    /// Resamples all frames to target dimensions using bilinear or nearest neighbor.
    pub fn resample(&self, target_w: u32, target_h: u32, smooth: bool) -> FrameSet {
        if target_w == self.width && target_h == self.height {
            return self.clone();
        }
        let frames: Vec<Frame> = self
            .frames
            .iter()
            .map(|f| {
                let bgra = if smooth {
                    super::resample::resample_bilinear(&f.bgra, self.width, self.height, target_w, target_h)
                } else {
                    super::resample::resample_nearest(&f.bgra, self.width, self.height, target_w, target_h)
                };
                Frame {
                    width: target_w,
                    height: target_h,
                    delay_ms: f.delay_ms,
                    bgra,
                }
            })
            .collect();

        FrameSet {
            frames,
            width: target_w,
            height: target_h,
            source: self.source.clone(),
        }
    }
}

/// Decodes an animation file, dispatching on the file extension.
pub fn decode_file(path: &Path) -> Result<FrameSet, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "gif" => decode_gif(path),
        "png" | "apng" => decode_png(path),
        "webp" => decode_webp(path),
        "" => Err("file has no extension".into()),
        other => Err(format!("unsupported media type '{other}'")),
    }
}

/// Decodes a GIF, honouring the disposal methods so that frame deltas
/// composite correctly (this is what the Python side calls
/// `iter_composited_frames`).
fn decode_gif(path: &Path) -> Result<FrameSet, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    let mut reader = options
        .read_info(BufReader::new(file))
        .map_err(|e| format!("gif header: {e}"))?;

    let width = reader.width() as u32;
    let height = reader.height() as u32;
    if width == 0 || height == 0 {
        return Err("gif has zero size".into());
    }

    let mut canvas = vec![0u8; (width * height * 4) as usize];
    let _previous: Option<Vec<u8>> = None;
    let mut frames: Vec<Frame> = Vec::new();

    while let Some(frame) = reader
        .read_next_frame()
        .map_err(|e| format!("gif frame: {e}"))?
    {
        let fw = frame.width as u32;
        let fh = frame.height as u32;
        let left = frame.left as u32;
        let top = frame.top as u32;

        // Keep the canvas as it was before this frame for `Previous` disposal.
        let saved = if frame.dispose == gif::DisposalMethod::Previous {
            Some(canvas.clone())
        } else {
            None
        };

        // Paste the frame rectangle onto the canvas.
        // In GIF, sub-frames only overwrite pixels where alpha != 0.
        // Transparent pixels (alpha == 0) leave the previous canvas content intact.
        let src: &[u8] = frame.buffer.as_ref();
        let copy_cols = fw.min(width.saturating_sub(left)) as usize;
        for y in 0..fh {
            let dy = top + y;
            if dy >= height {
                break;
            }
            let src_row_start = (y * fw * 4) as usize;
            let dst_row_start = ((dy * width + left) * 4) as usize;
            let src_row_end = src_row_start + copy_cols * 4;
            let dst_row_end = dst_row_start + copy_cols * 4;

            if src_row_end <= src.len() && dst_row_end <= canvas.len() {
                let src_slice = &src[src_row_start..src_row_end];
                let dst_slice = &mut canvas[dst_row_start..dst_row_end];
                for (s, d) in src_slice.chunks_exact(4).zip(dst_slice.chunks_exact_mut(4)) {
                    if s[3] != 0 {
                        d.copy_from_slice(s);
                    }
                }
            }
        }

        // The GIF delay is expressed in units of 10 ms.
        let delay_ms = (frame.delay as u32).saturating_mul(10);

        frames.push(Frame {
            width,
            height,
            delay_ms,
            bgra: rgba_to_premul_bgra(&canvas),
        });

        // Apply the disposal method for the next frame.
        match frame.dispose {
            gif::DisposalMethod::Background => {
                clear_rect(&mut canvas, width, height, left, top, fw, fh);
            }
            gif::DisposalMethod::Previous => {
                if let Some(saved) = saved {
                    canvas = saved;
                }
            }
            _ => {}
        }
    }

    if frames.is_empty() {
        return Err("gif contains no frames".into());
    }

    Ok(FrameSet {
        width,
        height,
        frames,
        source: path.display().to_string(),
    })
}

/// Decodes a PNG or APNG from a reader.
pub fn decode_png_from_reader<R: std::io::Read>(r: R, source: String) -> Result<FrameSet, String> {
    let mut decoder = png::Decoder::new(r);
    decoder.set_transformations(
        png::Transformations::EXPAND
            | png::Transformations::STRIP_16
            | png::Transformations::ALPHA,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("png header: {e}"))?;

    let (width, height) = reader.info().size();
    if width == 0 || height == 0 {
        return Err("png has zero size".into());
    }

    let mut canvas = vec![0u8; (width * height * 4) as usize];
    let _previous: Option<Vec<u8>> = None;
    let mut frames: Vec<Frame> = Vec::new();
    let mut seen_any = false;

    loop {
        let mut buffer = vec![0u8; reader.output_buffer_size()];
        let out = match reader.next_frame(&mut buffer) {
            Ok(info) => info,
            Err(_) => break,
        };
        let control = reader.info().frame_control.clone();

        match control {
            // The default image. In an animated PNG this is either the first
            // animation frame (when an fcTL precedes the IDAT chunk) or a
            // separate fallback image that must be skipped.
            None => {
                if seen_any {
                    break;
                }
                let rgba = png_to_rgba(&buffer, out.width, out.height, out.color_type);
                canvas = rgba;
                let is_animated = reader.info().is_animated();
                if !is_animated {
                    frames.push(Frame {
                        width,
                        height,
                        delay_ms: 100,
                        bgra: rgba_to_premul_bgra(&canvas),
                    });
                    break;
                }
            }
            Some(fc) => {
                seen_any = true;
                let fw = fc.width.min(width);
                let fh = fc.height.min(height);

                let saved = if fc.dispose_op == png::DisposeOp::Previous {
                    Some(canvas.clone())
                } else {
                    None
                };

                // The decoded pixels are the sub-frame placed at the origin
                // of the output buffer.
                let rgba = png_to_rgba(&buffer, fw, fh, out.color_type);
                let src = &rgba[..];
                for y in 0..fh {
                    let dy = fc.y_offset + y;
                    if dy >= height {
                        break;
                    }
                    let src_start = (y * fw * 4) as usize;
                    let dst_start = ((dy * width + fc.x_offset) * 4) as usize;
                    let copy_w = fw.min(width.saturating_sub(fc.x_offset)) * 4;
                    if copy_w == 0 {
                        break;
                    }
                    let src_end = (src_start + copy_w as usize).min(src.len());
                    let dst_end = (dst_start + (src_end - src_start)).min(canvas.len());
                    if src_end > src_start && dst_end > dst_start {
                        let n = (src_end - src_start).min(dst_end - dst_start);
                        if fc.blend_op == png::BlendOp::Over {
                            alpha_over(
                                &mut canvas[dst_start..dst_start + n],
                                &src[src_start..src_start + n],
                            );
                        } else {
                            canvas[dst_start..dst_start + n]
                                .copy_from_slice(&src[src_start..src_start + n]);
                        }
                    }
                }

                let den = if fc.delay_den == 0 { 100 } else { fc.delay_den as u32 };
                let delay_ms = (fc.delay_num as u32 * 1000) / den;

                frames.push(Frame {
                    width,
                    height,
                    delay_ms: delay_ms.max(10),
                    bgra: rgba_to_premul_bgra(&canvas),
                });

                match fc.dispose_op {
                    png::DisposeOp::Background => {
                        clear_rect(
                            &mut canvas,
                            width,
                            height,
                            fc.x_offset,
                            fc.y_offset,
                            fw,
                            fh,
                        );
                    }
                    png::DisposeOp::Previous => {
                        if let Some(saved) = saved {
                            canvas = saved;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    if frames.is_empty() {
        return Err("png contains no frames".into());
    }

    Ok(FrameSet {
        width,
        height,
        frames,
        source,
    })
}

/// Decodes a PNG or APNG from a file path.
fn decode_png(path: &Path) -> Result<FrameSet, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    decode_png_from_reader(BufReader::new(file), path.display().to_string())
}

/// Decodes a static or animated WebP file.
fn decode_webp(path: &Path) -> Result<FrameSet, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let reader = BufReader::new(file);
    let decoder = WebPDecoder::new(reader).map_err(|e| format!("webp header: {e}"))?;

    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 {
        return Err("webp has zero size".into());
    }

    if decoder.has_animation() {
        let frames_iter = decoder.into_frames();
        let collected = frames_iter
            .collect_frames()
            .map_err(|e| format!("decode webp animation: {e}"))?;

        let mut frames = Vec::with_capacity(collected.len());
        for frame in collected {
            let (num, denom) = frame.delay().numer_denom_ms();
            let delay_ms = if denom != 0 {
                (num / denom).max(10)
            } else {
                100
            };
            let rgba_buf = frame.into_buffer().into_raw();
            let bgra = rgba_to_premul_bgra(&rgba_buf);
            frames.push(Frame {
                width,
                height,
                delay_ms,
                bgra,
            });
        }

        if frames.is_empty() {
            return Err("animated webp contains no frames".into());
        }

        Ok(FrameSet {
            width,
            height,
            frames,
            source: path.display().to_string(),
        })
    } else {
        let img = image::DynamicImage::from_decoder(decoder)
            .map_err(|e| format!("decode static webp: {e}"))?;
        let rgba = img.into_rgba8().into_raw();
        let bgra = rgba_to_premul_bgra(&rgba);

        Ok(FrameSet {
            width,
            height,
            frames: vec![Frame {
                width,
                height,
                delay_ms: 100,
                bgra,
            }],
            source: path.display().to_string(),
        })
    }
}

/// Converts the PNG crate output (grayscale / gray+alpha / RGB / RGBA at
/// 8 bits per sample) into straight-alpha RGBA.
fn png_to_rgba(
    buffer: &[u8],
    width: u32,
    height: u32,
    color_type: png::ColorType,
) -> Vec<u8> {
    let samples = match color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Indexed => 1,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
    };
    let mut out = vec![0u8; (width * height * 4) as usize];
    let px = (width * height) as usize;

    for i in 0..px {
        let s = i * samples;
        let d = i * 4;
        if s + samples > buffer.len() || d + 4 > out.len() {
            break;
        }
        match color_type {
            png::ColorType::Grayscale => {
                let g = buffer[s];
                out[d] = g;
                out[d + 1] = g;
                out[d + 2] = g;
                out[d + 3] = 255;
            }
            png::ColorType::GrayscaleAlpha => {
                let g = buffer[s];
                out[d] = g;
                out[d + 1] = g;
                out[d + 2] = g;
                out[d + 3] = buffer[s + 1];
            }
            png::ColorType::Indexed => {
                let g = buffer[s];
                out[d] = g;
                out[d + 1] = g;
                out[d + 2] = g;
                out[d + 3] = 255;
            }
            png::ColorType::Rgb => {
                out[d] = buffer[s];
                out[d + 1] = buffer[s + 1];
                out[d + 2] = buffer[s + 2];
                out[d + 3] = 255;
            }
            png::ColorType::Rgba => {
                out[d] = buffer[s];
                out[d + 1] = buffer[s + 1];
                out[d + 2] = buffer[s + 2];
                out[d + 3] = buffer[s + 3];
            }
        }
    }
    out
}

/// Straight RGBA -> premultiplied BGRA.
fn rgba_to_premul_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; rgba.len()];
    for i in (0..rgba.len()).step_by(4) {
        let a = rgba[i + 3] as u32;
        out[i] = ((rgba[i + 2] as u32 * a + 127) / 255) as u8;
        out[i + 1] = ((rgba[i + 1] as u32 * a + 127) / 255) as u8;
        out[i + 2] = ((rgba[i] as u32 * a + 127) / 255) as u8;
        out[i + 3] = rgba[i + 3];
    }
    out
}

/// Source-over alpha blend of `src` onto `dst` (both straight RGBA).
fn alpha_over(dst: &mut [u8], src: &[u8]) {
    for i in (0..dst.len().min(src.len())).step_by(4) {
        let sa = src[i + 3] as u32;
        if sa == 255 {
            dst[i..i + 4].copy_from_slice(&src[i..i + 4]);
            continue;
        }
        if sa == 0 {
            continue;
        }
        let da = dst[i + 3] as u32;
        let out_a = sa + da * (255 - sa) / 255;
        if out_a == 0 {
            dst[i] = 0;
            dst[i + 1] = 0;
            dst[i + 2] = 0;
            dst[i + 3] = 0;
            continue;
        }
        for c in 0..3 {
            let sc = src[i + c] as u32 * sa;
            let dc = dst[i + c] as u32 * da * (255 - sa) / 255;
            dst[i + c] = ((sc + dc) / out_a).min(255) as u8;
        }
        dst[i + 3] = out_a.min(255) as u8;
    }
}

/// Clears a rectangle of the canvas to fully transparent.
fn clear_rect(
    canvas: &mut [u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) {
    for dy in y..(y + h).min(height) {
        let start = ((dy * width + x) * 4) as usize;
        let end = (start + (w.min(width.saturating_sub(x)) * 4) as usize).min(canvas.len());
        if start < end {
            canvas[start..end].iter_mut().for_each(|b| *b = 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premultiply_is_correct() {
        let rgba = [255u8, 128, 64, 128];
        let out = rgba_to_premul_bgra(&rgba);
        assert_eq!(out[0], 32); // blue * alpha: (64 * 128 + 127) / 255 = 32
        assert_eq!(out[1], 64);
        assert_eq!(out[2], 128);
        assert_eq!(out[3], 128);
    }

    #[test]
    fn clear_rect_zeroes_pixels() {
        let mut canvas = vec![255u8; 4 * 4 * 4];
        clear_rect(&mut canvas, 4, 4, 1, 1, 2, 2);
        // pixel (0,0) must be untouched, pixel (1,1) cleared
        assert_eq!(canvas[0], 255);
        assert_eq!(canvas[(1 * 4 + 1) * 4], 0);
    }

    #[test]
    fn gif_subframe_transparency_preserves_canvas() {
        let mut canvas = vec![100u8; 2 * 2 * 4];
        let subframe = [
            0u8, 0, 0, 0,      // transparent pixel (alpha == 0) -> must preserve canvas
            255, 200, 150, 255 // opaque pixel (alpha == 255) -> must overwrite canvas
        ];
        let copy_cols = 2usize;
        for (s, d) in subframe[..copy_cols * 4].chunks_exact(4).zip(canvas[..copy_cols * 4].chunks_exact_mut(4)) {
            if s[3] != 0 {
                d.copy_from_slice(s);
            }
        }
        // Pixel 0 must remain 100
        assert_eq!(canvas[0..4], [100, 100, 100, 100]);
        // Pixel 1 must be overwritten
        assert_eq!(canvas[4..8], [255, 200, 150, 255]);
    }

    #[test]
    fn spike_apng_subframe_roundtrip() {
        let mut buf = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut buf, 10, 10);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(3, 0).unwrap();
            let mut writer = encoder.write_header().unwrap();

            // Frame 0: full canvas 10x10 all black (0, 0, 0, 255)
            writer.set_frame_delay(1, 20).unwrap();
            let frame0 = vec![0u8, 0, 0, 255].repeat(10 * 10);
            writer.write_image_data(&frame0).unwrap();

            // Frame 1: sub-frame at (2, 2), size 4x4, all red (255, 0, 0, 255)
            writer.set_frame_dimension(4, 4).unwrap();
            writer.set_frame_position(2, 2).unwrap();
            writer.set_blend_op(png::BlendOp::Source).unwrap();
            writer.set_dispose_op(png::DisposeOp::None).unwrap();
            writer.set_frame_delay(1, 20).unwrap();
            let frame1 = vec![255u8, 0, 0, 255].repeat(4 * 4);
            writer.write_image_data(&frame1).unwrap();

            // Frame 2: sub-frame at (5, 5), size 3x3, all blue (0, 0, 255, 255)
            writer.set_frame_dimension(3, 3).unwrap();
            writer.set_frame_position(5, 5).unwrap();
            writer.set_blend_op(png::BlendOp::Source).unwrap();
            writer.set_dispose_op(png::DisposeOp::None).unwrap();
            writer.set_frame_delay(1, 20).unwrap();
            let frame2 = vec![0u8, 0, 255, 255].repeat(3 * 3);
            writer.write_image_data(&frame2).unwrap();
        }

        // Now decode with our own decoder
        let decoded = decode_png_from_reader(std::io::Cursor::new(&buf), "memory.apng".into()).unwrap();
        assert_eq!(decoded.width, 10);
        assert_eq!(decoded.height, 10);
        assert_eq!(decoded.frames.len(), 3);

        // Frame 0: (0,0) is black (BGRA: 0, 0, 0, 255)
        let f0 = &decoded.frames[0];
        assert_eq!(f0.bgra[0..4], [0, 0, 0, 255]);

        // Frame 1: (0,0) is black, but (2,2) is red (BGRA: 0, 0, 255, 255)
        let f1 = &decoded.frames[1];
        let idx_2_2 = ((2 * 10 + 2) * 4) as usize;
        assert_eq!(f1.bgra[0..4], [0, 0, 0, 255]);
        assert_eq!(f1.bgra[idx_2_2..idx_2_2 + 4], [0, 0, 255, 255]);

        // Frame 2: (2,2) is still red (DisposeOp::None preserved), and (5,5) is blue (BGRA: 255, 0, 0, 255)
        let f2 = &decoded.frames[2];
        let idx_5_5 = ((5 * 10 + 5) * 4) as usize;
        assert_eq!(f2.bgra[idx_2_2..idx_2_2 + 4], [0, 0, 255, 255]);
        assert_eq!(f2.bgra[idx_5_5..idx_5_5 + 4], [255, 0, 0, 255]);
    }

    #[test]
    fn decode_file_dispatches_webp() {
        let res = decode_file(Path::new("non_existent_test_file.webp"));
        assert!(res.is_err());
        let err = res.err().unwrap();
        assert!(!err.contains("unsupported media type"));
        assert!(err.contains("open non_existent_test_file.webp"));
    }
}
