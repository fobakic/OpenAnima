//! Win32 helpers: overlay windows and screen capture.

pub mod capture;
pub mod overlay;

pub use capture::ScreenCapture;
pub use overlay::{Overlay, OverlayId, OverlayManager};
