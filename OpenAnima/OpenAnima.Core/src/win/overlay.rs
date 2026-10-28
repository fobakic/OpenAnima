//! Win32 overlay windows.
//!
//! Every animation is a top-level layered window rendered with
//! `UpdateLayeredWindow` (32-bit premultiplied BGRA). That gives us:
//!   * true per-pixel alpha,
//!   * click-through via `WS_EX_TRANSPARENT`,
//!   * always-on-top without activating the window (`WS_EX_NOACTIVATE`),
//!   * and â€” most importantly â€” many overlays inside a *single* process,
//!     instead of one interpreter per animation.
//!
//! All window state lives on the thread that owns the message loop.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetMonitorInfoW, HBRUSH, HBITMAP,
    HDC, HGDIOBJ, MonitorFromWindow, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowRect, RegisterClassW, SetWindowLongPtrW, SetWindowPos,
    UpdateLayeredWindow, HICON, HWND_NOTOPMOST, HWND_TOPMOST, SM_CXSCREEN, SM_CYSCREEN,
    SystemParametersInfoW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, SWP_HIDEWINDOW,
    SWP_NOREDRAW,
    ULW_ALPHA, WINDOW_EX_STYLE, WNDCLASS_STYLES, WM_DESTROY, WM_NCHITTEST, WM_MOVE,
    WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_MOUSEWHEEL, LoadCursorW, IDC_ARROW, WNDCLASSW, WS_EX_LAYERED, WS_EX_TOPMOST,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE, GWLP_USERDATA, GWL_EXSTYLE,
    AppendMenuW, CreatePopupMenu, DestroyMenu, FindWindowW, GetCursorPos, PostMessageW,
    RegisterWindowMessageW, SetForegroundWindow, ShowWindow, TrackPopupMenu, HMENU, MENU_ITEM_FLAGS,
    MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MF_UNCHECKED,
    SW_HIDE, SW_RESTORE, SW_SHOWNOACTIVATE, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_CONTEXTMENU,
    WM_NCRBUTTONDOWN, WM_NCRBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_NULL,
};

use crate::media::{apply_chroma_key, ChromaKeyConfig, FrameSet};

/// Diagnostics: set `OPENANIMA_CORE_PRESENT=0` to skip the actual blit, or
/// `OPENANIMA_CORE_TOPMOST=0` to skip re-asserting the always-on-top flag.
/// Both exist so a memory regression can be attributed to a single stage.
fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

fn presenting_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| env_flag("OPENANIMA_CORE_PRESENT") && env_flag("ANIMA_CORE_PRESENT"))
}

fn topmost_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| env_flag("OPENANIMA_CORE_TOPMOST") && env_flag("ANIMA_CORE_TOPMOST"))
}

/// Row order of the DIB used as the `UpdateLayeredWindow` source.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DibOrientation {
    /// `biHeight` is negative, so the rows are stored top-down.
    TopDown,
    /// `biHeight` is positive, so the rows are stored bottom-up.
    BottomUp,
}

fn dib_orientation() -> DibOrientation {
    use std::sync::OnceLock;
    static ORIENTATION: OnceLock<DibOrientation> = OnceLock::new();
    *ORIENTATION.get_or_init(|| {
        match std::env::var("ANIMA_CORE_DIB").as_deref() {
            Ok("bottomup") | Ok("bottom-up") => DibOrientation::BottomUp,
            _ => DibOrientation::TopDown,
        }
    })
}

/// Computes target pixel dimensions from raw dimensions, screen height and scale percentage.
/// Scale is expressed as a fraction of screen height (e.g. 0.20 = 20% of screen height).
pub fn compute_dimensions(raw_w: u32, raw_h: u32, screen_h: i32, scale: f64) -> (i32, i32) {
    let screen_h = screen_h.max(480);
    let scale = scale.clamp(0.01, 10.0);
    let target_h = (screen_h as f64 * scale).round().clamp(16.0, 2560.0) as i32;
    let aspect = (raw_w as f64) / (raw_h.max(1) as f64);
    let target_w = (target_h as f64 * aspect).round().clamp(16.0, 2560.0) as i32;
    (target_w, target_h)
}

/// Diagnostics: `ANIMA_CORE_BLIT=0` skips the copy into the DIB,
/// `ANIMA_CORE_TICK=0` skips the whole presentation tick.
fn blit_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| env_flag("ANIMA_CORE_BLIT"))
}

fn tick_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| env_flag("ANIMA_CORE_TICK"))
}

/// Headless mode: no window is created at all. Used for decoding and
/// profiling on a live desktop without touching the compositor.
fn headless_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("ANIMA_CORE_HEADLESS")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}

/// Atom of the registered overlay window class.
static CLASS_ATOM: AtomicU16 = AtomicU16::new(0);

/// Number of frames pushed to the screen since start-up.
pub static FRAMES_PRESENTED: AtomicU64 = AtomicU64::new(0);

/// Identifier of an overlay, unique for the lifetime of the process.
pub type OverlayId = u64;

/// Registry that maps a window handle to its overlay id. The mapping is
/// mirrored into `GWLP_USERDATA` so the window proc can find the overlay
/// without taking a lock on every message.
static OVERLAY_IDS: std::sync::Mutex<Option<HashMap<isize, OverlayId>>> =
    std::sync::Mutex::new(None);

fn register_id(hwnd: isize, id: OverlayId) {
    let mut guard = OVERLAY_IDS.lock().unwrap();
    guard.get_or_insert_with(HashMap::new).insert(hwnd, id);
}

fn unregister_id(hwnd: isize) {
    let mut guard = OVERLAY_IDS.lock().unwrap();
    if let Some(map) = guard.as_mut() {
        map.remove(&hwnd);
    }
}

fn lookup_id(hwnd: isize) -> Option<OverlayId> {
    let guard = OVERLAY_IDS.lock().unwrap();
    guard.as_ref().and_then(|m| m.get(&hwnd).copied())
}

/// Global handle to the overlay manager, so the window proc can reach it.
/// Set once during startup, before any window is created.
static MANAGER: AtomicPtr<OverlayManager> = AtomicPtr::new(std::ptr::null_mut());

thread_local! {
    static IN_PROGRAMMATIC_MOVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn with_programmatic_update<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    IN_PROGRAMMATIC_MOVE.with(|flag| {
        flag.set(true);
        let res = f();
        flag.set(false);
        res
    })
}

