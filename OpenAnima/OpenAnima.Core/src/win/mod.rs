//! Win32 helpers: overlay windows and screen capture.

#[cfg(feature = "triggers")]
pub mod capture;
pub mod overlay;

#[cfg(feature = "triggers")]
pub use capture::ScreenCapture;
