//! AnimaCore â€” the native runtime for AnimaEngine.
//!
//! One process hosts every overlay window, decodes the animations, renders
//! them with per-pixel alpha and (optionally) runs the trigger engine. The
//! Python shell drives it over a named pipe.
//!
//! Usage:
//!   anima-core                      start the service
//!   anima-core --version            print the version
//!   anima-core --selftest FILE      decode FILE and print its properties
//!   anima-core --selftest-dir DIR   decode every animation in DIR

mod app;
mod ipc;
mod media;
mod proto;
mod trigger;
mod win;

use std::sync::Arc;

use windows::Win32::Foundation::{HANDLE, HWND, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MsgWaitForMultipleObjects, PeekMessageW, PostQuitMessage, SetTimer,
    TranslateMessage, MSG, PM_REMOVE, QS_ALLINPUT, WM_QUIT, WM_TIMER,
};

use app::Core;
use win::overlay::OverlayManager;

/// Timer ids (timers with a null window post to the thread queue).
const TIMER_COMMAND: usize = 1;
const TIMER_TRIGGER: usize = 2;
const TIMER_TOPMOST: usize = 3;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("anima-core {}", env!("CARGO_PKG_VERSION"));
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

    unsafe {
        // Null-window timers post WM_TIMER to this thread's queue.
        SetTimer(HWND::default(), TIMER_COMMAND, 10, None);
        SetTimer(HWND::default(), TIMER_TRIGGER, 60, None);
        SetTimer(HWND::default(), TIMER_TOPMOST, 500, None);
    }

    let mut msg = MSG::default();
    let mut last_trigger_tick = 0u64;

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

        // Pump window messages (this is also where the timers arrive).
        loop {
            let has_message = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };
            if !has_message.as_bool() {
                break;
            }
            if msg.message == WM_QUIT {
                break;
            }
            if msg.message == WM_TIMER {
                match msg.wParam.0 {
                    TIMER_COMMAND => drain_commands(&mut core, &server),
                    TIMER_TRIGGER => {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0);
                        if now.saturating_sub(last_trigger_tick) >= 60 {
                            last_trigger_tick = now;
                            tick_triggers(&mut core, &server);
                        }
                    }
                    TIMER_TOPMOST => core.overlays.enforce_topmost(),
                    _ => {}
                }
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