static LANG_OVERRIDE: AtomicU8 = AtomicU8::new(0);

extern "system" {
    fn GetUserDefaultUILanguage() -> u16;
}

pub fn set_core_language(lang: &str) {
    if lang.eq_ignore_ascii_case("ru") {
        LANG_OVERRIDE.store(2, Ordering::Release);
    } else {
        LANG_OVERRIDE.store(1, Ordering::Release);
    }
}

pub fn is_russian_locale() -> bool {
    let override_val = LANG_OVERRIDE.load(Ordering::Acquire);
    if override_val == 1 {
        return false;
    }
    if override_val == 2 {
        return true;
    }
    unsafe {
        let lang_id = GetUserDefaultUILanguage();
        (lang_id & 0x3FF) == 0x19
    }
}

/// Publishes the manager to the window proc.
///
/// # Safety
/// The manager must outlive every window created from it and must not be
/// moved while it is registered.
pub unsafe fn set_manager(manager: *mut OverlayManager) {
    MANAGER.store(manager, Ordering::Release);
}

pub fn get_desktop_workarea() -> RECT {
    let mut rc = RECT::default();
    unsafe {
        let ok = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rc as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        if ok.is_ok() && rc.right > rc.left && rc.bottom > rc.top {
            return rc;
        }
        RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        }
    }
}

pub fn get_desktop_workarea_for_hwnd(hwnd: HWND) -> RECT {
    if !hwnd.0.is_null() {
        unsafe {
            let hmonitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if !hmonitor.0.is_null() {
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetMonitorInfoW(hmonitor, &mut info).as_bool() {
                    let rc = info.rcWork;
                    if rc.right > rc.left && rc.bottom > rc.top {
                        return rc;
                    }
                }
            }
        }
    }
    get_desktop_workarea()
}

/// Dynamic pet physics state: gravity, mouse toss momentum, and springy bounce.
#[derive(Clone, Debug)]
pub struct PetPhysics {
    pub enabled: bool,
    pub bouncy: bool,
    pub vx: f64,
    pub vy: f64,
    pub exact_x: f64,
    pub exact_y: f64,
    pub is_dragging: bool,
    pub last_drag_x: i32,
    pub last_drag_y: i32,
    pub last_drag_time: std::time::Instant,
    pub is_grounded: bool,
    pub last_tick: std::time::Instant,
    pub spring_amp: f64,
    pub spring_phase: f64,
}

impl Default for PetPhysics {
    fn default() -> Self {
        Self {
            enabled: false, // OFF by default as requested!
            bouncy: true,  // When physics is turned on, spring bounce is enabled by default (toggleable)
            vx: 0.0,
            vy: 0.0,
            exact_x: 0.0,
            exact_y: 0.0,
            is_dragging: false,
            last_drag_x: 0,
            last_drag_y: 0,
            last_drag_time: std::time::Instant::now(),
            is_grounded: false,
            last_tick: std::time::Instant::now(),
            spring_amp: 0.0,
            spring_phase: 0.0,
        }
    }
}

/// A single on-screen animation.
pub struct Overlay {
    pub id: OverlayId,
    pub hwnd: HWND,
    pub raw_frames: Arc<FrameSet>,
    pub frames: Arc<FrameSet>,
    pub index: usize,
    pub x: i32,
    pub y: i32,
    pub last_presented_x: i32,
    pub last_presented_y: i32,
    pub scale: f64,
    pub smooth: bool,
    pub flip_x: bool,
    pub physics: PetPhysics,
    pub chroma: ChromaKeyConfig,
    pub screen_h: i32,
    pub alpha: f32,
    pub always_on_top: bool,
    pub click_through: bool,
    pub visible: bool,
    pub timer: usize,
    pub base_interval_ms: u32,
    pub current_interval_ms: u32,
    pub label: String,
    pub next_due: std::time::Instant,

    mem_dc: HDC,
    dib: HBITMAP,
    old_obj: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
    row_bytes: usize,
    bottom_up: bool,
    /// True when no window was created (headless mode).
    virtual_only: bool,
}

impl Overlay {
    pub fn width(&self) -> i32 {
        self.width
    }

    pub fn height(&self) -> i32 {
        self.height
    }

    pub fn update_timer_speed(&mut self) {
        // Ticking is driven centrally by the high-resolution thread timer in the main message loop.
    }

    pub fn set_physics(&mut self, enabled: bool) {
        self.physics.enabled = enabled;
        if enabled {
            self.physics.is_grounded = false;
            self.physics.is_dragging = false;
            self.physics.vx = 0.0;
            self.physics.vy = 0.0;
            self.physics.exact_x = self.x as f64;
            self.physics.exact_y = self.y as f64;
            self.physics.spring_amp = 0.0;
            self.physics.spring_phase = 0.0;
            self.physics.last_tick = std::time::Instant::now();
        } else {
            self.physics.is_grounded = true;
            self.physics.spring_amp = 0.0;
        }
        self.update_timer_speed();
    }

    pub fn set_bouncy(&mut self, bouncy: bool) {
        self.physics.bouncy = bouncy;
        if !bouncy {
            self.physics.spring_amp = 0.0;
        }
    }

    pub fn set_chroma(&mut self, config: ChromaKeyConfig) {
        if self.chroma == config {
            return;
        }
        self.chroma = config;
        self.rebuild_frames();
        self.present();
    }

    pub fn rebuild_frames(&mut self) {
        if self.chroma.enabled {
            self.frames = Arc::new(apply_chroma_key(&self.raw_frames, &self.chroma));
        } else {
            self.frames = self.raw_frames.clone();
        }
    }

