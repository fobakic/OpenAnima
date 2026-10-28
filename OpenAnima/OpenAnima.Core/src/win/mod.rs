//! Win32 helpers: overlay windows and screen capture.

#[cfg(feature = "triggers")]
pub mod capture;
pub mod overlay;

#[cfg(feature = "triggers")]
#[allow(unused_imports)]
pub use capture::ScreenCapture;
