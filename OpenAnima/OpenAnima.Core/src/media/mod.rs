//! Media decoding: GIF, PNG and APNG into premultiplied BGRA frames.

pub mod chroma;
pub mod decode;
pub mod resample;

pub use chroma::{apply_chroma_key, ChromaKeyConfig};
pub use decode::{decode_file, Frame, FrameSet};
pub use resample::{resample_bilinear, resample_bilinear_into, resample_nearest, resample_nearest_into};