    pub fn tick_physics(&mut self, dt: f64) {
        if !self.physics.enabled || self.physics.is_dragging || !self.visible {
            return;
        }

        let work = get_desktop_workarea_for_hwnd(self.hwnd);
        let min_x = work.left;
        let max_x = (work.right - self.width).max(min_x);
        let min_y = work.top;
        let max_y = (work.bottom - self.height).max(min_y);

        if self.physics.is_grounded {
            if self.physics.bouncy && self.physics.spring_amp > 0.3 {
                self.physics.spring_phase += 28.0 * dt;
                self.physics.spring_amp *= (1.0 - 9.0 * dt).max(0.0);
                let bounce_lift = (self.physics.spring_phase.sin().abs()) * self.physics.spring_amp;
                let target_y = (max_y as f64 - bounce_lift).round() as i32;
                if target_y != self.y {
                    self.move_to(self.x, target_y);
                }
            } else {
                self.physics.spring_amp = 0.0;
                if self.y != max_y {
                    self.move_to(self.x, max_y);
                }
                if self.current_interval_ms != self.base_interval_ms {
                    self.update_timer_speed();
                }
            }
            return;
        }

        // Apply gravity: 2200 px/s^2 gives a snappy, responsive desktop pet drop
        self.physics.vy += 2200.0 * dt;
        // Air resistance
        self.physics.vx *= (1.0 - 0.7 * dt).max(0.0);

        self.physics.exact_x += self.physics.vx * dt;
        self.physics.exact_y += self.physics.vy * dt;

        // Bounce on left/right screen edges with multi-monitor pass-through
        let wall_restitution = if self.physics.bouncy { 0.65 } else { 0.20 };
        if self.physics.exact_x < min_x as f64 {
            let has_left_monitor = unsafe {
                let pt = windows::Win32::Foundation::POINT { x: min_x - 10, y: self.y };
                let mon = windows::Win32::Graphics::Gdi::MonitorFromPoint(pt, windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONULL);
                !mon.0.is_null()
            };
            if !has_left_monitor {
                self.physics.exact_x = min_x as f64;
                self.physics.vx = -self.physics.vx * wall_restitution;
                if self.physics.vx > 10.0 {
                    self.flip_x = true;
                }
            }
        } else if self.physics.exact_x > max_x as f64 {
            let has_right_monitor = unsafe {
                let pt = windows::Win32::Foundation::POINT { x: max_x + self.width + 10, y: self.y };
                let mon = windows::Win32::Graphics::Gdi::MonitorFromPoint(pt, windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONULL);
                !mon.0.is_null()
            };
            if !has_right_monitor {
                self.physics.exact_x = max_x as f64;
                self.physics.vx = -self.physics.vx * wall_restitution;
                if self.physics.vx < -10.0 {
                    self.flip_x = false;
                }
            }
        }

        // Ceiling clamp
        if self.physics.exact_y < min_y as f64 {
            self.physics.exact_y = min_y as f64;
            let ceiling_restitution = if self.physics.bouncy { 0.45 } else { 0.10 };
            self.physics.vy = self.physics.vy.abs() * ceiling_restitution;
        }

        // Landing on floor (taskbar top edge)
        if self.physics.exact_y >= max_y as f64 {
            self.physics.exact_y = max_y as f64;
            let impact_speed = self.physics.vy;

            if self.physics.bouncy {
                if impact_speed > 160.0 {
                    // Elastic rebound like a rubber spring
                    self.physics.vy = -impact_speed * 0.60;
                    self.physics.vx *= 0.75;
                    self.physics.spring_amp = (impact_speed * 0.04).clamp(3.0, 20.0);
                    self.physics.spring_phase = 0.0;
                } else {
                    // Settle on taskbar
                    self.physics.vy = 0.0;
                    self.physics.vx = 0.0;
                    self.physics.is_grounded = true;
                    if impact_speed > 30.0 {
                        self.physics.spring_amp = (impact_speed * 0.03).clamp(2.0, 8.0);
                        self.physics.spring_phase = 0.0;
                    }
                }
            } else {
                // Inelastic (bouncy = false)
                if impact_speed > 250.0 {
                    self.physics.vy = -impact_speed * 0.15;
                    self.physics.vx *= 0.4;
                } else {
                    self.physics.vy = 0.0;
                    self.physics.vx = 0.0;
                    self.physics.is_grounded = true;
                    self.physics.spring_amp = 0.0;
                }
            }
        }

        let new_ix = self.physics.exact_x.round() as i32;
        let new_iy = self.physics.exact_y.round() as i32;
        if new_ix != self.x || new_iy != self.y {
            self.x = new_ix;
            self.y = new_iy;
            if !self.virtual_only {
                with_programmatic_update(|| unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        new_ix,
                        new_iy,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                });
            }
        }

