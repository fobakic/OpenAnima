//! OpenAnima Core — the native high-performance runtime for OpenAnima.
//!
//! One process hosts every overlay window, decodes animations with zero-copy,
//! and renders them with per-pixel alpha over games and desktop via Win32.
//! The C# WPF-UI drives it over a local secure named pipe.
//!
//! Usage:
//!   openanima-core                  start the service
//!   openanima-core --version        print the version
//!   openanima-core --selftest FILE  decode FILE and print its properties

mod app;
mod ipc;
mod media;
mod proto;
#[cfg(feature = "triggers")]
mod trigger;
mod win;

use std::sync::Arc;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WAIT_FAILED, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowLongPtrW,
    KillTimer, MsgWaitForMultipleObjects, PeekMessageW, PostQuitMessage, RegisterClassExW,
    SetTimer, SetWindowLongPtrW, TranslateMessage, GWLP_USERDATA, HMENU, HWND_MESSAGE,
    MSG, PM_REMOVE, QS_ALLINPUT, WINDOW_EX_STYLE, WM_QUIT, WM_TIMER, WNDCLASSEXW,
    WS_OVERLAPPED,
};

use app::Core;
use win::overlay::OverlayManager;

/// Timer ids (timers set on the hidden message-only window).
const TIMER_COMMAND: usize = 1;
#[cfg(feature = "triggers")]
const TIMER_TRIGGER: usize = 2;
const TIMER_TOPMOST: usize = 3;

struct MsgLoopContext {
    core: *mut Core,
    server: Arc<ipc::IpcServer>,
    #[cfg(feature = "triggers")]
    last_trigger_tick: std::sync::atomic::AtomicU64,
}

