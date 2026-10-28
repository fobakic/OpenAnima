//! Windows Media Foundation (WMF) video reader for MP4 and hardware formats.
//! Decodes frames directly to standard 32-bit RGB/BGRA without external binaries (ffmpeg).

use std::path::Path;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::Media::MediaFoundation::{
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL,
    MFShutdown, MFStartup,
    IMFAttributes, IMFSourceReader,
    MFMediaType_Video, MFVideoFormat_RGB32,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_DEFAULT_STRIDE, MF_PD_DURATION, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING,
    MF_VERSION, MFSTARTUP_NOSOCKET,
};

/// RAII Guard ensuring CoInitializeEx and MFStartup/Shutdown paired lifecycles.
pub struct WmfSession {
    com_initialized: bool,
}

impl WmfSession {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            let com_initialized = hr.is_ok();

            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET) {
                if com_initialized {
                    CoUninitialize();
                }
                return Err(format!("MFStartup failed: {:?}", e));
            }

            Ok(Self { com_initialized })
        }
    }
}

impl Drop for WmfSession {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            if self.com_initialized {
                CoUninitialize();
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct VideoMetadata {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_secs: f64,
    pub stride: i32,
}

pub struct DecodedFrame {
    pub timestamp_secs: f64,
    /// RGBA/BGRA 32-bit pixel buffer in top-down row order
    pub data: Vec<u8>,
}

pub struct WmfVideoDecoder {
    reader: Option<IMFSourceReader>,
    pub metadata: VideoMetadata,
    _session: WmfSession,
}

impl Drop for WmfVideoDecoder {
    fn drop(&mut self) {
        // Explicitly release COM IMFSourceReader while COM and MF are still alive
        self.reader = None;
    }
}

impl WmfVideoDecoder {
    pub fn open(path: &Path) -> Result<Self, String> {
        let session = WmfSession::new()?;

        let path_str = if let Ok(canon) = path.canonicalize() {
            let s = canon.to_string_lossy().to_string();
            if let Some(stripped) = s.strip_prefix(r"\\?\") {
                stripped.to_string()
            } else {
                s
            }
        } else {
            path.to_string_lossy().to_string()
        };
        let hstring = HSTRING::from(path_str.as_str());

        unsafe {
            // Configure source reader attributes
            let mut attrs: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs, 2)
                .map_err(|e| format!("MFCreateAttributes failed: {:?}", e))?;
            
            let attrs = attrs.ok_or("Failed to create IMFAttributes")?;
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1)
                .map_err(|e| format!("Set advanced video processing failed: {:?}", e))?;

            // Create IMFSourceReader
            let reader = MFCreateSourceReaderFromURL(PCWSTR(hstring.as_ptr()), Some(&attrs))
                .map_err(|e| format!("MFCreateSourceReaderFromURL failed: {:?}", e))?;

            // Setup RGB32 output format
            let partial_type = MFCreateMediaType()
                .map_err(|e| format!("MFCreateMediaType failed: {:?}", e))?;

            partial_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| format!("Set MF_MT_MAJOR_TYPE failed: {:?}", e))?;
            partial_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(|e| format!("Set MF_MT_SUBTYPE failed: {:?}", e))?;

            reader.SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &partial_type)
                .map_err(|e| format!("SetCurrentMediaType failed: {:?}", e))?;

            // Query actual media type
            let current_type = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
                .map_err(|e| format!("GetCurrentMediaType failed: {:?}", e))?;

            let (width, height) = match current_type.GetUINT64(&MF_MT_FRAME_SIZE) {
                Ok(val) => ((val >> 32) as u32, (val & 0xFFFFFFFF) as u32),
                Err(e) => return Err(format!("Get MF_MT_FRAME_SIZE failed: {:?}", e)),
            };

            let fps = match current_type.GetUINT64(&MF_MT_FRAME_RATE) {
                Ok(val) => {
                    let num = (val >> 32) as u32;
                    let den = (val & 0xFFFFFFFF) as u32;
                    if den > 0 { num as f64 / den as f64 } else { 30.0 }
                }
                Err(_) => 30.0,
            };