        if self.physics.is_grounded && self.physics.spring_amp <= 0.3 {
            self.update_timer_speed();
        }
    }

    /// Advances to the next frame if its delay elapsed, then repaints.
    pub fn tick(&mut self) {
        if !self.visible {
            return;
        }
        if !tick_enabled() {
            return;
        }

        let now = std::time::Instant::now();

        // 1. Physics update tick (if enabled)
        if self.physics.enabled {
            let dt = (now.duration_since(self.physics.last_tick).as_secs_f64()).clamp(0.001, 0.050);
            self.physics.last_tick = now;
            self.tick_physics(dt);
        }

        // 2. Animation presentation tick
        if self.frames.frames.is_empty() {
            return;
        }
        if now < self.next_due {
            return;
        }
        let delay = self.frames.frames[self.index].delay_ms.max(16) as u64;
        self.next_due = now + std::time::Duration::from_millis(delay);
        self.index = (self.index + 1) % self.frames.frames.len();
        self.present();
    }

    /// Blits the current frame into the layered window.
    pub fn present(&mut self) {
        if self.virtual_only || self.bits.is_null() {
            return;
        }
        if self.frames.frames.is_empty() {
            return;
        }
        let frame = &self.frames.frames[self.index];

        let needed = self.row_bytes * self.height as usize;
        unsafe {
            let dst = std::slice::from_raw_parts_mut(self.bits, needed);
            if !blit_enabled() {
                // Skip the copy, but keep the slice alive for the optimiser.
                std::hint::black_box(&dst);
            } else if frame.width as i32 == self.width && frame.height as i32 == self.height {
                if self.bottom_up {
                    for y in 0..self.height as usize {
                        let src = y * self.row_bytes;
                        let out = (self.height as usize - 1 - y) * self.row_bytes;
                        dst[out..out + self.row_bytes]
                            .copy_from_slice(&frame.bgra[src..src + self.row_bytes]);
                    }
                } else {
                    let needed_src = needed.min(frame.bgra.len());
                    dst[..needed_src].copy_from_slice(&frame.bgra[..needed_src]);
                }
            } else if self.smooth {
                crate::media::resample_bilinear_into(
                    &frame.bgra,
                    frame.width,
                    frame.height,
                    dst,
                    self.width as u32,
                    self.height as u32,
                );
            } else {
                crate::media::resample_nearest_into(
                    &frame.bgra,
                    frame.width,
                    frame.height,
                    dst,
                    self.width as u32,
                    self.height as u32,
                );
            }
        }

        if self.flip_x {
            unsafe {
                let dst = std::slice::from_raw_parts_mut(self.bits, needed);
                let w = self.width as usize;
                let mid = w / 2;
                for y in 0..self.height as usize {
                    let row_start = y * self.row_bytes;
                    let row = &mut dst[row_start..row_start + (w * 4)];
                    for x in 0..mid {
                        let l = x * 4;
                        let r = (w - 1 - x) * 4;
                        row.swap(l, r);
                        row.swap(l + 1, r + 1);
                        row.swap(l + 2, r + 2);
                        row.swap(l + 3, r + 3);
                    }
                }
            }
        }

        let size = SIZE { cx: self.width, cy: self.height };
        let pt_src = POINT { x: 0, y: 0 };
        let position_changed = self.x != self.last_presented_x || self.y != self.last_presented_y;
        let pt_dst = POINT { x: self.x, y: self.y };
        let ppt_dst: Option<*const POINT> = if position_changed {
            self.last_presented_x = self.x;
            self.last_presented_y = self.y;
            Some(&pt_dst)
        } else {
            None
        };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: (self.alpha.clamp(0.0, 1.0) * 255.0) as u8,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };

        unsafe {
            FRAMES_PRESENTED.fetch_add(1, Ordering::Relaxed);
            if !presenting_enabled() {
                return;
            }
            let _ = UpdateLayeredWindow(
                self.hwnd,
                HDC::default(),
                ppt_dst,
                Some(&size),
                self.mem_dc,
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    /// Moves the overlay to an absolute screen position.
    pub fn move_to(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
        self.physics.exact_x = x as f64;
        self.physics.exact_y = y as f64;
        if self.virtual_only {
            return;
        }
        with_programmatic_update(|| unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        });
        self.present();
    }

    pub fn set_alpha(&mut self, alpha: f32) {
        self.alpha = alpha.clamp(0.0, 1.0);
        self.present();
    }

    pub fn set_always_on_top(&mut self, value: bool) {
        self.always_on_top = value;
        self.apply_topmost();
    }

    pub fn apply_topmost(&self) {
        if self.virtual_only || !topmost_enabled() {
            return;
        }
        with_programmatic_update(|| unsafe {
            let cur_style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32;
            let target_topmost = self.always_on_top;
            let has_topmost = (cur_style & WS_EX_TOPMOST.0) != 0;

            if target_topmost != has_topmost {
                let new_style = if target_topmost {
                    cur_style | WS_EX_TOPMOST.0
                } else {
                    cur_style & !WS_EX_TOPMOST.0
                };
                let _ = SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, new_style as isize);
            }

            let after = if target_topmost { HWND_TOPMOST } else { HWND_NOTOPMOST };
            let _ = SetWindowPos(
                self.hwnd,
                after,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW,
            );
        });
    }

    pub fn set_click_through(&mut self, value: bool) {
        self.click_through = value;
        self.apply_click_through();
    }

    pub fn apply_click_through(&self) {
        if self.virtual_only {
            return;
        }
        with_programmatic_update(|| unsafe {
            let style = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32;
            let mut style = style;
            style |= WS_EX_LAYERED.0 | WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0;
            if self.always_on_top {
                style |= WS_EX_TOPMOST.0;
            }
            if self.click_through {
                style |= WS_EX_TRANSPARENT.0;
            } else {
                style &= !WS_EX_TRANSPARENT.0;
            }
            let _ = SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, style as isize);
            let _ = SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        });
    }

    pub fn set_visible(&mut self, value: bool) {
        self.visible = value;
        if self.virtual_only {
            return;
        }
        with_programmatic_update(|| unsafe {
            if value {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                );
            } else {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_HIDEWINDOW,
                );
            }
        });
        if value {
            self.apply_topmost();
            self.present();
        }
    }

    pub fn rescale(&mut self, new_scale: f64) {
        let new_scale = new_scale.clamp(0.01, 10.0);
        if self.virtual_only {
            self.scale = new_scale;
            return;
        }
        let (new_w, new_h) = compute_dimensions(
            self.raw_frames.width,
            self.raw_frames.height,
            self.screen_h,
            new_scale,
        );

        if new_w == self.width && new_h == self.height {
            self.scale = new_scale;
            return;
        }

        // Recreate DIB section FIRST. If allocation fails, rollback and keep existing dimensions.
        if !self.recreate_dib(new_w, new_h) {
            return;
        }

        // Keep center in place
        let delta_w = new_w - self.width;
        let delta_h = new_h - self.height;
        self.x -= delta_w / 2;
        self.y -= delta_h / 2;

        self.scale = new_scale;
        self.width = new_w;
        self.height = new_h;
        self.row_bytes = (new_w as usize) * 4;

        with_programmatic_update(|| unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                self.x,
                self.y,
                self.width,
                self.height,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        });
        self.present();
    }

    pub fn set_smooth(&mut self, smooth: bool) {
        if self.smooth == smooth {
            return;
        }
        self.smooth = smooth;
        if self.virtual_only {
            return;
        }
        self.present();
    }

    fn recreate_dib(&mut self, target_w: i32, target_h: i32) -> bool {
        unsafe {
            let bottom_up = dib_orientation() == DibOrientation::BottomUp;
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: target_w,
                    biHeight: if bottom_up { target_h } else { -target_h },
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [windows::Win32::Graphics::Gdi::RGBQUAD::default()],
            };

            let mut bits: *mut c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(
                self.mem_dc,
                &mut bmi as *mut BITMAPINFO,
                DIB_RGB_COLORS,
                &mut bits as *mut *mut c_void,
                None,
                0,
            );
            match dib {
                Ok(new_dib) if !new_dib.0.is_null() => {
                    if !self.old_obj.0.is_null() && !self.mem_dc.is_invalid() {
                        let _ = SelectObject(self.mem_dc, self.old_obj);
                    }
                    if !self.dib.0.is_null() {
                        let _ = DeleteObject(self.dib);
                    }
                    self.dib = new_dib;
                    self.old_obj = SelectObject(self.mem_dc, new_dib);
                    self.bits = bits as *mut u8;
                    true
                }
                _ => false,
            }
        }
    }

    pub fn toggle_flip_x(&mut self) {
        self.flip_x = !self.flip_x;
        self.present();
    }
}

