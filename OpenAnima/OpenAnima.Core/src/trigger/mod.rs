//! Trigger engine: screen capture + perceptual matching against pack
//! reference frames.

pub mod ahash;
pub mod engine;
pub mod npy;

pub use engine::{TriggerEngine, TriggerPack};