            // Query stride
            let stride = match current_type.GetUINT32(&MF_MT_DEFAULT_STRIDE) {
                Ok(s) => s as i32,
                Err(_) => (width * 4) as i32,
            };

            let duration_secs = match reader.GetPresentationAttribute(
                MF_SOURCE_READER_MEDIASOURCE.0 as u32,
                &MF_PD_DURATION,
            ) {
                Ok(prop) => {
                    if let Ok(units) = u64::try_from(&prop) {
                        units as f64 / 10_000_000.0
                    } else {
                        0.0
                    }
                }
                Err(_) => 0.0,
            };

            let metadata = VideoMetadata {
                width,
                height,
                fps,
                duration_secs,
                stride,
            };

            Ok(Self {
                reader: Some(reader),
                metadata,
                _session: session,
            })
        }
    }

    pub fn read_next_frame(&mut self) -> Result<Option<DecodedFrame>, String> {
        let width = self.metadata.width;
        let height = self.metadata.height;
        let stride = self.metadata.stride;
        let pitch = stride.abs() as usize;
        let row_bytes = (width * 4) as usize;

        unsafe {
            loop {
                let mut actual_stream_index = 0u32;
                let mut stream_flags = 0u32;
                let mut timestamp = 0i64;
                let mut sample = None;

                let reader = self.reader.as_ref().ok_or("Reader is None")?;
                reader.ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    Some(&mut actual_stream_index),
                    Some(&mut stream_flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                ).map_err(|e| format!("ReadSample failed: {:?}", e))?;

                if (stream_flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32)) != 0 {
                    return Ok(None);
                }

                let Some(sample) = sample else {
                    continue;
                };

                let buffer = sample.ConvertToContiguousBuffer()
                    .map_err(|e| format!("ConvertToContiguousBuffer failed: {:?}", e))?;

                let mut ptr = std::ptr::null_mut();
                let mut max_len = 0u32;
                let mut cur_len = 0u32;
                buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len))
                    .map_err(|e| format!("Buffer Lock failed: {:?}", e))?;

                let src_slice = std::slice::from_raw_parts(ptr, cur_len as usize);

                let mut output = vec![0u8; (width * height * 4) as usize];

                let is_bottom_up = stride < 0;

                for y in 0..height as usize {
                    let src_y = if is_bottom_up {
                        height as usize - 1 - y
                    } else {
                        y
                    };

                    let src_offset = src_y * pitch;
                    let dst_offset = y * row_bytes;

                    if src_offset + row_bytes <= src_slice.len() {
                        output[dst_offset..dst_offset + row_bytes]
                            .copy_from_slice(&src_slice[src_offset..src_offset + row_bytes]);
                    }
                }

                let _ = buffer.Unlock();

                let timestamp_secs = if timestamp > 0 {
                    timestamp as f64 / 10_000_000.0
                } else {
                    0.0
                };

                return Ok(Some(DecodedFrame {
                    timestamp_secs,
                    data: output,
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wmf_open_real_mp4() {
        let candidates = [
            Path::new(r"C:\Users\fobakic\Downloads\video_2026-10-27_21-25-38.mp4"),
            Path::new(r"C:\Users\fobakic\Videos\2026-07-22 22-17-34.mp4"),
        ];
        for path in &candidates {
            if path.exists() {
                let mut dec = WmfVideoDecoder::open(path).expect("Failed to open MP4");
                assert!(dec.metadata.width > 0);
                assert!(dec.metadata.height > 0);
                assert!(dec.metadata.stride != 0);
                let frame = dec.read_next_frame().expect("Read frame failed");
                assert!(frame.is_some());
                let f = frame.unwrap();
                assert_eq!(f.data.len(), (dec.metadata.width * dec.metadata.height * 4) as usize);
                break;
            }
        }
    }
}