/// Owns every overlay window. Lives on the message-loop thread.
pub struct OverlayManager {
    overlays: HashMap<OverlayId, Overlay>,
    next_id: OverlayId,
    hinstance: HINSTANCE,
    pub broadcaster: Option<crate::ipc::Broadcaster>,
}

impl OverlayManager {
    pub fn new() -> Self {
        let hinstance = unsafe {
            GetModuleHandleW(None)
                .map(|m| HINSTANCE(m.0))
                .unwrap_or(HINSTANCE(std::ptr::null_mut()))
        };
        OverlayManager {
            overlays: HashMap::new(),
            next_id: 1,
            hinstance,
            broadcaster: None,
        }
    }

    pub fn set_broadcaster(&mut self, broadcaster: crate::ipc::Broadcaster) {
        self.broadcaster = Some(broadcaster);
    }

    pub fn len(&self) -> usize {
        self.overlays.len()
    }

    pub fn is_empty(&self) -> bool {
        self.overlays.is_empty()
    }

    pub fn get(&self, id: OverlayId) -> Option<&Overlay> {
        self.overlays.get(&id)
    }

    pub fn get_mut(&mut self, id: OverlayId) -> Option<&mut Overlay> {
        self.overlays.get_mut(&id)
    }

    pub fn ids(&self) -> Vec<OverlayId> {
        self.overlays.keys().copied().collect()
    }

    /// Creates a new overlay window showing the given animation.
    pub fn create(
        &mut self,
        raw_frames: Arc<FrameSet>,
        x: i32,
        y: i32,
        fps: i32,
        scale: f64,
        smooth: bool,
        always_on_top: bool,
        click_through: bool,
        physics: bool,
        bouncy: bool,
        chroma: ChromaKeyConfig,
        label: String,
    ) -> Result<OverlayId, String> {
        ensure_class(self.hinstance)?;
        if raw_frames.frames.is_empty() {
            return Err("animation has no frames".into());
        }

        let active_frames = if chroma.enabled {
            Arc::new(apply_chroma_key(&raw_frames, &chroma))
        } else {
            raw_frames.clone()
        };

        let id = self.next_id;
        self.next_id += 1;

        let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        let (width, height) = compute_dimensions(
            raw_frames.width,
            raw_frames.height,
            screen_h,
            scale,
        );

        let base_interval = (1000 / fps.max(1)).max(1) as u32;

        if headless_enabled() {
            // No window, no DIB: the frames are only decoded and cached.
            let overlay = Overlay {
                id,
                hwnd: HWND::default(),
                raw_frames: raw_frames.clone(),
                frames: active_frames,
                index: 0,
                x,
                y,
                last_presented_x: i32::MIN,
                last_presented_y: i32::MIN,
                scale,
                smooth,
                flip_x: false,
                physics: PetPhysics {
                    enabled: physics,
                    bouncy,
                    exact_x: x as f64,
                    exact_y: y as f64,
                    ..Default::default()
                },
                chroma,
                screen_h,
                alpha: 1.0,
                always_on_top,
                click_through,
                visible: true,
                timer: 0,
                base_interval_ms: base_interval,
                current_interval_ms: base_interval,
                label,
                next_due: std::time::Instant::now(),
                mem_dc: HDC::default(),
                dib: HBITMAP(std::ptr::null_mut()),
                old_obj: HGDIOBJ(std::ptr::null_mut()),
                bits: std::ptr::null_mut(),
                width,
                height,
                row_bytes: (width as usize) * 4,
                bottom_up: false,
                virtual_only: true,
            };
            self.overlays.insert(id, overlay);
            return Ok(id);
        }

        let mut ex_style: WINDOW_EX_STYLE =
            WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        if always_on_top {
            ex_style |= WS_EX_TOPMOST;
        }
        if click_through {
            ex_style |= WS_EX_TRANSPARENT;
        }

        let class_name = w!("AnimaCoreOverlay");
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(class_name.as_ptr()),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                width,
                height,
                None,
                None,
                self.hinstance,
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW failed: {e}"))?;

        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, id as isize);
        }
        register_id(hwnd.0 as isize, id);

        // Create the DIB used as the UpdateLayeredWindow source.
        let row_bytes = (width as usize) * 4;
        let bottom_up = dib_orientation() == DibOrientation::BottomUp;
        let mem_dc = unsafe { CreateCompatibleDC(HDC::default()) };
        if mem_dc.is_invalid() {
            return Err("CreateCompatibleDC failed".into());
        }

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // A negative height means top-down rows, matching our frame
                // layout; a positive height means the classic bottom-up order.
                biHeight: if bottom_up { height } else { -height },
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [Default::default(); 1],
        };

        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = unsafe {
            CreateDIBSection(
                mem_dc,
                &mut bmi as *mut BITMAPINFO,
                DIB_RGB_COLORS,
                &mut bits as *mut *mut c_void,
                None,
                0,
            )
        }
        .map_err(|e| format!("CreateDIBSection: {e}"))?;

        let old_obj = unsafe { SelectObject(mem_dc, dib) };

        let overlay = Overlay {
            id,
            hwnd,
            raw_frames: raw_frames.clone(),
            frames: active_frames,
            index: 0,
            x,
            y,
            last_presented_x: i32::MIN,
            last_presented_y: i32::MIN,
            scale,
            smooth,
            flip_x: false,
            physics: PetPhysics {
                enabled: physics,
                bouncy,
                exact_x: x as f64,
                exact_y: y as f64,
                ..Default::default()
            },
            chroma,
            screen_h,
            alpha: 1.0,
            always_on_top,
            click_through,
            visible: true,
            timer: 0,
            base_interval_ms: base_interval,
            current_interval_ms: base_interval,
            label,
            next_due: std::time::Instant::now(),
            mem_dc,
            dib,
            old_obj,
            bits: bits as *mut u8,
            width,
            height,
            row_bytes,
            bottom_up,
            virtual_only: false,
        };

        self.overlays.insert(id, overlay);
        if let Some(overlay) = self.overlays.get_mut(&id) {
            overlay.next_due = std::time::Instant::now();
            overlay.present();
            overlay.set_visible(true);
            overlay.apply_topmost();
            if overlay.physics.enabled {
                overlay.update_timer_speed();
            }
        }
        Ok(id)
    }

    /// Destroys an overlay window and releases its resources.
    pub fn destroy(&mut self, id: OverlayId) {
        if let Some(overlay) = self.overlays.remove(&id) {
            if overlay.virtual_only {
                return;
            }
            unsafe {
                let _ = SetWindowLongPtrW(overlay.hwnd, GWLP_USERDATA, 0);
                unregister_id(overlay.hwnd.0 as isize);
                let _ = DestroyWindow(overlay.hwnd);
                let _ = SelectObject(overlay.mem_dc, overlay.old_obj);
                let _ = DeleteObject(overlay.dib);
                let _ = DeleteDC(overlay.mem_dc);
            }
            if let Some(ref bc) = self.broadcaster {
                bc.broadcast(crate::proto::Event::new(
                    "overlay.destroyed",
                    serde_json::json!({ "overlay_id": id }),
                ));
            }
        }
    }

    pub fn destroy_all(&mut self) {
        for id in self.ids() {
            self.destroy(id);
        }
    }

    /// Advances every overlay. Called from the message loop.
    pub fn tick_all(&mut self) {
        for overlay in self.overlays.values_mut() {
            overlay.tick();
        }
    }

    /// Re-asserts the always-on-top flag; other windows can steal the
    /// top-most position, so this runs on a slow watchdog.
    pub fn enforce_topmost(&mut self) {
        for overlay in self.overlays.values() {
            if overlay.always_on_top {
                overlay.apply_topmost();
            }
        }
    }
}

