//! High-performance APNG encoder with 5-stage optimization pipeline:
//! 1. Frame coalescing & FPS downsampling
//! 2. Bilinear spatial downscaling
//! 3. Chroma key background removal (optional)
//! 4. Dirty-alpha zeroing for maximum DEFLATE compression
//! 5. Inter-frame difference bounding-box subframe encoding (fcTL / fdAT)

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use super::chroma::{apply_to_frame, ChromaKeyConfig};
use super::decode::Frame;
use super::resample::resample_bilinear_into;
use super::wmf::WmfVideoDecoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct ConversionOptions {
    pub target_fps: u32,          // e.g. 15, 20, 24, 30 (default 20)
    pub max_dimension: u32,       // e.g. 384, 512 (0 = keep original)
    pub chroma_key: ChromaKeyConfig,
    #[allow(dead_code)]
    pub quality_level: u8,        // Compression level 1..9 (default 6)
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self {
            target_fps: 20,
            max_dimension: 384,
            chroma_key: ChromaKeyConfig::default(),
            quality_level: 6,
        }
    }
}

pub struct ConversionProgress {
    pub current_frame: usize,
    pub total_frames: usize,
    pub percent: f32,
}

/// Zeroes RGB channels for any pixel where alpha is 0 (or below noise threshold)
/// so that transparent regions compress efficiently with DEFLATE.
pub fn clean_dirty_alpha(rgba: &mut [u8]) {
    for chunk in rgba.chunks_exact_mut(4) {
        if chunk[3] < 4 {
            chunk[0] = 0;
            chunk[1] = 0;
            chunk[2] = 0;
            chunk[3] = 0;
        }
    }
}

/// Calculates the minimal axis-aligned bounding box of differing pixels between two frames.
/// Returns None if frames are identical.
pub fn calculate_diff_box(prev: &[u8], curr: &[u8], width: u32, height: u32) -> Option<Rect> {
    if prev.len() != curr.len() || prev.len() != (width * height * 4) as usize {
        return Some(Rect { x: 0, y: 0, width, height });
    }

    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut changed = false;

    for y in 0..height {
        let row_offset = (y * width * 4) as usize;
        let prev_row = &prev[row_offset..row_offset + (width * 4) as usize];
        let curr_row = &curr[row_offset..row_offset + (width * 4) as usize];

        if prev_row == curr_row {
            continue;
        }

        for x in 0..width {
            let px_offset = (x * 4) as usize;
            if prev_row[px_offset..px_offset + 4] != curr_row[px_offset..px_offset + 4] {
                changed = true;
                if x < min_x { min_x = x; }
                if x > max_x { max_x = x; }
                if y < min_y { min_y = y; }
                if y > max_y { max_y = y; }
            }
        }
    }

    if !changed {
        None
    } else {
        Some(Rect {
            x: min_x,
            y: min_y,
            width: max_x - min_x + 1,
            height: max_y - min_y + 1,
        })
    }
}

/// Extracts a rectangular subframe from full-sized RGBA pixel data.
pub fn extract_subframe(rgba: &[u8], full_width: u32, rect: &Rect) -> Vec<u8> {
    let mut sub = Vec::with_capacity((rect.width * rect.height * 4) as usize);
    let row_bytes = (rect.width * 4) as usize;

    for y in 0..rect.height {
        let src_y = rect.y + y;
        let src_offset = ((src_y * full_width + rect.x) * 4) as usize;
        sub.extend_from_slice(&rgba[src_offset..src_offset + row_bytes]);
    }

    sub
}

/// Converts a 32-bit BGRA buffer to standard RGBA in place or into a new buffer.
pub fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = vec![0u8; bgra.len()];
    for (src, dst) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        dst[0] = src[2]; // R
        dst[1] = src[1]; // G
        dst[2] = src[0]; // B
        dst[3] = src[3]; // A
    }
    rgba
}

#[allow(dead_code)]
pub struct FrameItem {
    pub rgba: Vec<u8>,
    pub delay_ms: u16,
}

