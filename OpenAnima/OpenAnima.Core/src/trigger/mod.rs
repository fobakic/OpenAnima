//! Trigger engine: screen capture + perceptual matching against pack
//! reference frames.

pub mod ahash;
pub mod engine;
pub mod npy;

#[allow(unused_imports)]
pub use engine::{TriggerEngine, TriggerPack};