impl Default for OverlayManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Registers the overlay window class once per process.
fn ensure_class(hinstance: HINSTANCE) -> Result<(), String> {
    if CLASS_ATOM.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    let class_name = w!("AnimaCoreOverlay");
    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(overlay_wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance,
        hIcon: HICON::default(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW).unwrap_or_default() },
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
    };
    let atom = unsafe { RegisterClassW(&wc) };
    if atom == 0 {
        return Err("RegisterClassW failed".into());
    }
    CLASS_ATOM.store(atom, Ordering::Release);
    Ok(())
}

const IDM_HEADER: usize = 0;
const IDM_SCALE_10: usize = 101;
const IDM_SCALE_15: usize = 102;
const IDM_SCALE_20: usize = 103;
const IDM_SCALE_30: usize = 104;
const IDM_SCALE_50: usize = 105;
const IDM_SCALE_100: usize = 106;

const IDM_ALPHA_100: usize = 201;
const IDM_ALPHA_85: usize = 202;
const IDM_ALPHA_70: usize = 203;
const IDM_ALPHA_50: usize = 204;

const IDM_TOGGLE_TOPMOST: usize = 301;
const IDM_TOGGLE_FLIP: usize = 302;
const IDM_TOGGLE_CLICKTHROUGH: usize = 303;
const IDM_TOGGLE_PHYSICS: usize = 304;
const IDM_TOGGLE_BOUNCY: usize = 305;

const IDM_HIDE: usize = 401;
const IDM_CLOSE: usize = 402;

const IDM_OPEN_SETTINGS: usize = 501;

unsafe fn add_menu_item(hmenu: HMENU, id: usize, text: &str, flags: MENU_ITEM_FLAGS) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(hmenu, flags, id, PCWSTR(wide.as_ptr()));
}

unsafe fn add_separator(hmenu: HMENU) {
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
}

unsafe fn add_submenu(hmenu: HMENU, sub: HMENU, text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = AppendMenuW(hmenu, MF_POPUP, sub.0 as usize, PCWSTR(wide.as_ptr()));
}

unsafe fn add_menu_check(hmenu: HMENU, id: usize, text: &str, checked: bool) {
    let flag = if checked { MF_CHECKED } else { MF_UNCHECKED };
    add_menu_item(hmenu, id, text, MF_STRING | flag);
}

unsafe fn add_menu_radio(hmenu: HMENU, id: usize, text: &str, checked: bool) {
    let flag = if checked { MF_CHECKED } else { MF_UNCHECKED };
    add_menu_item(hmenu, id, text, MF_STRING | flag);
}

fn open_ui_settings_window() {
    unsafe {
        let msg_name = windows::core::w!("OpenAnima_ShowWindow");
        let msg = RegisterWindowMessageW(PCWSTR(msg_name.as_ptr()));
        if msg != 0 {
            let _ = PostMessageW(HWND(0xFFFF as _), msg, WPARAM(0), LPARAM(0));
        }

        let titles = [
            windows::core::w!("OpenAnima — Легковесный движок оверлеев"),
            windows::core::w!("OpenAnima — Lightweight Desktop Overlay Engine"),
        ];

        for title in &titles {
            if let Ok(wnd) = FindWindowW(None, PCWSTR(title.as_ptr())) {
                if (wnd.0 as usize) != 0 {
                    let _ = ShowWindow(wnd, SW_RESTORE);
                    let _ = SetForegroundWindow(wnd);
                    return;
                }
            }
        }

        if let Ok(mut exe_path) = std::env::current_exe() {
            exe_path.set_file_name("OpenAnima.UI.exe");
            if exe_path.exists() {
                let _ = std::process::Command::new(exe_path).spawn();
            }
        }
    }
}