/// Encodes a sequence of RGBA frames into an optimized APNG file.
#[allow(dead_code)]
pub fn encode_apng(
    frames: Vec<FrameItem>,
    width: u32,
    height: u32,
    out_path: &Path,
) -> Result<(), String> {
    if frames.is_empty() {
        return Err("No frames to encode".to_string());
    }

    let file = File::create(out_path)
        .map_err(|e| format!("Failed to create output file: {:?}", e))?;
    let ref mut w = BufWriter::new(file);

    let mut encoder = png::Encoder::new(w, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_animated(frames.len() as u32, 0)
        .map_err(|e| format!("set_animated failed: {:?}", e))?;

    let mut writer = encoder.write_header()
        .map_err(|e| format!("write_header failed: {:?}", e))?;

    let mut prev_frame: Option<Vec<u8>> = None;

    for (idx, frame) in frames.into_iter().enumerate() {
        let delay_ms = frame.delay_ms.max(10);
        let num = delay_ms;
        let den = 1000;

        if idx == 0 {
            // First frame: must be full canvas
            writer.reset_frame_position()
                .map_err(|e| format!("reset_frame_position failed: {:?}", e))?;
            writer.set_frame_dimension(width, height)
                .map_err(|e| format!("set_frame_dimension failed: {:?}", e))?;
            writer.set_frame_position(0, 0)
                .map_err(|e| format!("set_frame_position failed: {:?}", e))?;
            writer.set_frame_delay(num, den)
                .map_err(|e| format!("set_frame_delay failed: {:?}", e))?;
            writer.set_blend_op(png::BlendOp::Source)
                .map_err(|e| format!("set_blend_op failed: {:?}", e))?;
            writer.set_dispose_op(png::DisposeOp::None)
                .map_err(|e| format!("set_dispose_op failed: {:?}", e))?;

            writer.write_image_data(&frame.rgba)
                .map_err(|e| format!("write_image_data failed: {:?}", e))?;

            prev_frame = Some(frame.rgba);
        } else {
            let prev = prev_frame.as_ref().unwrap();
            let rect = calculate_diff_box(prev, &frame.rgba, width, height)
                .unwrap_or(Rect { x: 0, y: 0, width: 1, height: 1 });

            let sub_data = extract_subframe(&frame.rgba, width, &rect);

            writer.reset_frame_position()
                .map_err(|e| format!("reset_frame_position failed: {:?}", e))?;
            writer.set_frame_dimension(rect.width, rect.height)
                .map_err(|e| format!("set_frame_dimension failed: {:?}", e))?;
            writer.set_frame_position(rect.x, rect.y)
                .map_err(|e| format!("set_frame_position failed: {:?}", e))?;
            writer.set_frame_delay(num, den)
                .map_err(|e| format!("set_frame_delay failed: {:?}", e))?;
            writer.set_blend_op(png::BlendOp::Source)
                .map_err(|e| format!("set_blend_op failed: {:?}", e))?;
            writer.set_dispose_op(png::DisposeOp::None)
                .map_err(|e| format!("set_dispose_op failed: {:?}", e))?;

            writer.write_image_data(&sub_data)
                .map_err(|e| format!("write_image_data failed: {:?}", e))?;

            prev_frame = Some(frame.rgba);
        }
    }

    writer.finish()
        .map_err(|e| format!("writer finish failed: {:?}", e))?;

    Ok(())
}

/// Converts an MP4 file to an optimized APNG using WMF decoder and APNG pipeline (streaming mode).
pub fn convert_mp4_to_apng<F>(
    in_path: &Path,
    out_path: &Path,
    options: &ConversionOptions,
    cancel_flag: &std::sync::atomic::AtomicBool,
    mut progress_cb: F,
) -> Result<(), String>
where
    F: FnMut(ConversionProgress),
{
    let mut decoder = WmfVideoDecoder::open(in_path)?;
    let meta = decoder.metadata.clone();

    if meta.width == 0 || meta.height == 0 {
        return Err("Invalid video dimensions (0x0)".to_string());
    }

    // Determine target dimensions
    let (target_w, target_h) = if options.max_dimension > 0 && (meta.width > options.max_dimension || meta.height > options.max_dimension) {
        if meta.width >= meta.height {
            let h = ((meta.height as f64 * options.max_dimension as f64) / meta.width as f64).round() as u32;
            (options.max_dimension, h.max(1))
        } else {
            let w = ((meta.width as f64 * options.max_dimension as f64) / meta.height as f64).round() as u32;
            (w.max(1), options.max_dimension)
        }
    } else {
        (meta.width, meta.height)
    };

    let target_fps = options.target_fps.clamp(5, 60);
    let frame_interval_secs = 1.0 / (target_fps as f64);
    let total_est_frames = if meta.duration_secs > 0.0 {
        (meta.duration_secs * target_fps as f64).round() as usize
    } else {
        0
    };

    let file = File::create(out_path)
        .map_err(|e| format!("Failed to create output file: {:?}", e))?;
    let mut w = BufWriter::new(file);

    let mut encoder = png::Encoder::new(&mut w, target_w, target_h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let initial_hint = (total_est_frames.max(1) as u32).min(500_000);
    encoder.set_animated(initial_hint, 0)
        .map_err(|e| format!("set_animated failed: {:?}", e))?;

    let mut writer = encoder.write_header()
        .map_err(|e| format!("write_header failed: {:?}", e))?;

    let mut written_count = 0u32;
    let mut prev_frame: Option<Vec<u8>> = None;
    let mut pending_frame: Option<(Vec<u8>, u16)> = None;

    let write_frame_to_apng = |frame_rgba: &[u8], delay_ms: u16, writer: &mut png::Writer<&mut BufWriter<File>>, prev: &mut Option<Vec<u8>>, count: &mut u32| -> Result<(), String> {
        let delay_ms = delay_ms.max(10);
        let num = delay_ms;
        let den = 1000;

        if *count == 0 {
            writer.reset_frame_position()
                .map_err(|e| format!("reset_frame_position failed: {:?}", e))?;
            writer.set_frame_dimension(target_w, target_h)
                .map_err(|e| format!("set_frame_dimension failed: {:?}", e))?;
            writer.set_frame_position(0, 0)
                .map_err(|e| format!("set_frame_position failed: {:?}", e))?;
            writer.set_frame_delay(num, den)
                .map_err(|e| format!("set_frame_delay failed: {:?}", e))?;
            writer.set_blend_op(png::BlendOp::Source)
                .map_err(|e| format!("set_blend_op failed: {:?}", e))?;
            writer.set_dispose_op(png::DisposeOp::None)
                .map_err(|e| format!("set_dispose_op failed: {:?}", e))?;
            writer.write_image_data(frame_rgba)
                .map_err(|e| format!("write_image_data failed: {:?}", e))?;
        } else {
            let prev_ref = prev.as_ref().unwrap();
            let rect = calculate_diff_box(prev_ref, frame_rgba, target_w, target_h)
                .unwrap_or(Rect { x: 0, y: 0, width: 1, height: 1 });

            let sub_data = extract_subframe(frame_rgba, target_w, &rect);

            writer.reset_frame_position()
                .map_err(|e| format!("reset_frame_position failed: {:?}", e))?;
            writer.set_frame_dimension(rect.width, rect.height)
                .map_err(|e| format!("set_frame_dimension failed: {:?}", e))?;
            writer.set_frame_position(rect.x, rect.y)
                .map_err(|e| format!("set_frame_position failed: {:?}", e))?;
            writer.set_frame_delay(num, den)
                .map_err(|e| format!("set_frame_delay failed: {:?}", e))?;
            writer.set_blend_op(png::BlendOp::Source)
                .map_err(|e| format!("set_blend_op failed: {:?}", e))?;
            writer.set_dispose_op(png::DisposeOp::None)
                .map_err(|e| format!("set_dispose_op failed: {:?}", e))?;
            writer.write_image_data(&sub_data)
                .map_err(|e| format!("write_image_data failed: {:?}", e))?;
        }

        *prev = Some(frame_rgba.to_vec());
        *count += 1;
        Ok(())
    };

    let mut next_timestamp_target = 0.0f64;
    let mut decoded_count = 0usize;

    while let Some(decoded) = decoder.read_next_frame()? {
        if cancel_flag.load(std::sync::atomic::Ordering::Relaxed) {
            drop(writer);
            drop(w);
            let _ = std::fs::remove_file(out_path);
            return Err("Conversion cancelled by user".to_string());
        }

        if decoded.timestamp_secs + 0.005 < next_timestamp_target {
            continue;
        }

        next_timestamp_target = decoded.timestamp_secs + frame_interval_secs;
        decoded_count += 1;

        // Step 1: Ensure Alpha is 255 for opaque video
        let mut bgra = decoded.data;
        for px in bgra.chunks_exact_mut(4) {
            px[3] = 255;
        }

        // Step 2: Spatial downscaling if required
        let (processed_bgra, cur_w, cur_h) = if target_w != meta.width || target_h != meta.height {
            let mut scaled = vec![0u8; (target_w * target_h * 4) as usize];
            resample_bilinear_into(&bgra, meta.width, meta.height, &mut scaled, target_w, target_h);
            (scaled, target_w, target_h)
        } else {
            (bgra, meta.width, meta.height)
        };

        // Step 3: Chroma Key (if enabled)
        let keyed_bgra = if options.chroma_key.enabled {
            let frame = Frame {
                width: cur_w,
                height: cur_h,
                delay_ms: (frame_interval_secs * 1000.0).round() as u32,
                bgra: processed_bgra,
            };
            let keyed = apply_to_frame(&frame, &options.chroma_key);
            keyed.bgra
        } else {
            processed_bgra
        };

        // Step 4: Convert to RGBA and perform dirty-alpha cleanup
        let mut rgba = bgra_to_rgba(&keyed_bgra);
        if options.chroma_key.enabled {
            clean_dirty_alpha(&mut rgba);
        }

        // Step 5: Frame coalescing & streaming write
        let delay_ms = (frame_interval_secs * 1000.0).round() as u16;
        if let Some((ref mut p_rgba, ref mut p_delay)) = pending_frame {
            if *p_rgba == rgba {
                *p_delay = p_delay.saturating_add(delay_ms);
            } else {
                let to_write_rgba = std::mem::replace(p_rgba, rgba);
                let to_write_delay = *p_delay;
                *p_delay = delay_ms;
                write_frame_to_apng(&to_write_rgba, to_write_delay, &mut writer, &mut prev_frame, &mut written_count)?;
            }
        } else {
            pending_frame = Some((rgba, delay_ms));
        }

        let percent = if total_est_frames > 0 {
            ((decoded_count as f32) / (total_est_frames as f32) * 98.0).min(98.0)
        } else {
            50.0
        };

        progress_cb(ConversionProgress {
            current_frame: decoded_count,
            total_frames: total_est_frames,
            percent,
        });
    }

    // Flush final pending frame
    if let Some((p_rgba, p_delay)) = pending_frame.take() {
        write_frame_to_apng(&p_rgba, p_delay, &mut writer, &mut prev_frame, &mut written_count)?;
    }

    if written_count == 0 {
        drop(writer);
        drop(w);
        let _ = std::fs::remove_file(out_path);
        return Err("No valid frames decoded from video".to_string());
    }

    writer.finish().map_err(|e| format!("writer finish failed: {:?}", e))?;
    drop(w);

    // Step 6: Patch acTL chunk with exact written_count
    let _ = patch_actl_num_frames(out_path, written_count);

    progress_cb(ConversionProgress {
        current_frame: decoded_count,
        total_frames: total_est_frames,
        percent: 100.0,
    });

    Ok(())
}

pub fn patch_actl_num_frames(file_path: &Path, actual_count: u32) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    let mut file = OpenOptions::new().read(true).write(true).open(file_path)?;
    let mut header = [0u8; 128];
    let n = file.read(&mut header)?;
    if let Some(pos) = header[..n].windows(4).position(|w| w == b"acTL") {
        let num_frames_offset = pos + 4;
        let crc_offset = pos + 12;

        if crc_offset + 4 <= n {
            header[num_frames_offset..num_frames_offset + 4].copy_from_slice(&actual_count.to_be_bytes());
            let crc = crc32(&header[pos..pos + 12]);
            header[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_be_bytes());

            file.seek(SeekFrom::Start(pos as u64))?;
            file.write_all(&header[pos..crc_offset + 4])?;
        }
    }
    Ok(())
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_dirty_alpha() {
        let mut px = vec![
            255, 100, 50, 0,    // Dirty transparent pixel
            200, 200, 200, 2,   // Noise transparent pixel
            128, 128, 128, 255, // Opaque pixel
        ];
        clean_dirty_alpha(&mut px);

        assert_eq!(&px[0..4], &[0, 0, 0, 0]);
        assert_eq!(&px[4..8], &[0, 0, 0, 0]);
        assert_eq!(&px[8..12], &[128, 128, 128, 255]);
    }

    #[test]
    fn test_calculate_diff_box() {
        let w = 4;
        let h = 4;
        let prev = vec![0u8; (w * h * 4) as usize];
        let mut curr = prev.clone();

        // Identical frames
        assert_eq!(calculate_diff_box(&prev, &curr, w, h), None);

        // Change single pixel at (2, 1)
        let idx = ((1 * w + 2) * 4) as usize;
        curr[idx] = 255;
        curr[idx + 3] = 255;

        let diff = calculate_diff_box(&prev, &curr, w, h).expect("Should detect change");
        assert_eq!(diff, Rect { x: 2, y: 1, width: 1, height: 1 });

        // Change another pixel at (3, 3)
        let idx2 = ((3 * w + 3) * 4) as usize;
        curr[idx2] = 255;
        curr[idx2 + 3] = 255;

        let diff2 = calculate_diff_box(&prev, &curr, w, h).expect("Should detect change");
        assert_eq!(diff2, Rect { x: 2, y: 1, width: 2, height: 3 });
    }

    #[test]
    fn test_extract_subframe() {
        let w = 3;
        let h = 3;
        // 3x3 grid with pixel colors = (x, y)
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let off = ((y * w + x) * 4) as usize;
                rgba[off] = x as u8;
                rgba[off + 1] = y as u8;
                rgba[off + 3] = 255;
            }
        }

        let rect = Rect { x: 1, y: 1, width: 2, height: 2 };
        let sub = extract_subframe(&rgba, w, &rect);
        assert_eq!(sub.len(), 2 * 2 * 4);

        // Subframe top-left should be (1, 1)
        assert_eq!(sub[0], 1);
        assert_eq!(sub[1], 1);

        // Subframe top-right should be (2, 1)
        assert_eq!(sub[4], 2);
        assert_eq!(sub[5], 1);
    }

    #[test]
    fn test_png_encoder_streaming_mismatch() {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, 10, 10);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(100, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        
        let frame_data = vec![255u8; 10 * 10 * 4];
        writer.set_frame_delay(100, 1000).unwrap();
        writer.write_image_data(&frame_data).unwrap();

        // Write 2nd frame
        writer.set_frame_dimension(10, 10).unwrap();
        writer.set_frame_position(0, 0).unwrap();
        writer.set_frame_delay(100, 1000).unwrap();
        writer.write_image_data(&frame_data).unwrap();

        let res = writer.finish();
        println!("writer.finish() result with 2 frames instead of 100: {:?}", res);
        assert!(res.is_ok());

        // Check acTL chunk at offset 33
        assert_eq!(&out[37..41], b"acTL");
        let initial_num_frames = u32::from_be_bytes([out[41], out[42], out[43], out[44]]);
        println!("initial acTL num_frames: {}", initial_num_frames);
        assert_eq!(initial_num_frames, 100);

        // Test decoder on this APNG
        let decoder = png::Decoder::new(&out[..]);
        let reader = decoder.read_info().unwrap();
        let info = reader.info();
        println!("Reader animation control: {:?}", info.animation_control);

        // Verify CRC computation
        let actl_data = &out[37..49]; // "acTL" + 4 bytes num_frames + 4 bytes num_plays
        let actl_crc = u32::from_be_bytes([out[49], out[50], out[51], out[52]]);
        assert_eq!(crc32(actl_data), actl_crc);
        println!("CRC32 match verified!");
    }
}