unsafe extern "system" fn msg_wnd_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_TIMER {
        let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut MsgLoopContext;
        if !ptr.is_null() {
            let ctx = &mut *ptr;
            match wparam.0 {
                TIMER_COMMAND => {
                    drain_commands(&mut (*ctx.core), &ctx.server);
                    (*ctx.core).overlays.tick_all();
                }
                #[cfg(feature = "triggers")]
                TIMER_TRIGGER => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    let last = ctx.last_trigger_tick.load(std::sync::atomic::Ordering::Relaxed);
                    if now.saturating_sub(last) >= 60 {
                        ctx.last_trigger_tick.store(now, std::sync::atomic::Ordering::Relaxed);
                        tick_triggers(&mut (*ctx.core), &ctx.server);
                    }
                }
                TIMER_TOPMOST => {
                    (*ctx.core).overlays.enforce_topmost();
                }
                _ => {}
            }
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

fn create_message_window(ctx_ptr: *mut MsgLoopContext) -> Result<HWND, String> {
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None)
            .map_err(|e| format!("GetModuleHandleW failed: {e}"))?
            .into();

        let class_name = w!("OpenAnimaMsgWindowClass");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: Default::default(),
            lpfnWndProc: Some(msg_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: Default::default(),
            hCursor: Default::default(),
            hbrBackground: Default::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: class_name,
            hIconSm: Default::default(),
        };

        let atom = RegisterClassExW(&wc);
        if atom == 0 {
            let err = windows::Win32::Foundation::GetLastError();
            if err.0 != 1410 {
                return Err(format!("RegisterClassExW failed: {err:?}"));
            }
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("OpenAnimaMsgWindow"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            HMENU::default(),
            hinstance,
            None,
        )
        .map_err(|e| format!("CreateWindowExW (HWND_MESSAGE) failed: {e}"))?;

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx_ptr as isize);
        Ok(hwnd)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("openanima-core {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if let Some(index) = args.iter().position(|a| a == "--selftest") {
        let Some(path) = args.get(index + 1) else {
            eprintln!("--selftest needs a file path");
            std::process::exit(2);
        };
        match selftest_file(std::path::Path::new(path)) {
            Ok(json) => println!("{json}"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if let Some(index) = args.iter().position(|a| a == "--selftest-dir") {
        let Some(dir) = args.get(index + 1) else {
            eprintln!("--selftest-dir needs a directory");
            std::process::exit(2);
        };
        let mut results = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ext != "gif" && ext != "png" && ext != "apng" {
                    continue;
                }
                let line = match selftest_file(&path) {
                    Ok(json) => json,
                    Err(e) => format!("{{\"file\":\"{}\",\"error\":\"{}\"}}", path.display(), e),
                };
                results.push(line);
            }
        }
        for line in results {
            println!("{line}");
        }
        return;
    }

    if let Err(e) = run() {
        eprintln!("anima-core failed to start: {e}");
        std::process::exit(1);
    }
}

/// Decodes a file and prints a JSON description of it.
fn selftest_file(path: &std::path::Path) -> Result<String, String> {
    let started = std::time::Instant::now();
    let frames = media::decode_file(path)?;
    let elapsed = started.elapsed();
    let delays: Vec<u32> = frames.frames.iter().map(|f| f.delay_ms).collect();
    Ok(serde_json::json!({
        "file": path.display().to_string(),
        "width": frames.width,
        "height": frames.height,
        "frames": frames.frames.len(),
        "bytes": frames.bytes(),
        "decode_ms": elapsed.as_millis() as u64,
        "delays_ms": delays,
    })
    .to_string())
}

fn run() -> Result<(), String> {
    // The overlay manager must outlive every window, and its address has to
    // stay stable because the window proc looks it up through a raw pointer.
    let mut core = Box::new(Core::new()?);
    let overlays_ptr: *mut OverlayManager = &mut core.overlays;
    unsafe { win::overlay::set_manager(overlays_ptr) };

    let server = ipc::IpcServer::start().map_err(|e| format!("ipc server: {e}"))?;
    println!(
        "anima-core {} listening on {} (pid {})",
        env!("CARGO_PKG_VERSION"),
        proto::PIPE_NAME,
        std::process::id()
    );

    let mut ctx = Box::new(MsgLoopContext {
        core: &mut *core,
        server: server.clone(),
        #[cfg(feature = "triggers")]
        last_trigger_tick: std::sync::atomic::AtomicU64::new(0),
    });

    let msg_hwnd = create_message_window(&mut *ctx)?;

    unsafe {
        // Window-bound timers post to msg_hwnd and are delivered to msg_wnd_proc
        // even during modal message pumps (TrackPopupMenu, caption-drag, etc.).
        SetTimer(msg_hwnd, TIMER_COMMAND, 10, None);
        #[cfg(feature = "triggers")]
        SetTimer(msg_hwnd, TIMER_TRIGGER, 60, None);
        SetTimer(msg_hwnd, TIMER_TOPMOST, 500, None);
    }

    let mut msg = MSG::default();

    loop {
        // Sleep until either a window message arrives or an IPC command was
        // queued. The 15 ms timeout provides a smooth ~60 Hz physics and animation loop.
        let handles: [HANDLE; 1] = [server.wakeup_handle()];
        let result = unsafe {
            MsgWaitForMultipleObjects(Some(&handles), false, 15, QS_ALLINPUT)
        };
        if result.0 == WAIT_OBJECT_0.0 {
            server.reset();
            drain_commands(&mut core, &server);
        } else if result.0 == WAIT_FAILED.0 {
            // A failure here is not fatal; fall back to a short sleep.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Pump window messages (dispatches WM_TIMER to msg_wnd_proc and window messages to overlay_wnd_proc).
        loop {
            let has_message = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };
            if !has_message.as_bool() {
                break;
            }
            if msg.message == WM_QUIT {
                core.quit = true;
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Update physics and animations across all active overlays
        core.overlays.tick_all();

        if core.quit {
            break;
        }
    }

    unsafe {
        let _ = KillTimer(msg_hwnd, TIMER_COMMAND);
        #[cfg(feature = "triggers")]
        let _ = KillTimer(msg_hwnd, TIMER_TRIGGER);
        let _ = KillTimer(msg_hwnd, TIMER_TOPMOST);
        let _ = DestroyWindow(msg_hwnd);
    }

    core.overlays.destroy_all();
    unsafe {
        PostQuitMessage(0);
    }
    Ok(())
}

/// Answers every pending IPC request on the message-loop thread.
fn drain_commands(core: &mut Core, server: &Arc<ipc::IpcServer>) {
    while let Ok(command) = server.commands.try_recv() {
        match command {
            ipc::Command::Request { req, reply } => {
                let response = core.dispatch(&req);
                let _ = reply.send(response);
            }
            ipc::Command::Broadcast(event) => {
                server.broadcast(event);
            }
        }
    }
}

/// Advances the trigger engine and notifies clients about fired triggers.
#[cfg(feature = "triggers")]
fn tick_triggers(core: &mut Core, server: &Arc<ipc::IpcServer>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let outcomes = core.triggers.tick(now);
    for outcome in outcomes {
        if outcome.fired {
            server.broadcast(proto::Event::new(
                "trigger.fired",
                serde_json::json!({
                    "id": outcome.trigger_id,
                    "score": outcome.score,
                }),
            ));
        }
    }
}