unsafe fn show_overlay_context_menu(hwnd: HWND, pt: POINT) {
    let id = match lookup_id(hwnd.0 as isize) {
        Some(id) => id,
        None => return,
    };
    let manager = MANAGER.load(Ordering::Acquire);
    if manager.is_null() {
        return;
    }

    let (label, scale, alpha, always_on_top, click_through, flip_x, physics_enabled, bouncy_enabled) = {
        let mgr = &mut *manager;
        let overlay = match mgr.overlays.get_mut(&id) {
            Some(o) => o,
            None => return,
        };
        (
            overlay.label.clone(),
            overlay.scale,
            overlay.alpha,
            overlay.always_on_top,
            overlay.click_through,
            overlay.flip_x,
            overlay.physics.enabled,
            overlay.physics.bouncy,
        )
    };

    let ru = is_russian_locale();

    let hmenu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    let current_pct = (scale * 100.0).round() as i32;
    let current_alpha_pct = (alpha * 100.0).round() as i32;

    // Submenu: Scale
    let scale_menu = CreatePopupMenu().unwrap_or_default();
    add_menu_radio(scale_menu, IDM_SCALE_10, if ru { "10% экрана (Мини)" } else { "10% screen (Mini)" }, current_pct == 10);
    add_menu_radio(scale_menu, IDM_SCALE_15, if ru { "15% экрана" } else { "15% screen" }, current_pct == 15);
    add_menu_radio(scale_menu, IDM_SCALE_20, if ru { "20% экрана (Стандарт)" } else { "20% screen (Standard)" }, current_pct == 20);
    add_menu_radio(scale_menu, IDM_SCALE_30, if ru { "30% экрана" } else { "30% screen" }, current_pct == 30);
    add_menu_radio(scale_menu, IDM_SCALE_50, if ru { "50% экрана (Большой)" } else { "50% screen (Large)" }, current_pct == 50);
    add_menu_radio(scale_menu, IDM_SCALE_100, if ru { "100% экрана (Во весь экран)" } else { "100% screen (Full size)" }, current_pct == 100);

    // Submenu: Alpha
    let alpha_menu = CreatePopupMenu().unwrap_or_default();
    add_menu_radio(alpha_menu, IDM_ALPHA_100, if ru { "100% (Непрозрачно)" } else { "100% (Opaque)" }, current_alpha_pct >= 95);
    add_menu_radio(alpha_menu, IDM_ALPHA_85, "85%", (80..95).contains(&current_alpha_pct));
    add_menu_radio(alpha_menu, IDM_ALPHA_70, "70%", (60..80).contains(&current_alpha_pct));
    add_menu_radio(alpha_menu, IDM_ALPHA_50, if ru { "50% (Полупрозрачно)" } else { "50% (Semi-transparent)" }, current_alpha_pct < 60);

    // Header with animation name
    let filename = std::path::Path::new(&label)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&label);
    let header_text = format!("🐱 {}", filename);
    add_menu_item(hmenu, IDM_HEADER, &header_text, MF_DISABLED | MF_GRAYED);
    add_separator(hmenu);

    // Submenus
    let scale_title = if ru {
        format!("Масштаб (сейчас {}%)", current_pct)
    } else {
        format!("Scale (currently {}%)", current_pct)
    };
    add_submenu(hmenu, scale_menu, &scale_title);

    let alpha_title = if ru {
        format!("Прозрачность (сейчас {}%)", current_alpha_pct)
    } else {
        format!("Opacity (currently {}%)", current_alpha_pct)
    };
    add_submenu(hmenu, alpha_menu, &alpha_title);
    add_separator(hmenu);

    // Toggles
    add_menu_check(hmenu, IDM_TOGGLE_TOPMOST, if ru { "Поверх всех окон" } else { "Always on top" }, always_on_top);
    add_menu_check(hmenu, IDM_TOGGLE_FLIP, if ru { "Отразить зеркально (По горизонтали)" } else { "Flip horizontally" }, flip_x);
    add_menu_check(hmenu, IDM_TOGGLE_CLICKTHROUGH, if ru { "Клик насквозь (Click-through)" } else { "Click-through" }, click_through);
    add_menu_check(hmenu, IDM_TOGGLE_PHYSICS, if ru { "⚡ Физика и гравитация (по умолчанию выкл)" } else { "⚡ Physics & gravity (off by default)" }, physics_enabled);
    if physics_enabled {
        add_menu_check(hmenu, IDM_TOGGLE_BOUNCY, if ru { "🌀 Пружинящий отскок (Spring bounce)" } else { "🌀 Spring bounce" }, bouncy_enabled);
    }
    add_separator(hmenu);

    // Actions
    add_menu_item(hmenu, IDM_HIDE, if ru { "Скрыть персонажа" } else { "Hide character" }, MF_STRING);
    add_menu_item(hmenu, IDM_CLOSE, if ru { "Закрыть персонажа" } else { "Close character" }, MF_STRING);
    add_separator(hmenu);

    // App Control
    add_menu_item(hmenu, IDM_OPEN_SETTINGS, if ru { "⚙️ Панель управления OpenAnima" } else { "⚙️ OpenAnima Control Panel" }, MF_STRING);

    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        hmenu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(hmenu);

    let choice = cmd.0 as usize;
    if choice == 0 {
        return;
    }

    let mgr = &mut *manager;
    match choice {
        IDM_SCALE_10 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.rescale(0.10); } }
        IDM_SCALE_15 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.rescale(0.15); } }
        IDM_SCALE_20 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.rescale(0.20); } }
        IDM_SCALE_30 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.rescale(0.30); } }
        IDM_SCALE_50 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.rescale(0.50); } }
        IDM_SCALE_100 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.rescale(1.00); } }
        IDM_ALPHA_100 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.set_alpha(1.0); } }
        IDM_ALPHA_85 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.set_alpha(0.85); } }
        IDM_ALPHA_70 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.set_alpha(0.70); } }
        IDM_ALPHA_50 => { if let Some(o) = mgr.overlays.get_mut(&id) { o.set_alpha(0.50); } }
        IDM_TOGGLE_TOPMOST => {
            if let Some(o) = mgr.overlays.get_mut(&id) {
                let cur = o.always_on_top;
                o.set_always_on_top(!cur);
            }
        }
        IDM_TOGGLE_FLIP => {
            if let Some(o) = mgr.overlays.get_mut(&id) {
                o.toggle_flip_x();
            }
        }
        IDM_TOGGLE_CLICKTHROUGH => {
            if let Some(o) = mgr.overlays.get_mut(&id) {
                let cur = o.click_through;
                o.set_click_through(!cur);
            }
        }
        IDM_TOGGLE_PHYSICS => {
            if let Some(o) = mgr.overlays.get_mut(&id) {
                let cur = o.physics.enabled;
                o.set_physics(!cur);
            }
        }
        IDM_TOGGLE_BOUNCY => {
            if let Some(o) = mgr.overlays.get_mut(&id) {
                let cur = o.physics.bouncy;
                o.set_bouncy(!cur);
            }
        }
        IDM_HIDE => {
            if let Some(o) = mgr.overlays.get_mut(&id) {
                o.set_visible(false);
            }
        }
        IDM_CLOSE => {
            mgr.destroy(id);
        }
        IDM_OPEN_SETTINGS => {
            open_ui_settings_window();
        }
        _ => {}
    }
}

