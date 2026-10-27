//! Media decoding: GIF, PNG and APNG into premultiplied BGRA frames.

pub mod chroma;
pub mod decode;
pub mod resample;
pub mod wmf;
pub mod apng_encoder;

pub use chroma::{apply_chroma_key, ChromaKeyConfig};
pub use decode::{decode_file, FrameSet};
pub use resample::{resample_bilinear_into, resample_nearest_into};