/// Window procedure for overlay windows.
unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCHITTEST {
        // Return HTCAPTION (2) so left-clicking anywhere on the visible
        // animation starts dragging it smoothly across the desktop.
        return LRESULT(2);
    }
    if message == WM_NCRBUTTONDOWN || message == WM_RBUTTONDOWN {
        return LRESULT(0);
    }
    if message == WM_NCRBUTTONUP || message == WM_RBUTTONUP || message == WM_CONTEXTMENU {
        let mut pt = POINT::default();
        let _ = unsafe { GetCursorPos(&mut pt) };
        unsafe {
            show_overlay_context_menu(hwnd, pt);
        }
        return LRESULT(0);
    }
    if message == WM_ENTERSIZEMOVE {
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
            let id = lookup_id(hwnd.0 as isize);
            if let Some(id) = id {
                let manager = MANAGER.load(Ordering::Acquire);
                if !manager.is_null() {
                    if let Some(overlay) = (*manager).overlays.get_mut(&id) {
                        overlay.physics.is_dragging = true;
                        overlay.physics.is_grounded = false;
                        overlay.physics.last_drag_x = rect.left;
                        overlay.physics.last_drag_y = rect.top;
                        overlay.physics.last_drag_time = std::time::Instant::now();
                        overlay.physics.vx = 0.0;
                        overlay.physics.vy = 0.0;
                        overlay.physics.exact_x = rect.left as f64;
                        overlay.physics.exact_y = rect.top as f64;
                        overlay.physics.spring_amp = 0.0;
                        overlay.physics.spring_phase = 0.0;
                        overlay.update_timer_speed();
                    }
                }
            }
        }
        return LRESULT(0);
    }
    if message == WM_MOVE {
        if IN_PROGRAMMATIC_MOVE.with(|flag| flag.get()) {
            return LRESULT(0);
        }
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
            let id = lookup_id(hwnd.0 as isize);
            if let Some(id) = id {
                let manager = MANAGER.load(Ordering::Acquire);
                if !manager.is_null() {
                    if let Some(overlay) = (*manager).overlays.get_mut(&id) {
                        if overlay.physics.is_dragging {
                            let now = std::time::Instant::now();
                            let dt = now.duration_since(overlay.physics.last_drag_time).as_secs_f64();
                            // For high-polling mice (500Hz-1000Hz), accumulate if dt < 0.008s (~125Hz)
                            if dt >= 0.008 {
                                if dt < 0.25 {
                                    let inst_vx = (rect.left - overlay.physics.last_drag_x) as f64 / dt;
                                    let inst_vy = (rect.top - overlay.physics.last_drag_y) as f64 / dt;
                                    overlay.physics.vx = overlay.physics.vx * 0.35 + inst_vx * 0.65;
                                    overlay.physics.vy = overlay.physics.vy * 0.35 + inst_vy * 0.65;
                                }
                                overlay.physics.last_drag_x = rect.left;
                                overlay.physics.last_drag_y = rect.top;
                                overlay.physics.last_drag_time = now;
                            }
                            overlay.physics.is_grounded = false;
                        }

                        overlay.x = rect.left;
                        overlay.y = rect.top;
                        overlay.physics.exact_x = rect.left as f64;
                        overlay.physics.exact_y = rect.top as f64;
                    }
                }
            }
        }
        return LRESULT(0);
    }
    if message == WM_EXITSIZEMOVE {
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_ok() {
            let id = lookup_id(hwnd.0 as isize);
            if let Some(id) = id {
                let manager = MANAGER.load(Ordering::Acquire);
                if !manager.is_null() {
                    if let Some(overlay) = (*manager).overlays.get_mut(&id) {
                        overlay.x = rect.left;
                        overlay.y = rect.top;
                        overlay.physics.exact_x = rect.left as f64;
                        overlay.physics.exact_y = rect.top as f64;
                        overlay.apply_topmost();

                        overlay.physics.is_dragging = false;
                        overlay.physics.last_tick = std::time::Instant::now();

                        let now = std::time::Instant::now();
                        let dt_since_last_move = now.duration_since(overlay.physics.last_drag_time).as_secs_f64();
                        if dt_since_last_move > 0.12 {
                            // User paused mouse before releasing -> gentle drop
                            overlay.physics.vx = 0.0;
                            overlay.physics.vy = 0.0;
                        } else {
                            // Toss velocity with dynamic impulse multiplier
                            overlay.physics.vx = (overlay.physics.vx * 1.25).clamp(-3200.0, 3200.0);
                            overlay.physics.vy = (overlay.physics.vy * 1.25).clamp(-3200.0, 3200.0);
                        }

                        if !overlay.physics.enabled {
                            overlay.physics.is_grounded = true;
                            overlay.physics.vx = 0.0;
                            overlay.physics.vy = 0.0;
                        } else {
                            let work = get_desktop_workarea_for_hwnd(overlay.hwnd);
                            let max_y = (work.bottom - overlay.height).max(work.top);
                            if overlay.y >= max_y && overlay.physics.vy.abs() < 100.0 {
                                overlay.y = max_y;
                                overlay.physics.is_grounded = true;
                                overlay.physics.vx = 0.0;
                                overlay.physics.vy = 0.0;
                            } else {
                                overlay.physics.is_grounded = false;
                            }
                        }
                        overlay.update_timer_speed();
                    }
                }
            }
        }
        return LRESULT(0);
    }
    if message == WM_MOUSEWHEEL {
        let keys = (wparam.0 & 0xFFFF) as u16;
        let delta = ((wparam.0 >> 16) as i16) as i32;
        // Check if Ctrl key is pressed (MK_CONTROL = 0x0008)
        if (keys & 0x0008) != 0 {
            let id = lookup_id(hwnd.0 as isize);
            if let Some(id) = id {
                let manager = MANAGER.load(Ordering::Acquire);
                if !manager.is_null() {
                    if let Some(overlay) = (*manager).overlays.get_mut(&id) {
                        let step = if delta > 0 { 0.02 } else { -0.02 };
                        let new_scale = (overlay.scale + step).clamp(0.01, 10.0);
                        overlay.rescale(new_scale);
                    }
                }
            }
            return LRESULT(0);
        }
    }
    if message == WM_DESTROY {
        unregister_id(hwnd.0 as isize);
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}
