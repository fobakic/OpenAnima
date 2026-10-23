//! Command dispatcher: turns IPC requests into actions on the core state.
//!
//! Every command runs on the message-loop thread, so overlay windows are
//! always touched from the thread that owns them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde_json::{json, Value};

use crate::media::{decode_file, ChromaKeyConfig, FrameSet};
use crate::proto::{Request, Response, PROTOCOL_VERSION};
#[cfg(feature = "triggers")]
use crate::trigger::engine::TriggerEngine;
use crate::win::overlay::{OverlayId, OverlayManager};

/// The whole core state.
pub struct Core {
    pub overlays: OverlayManager,
    #[cfg(feature = "triggers")]
    pub triggers: TriggerEngine,
    pub started_at: Instant,
    /// Decoded animations, keyed by absolute path.
    pub media_cache: HashMap<PathBuf, Arc<FrameSet>>,
    pub frames_decoded: u64,
    pub commands_handled: u64,
    pub quit: bool,
}

impl Core {
    pub fn new() -> Result<Self, String> {
        Ok(Core {
            overlays: OverlayManager::new(),
            #[cfg(feature = "triggers")]
            triggers: TriggerEngine::new()?,
            started_at: Instant::now(),
            media_cache: HashMap::new(),
            frames_decoded: 0,
            commands_handled: 0,
            quit: false,
        })
    }

    /// Decodes an animation, reusing the cached result when possible.
    pub fn load_media(&mut self, path: &Path) -> Result<Arc<FrameSet>, String> {
        let key = normalize(path);
        if let Some(cached) = self.media_cache.get(&key) {
            return Ok(cached.clone());
        }
        let frames = Arc::new(decode_file(path)?);
        self.frames_decoded += frames.frames.len() as u64;
        self.media_cache.insert(key, frames.clone());
        Ok(frames)
    }

    /// Prunes cached animations that are no longer referenced by any active overlay.
    pub fn prune_unused_media(&mut self) {
        let before = self.media_cache.len();
        self.media_cache.retain(|_, frames| Arc::strong_count(frames) > 1);
        let after = self.media_cache.len();
        if before != after {
            trim_working_set();
        }
    }

    /// Handles one request and produces the reply.
    pub fn dispatch(&mut self, req: &Request) -> Response {
        self.commands_handled += 1;
        let params = &req.params;

        match req.cmd.as_str() {
            "core.hello" => Response::ok(
                req.id,
                json!({
                    "protocol": PROTOCOL_VERSION,
                    "pid": std::process::id(),
                    "capabilities": [
                        "overlay.play", "overlay.stop", "overlay.list", "overlay.set",
                        "media.info", "trigger.load", "trigger.start", "trigger.stop",
                        "trigger.status", "core.stats"
                    ],
                    "engine": "ahash+dhash",
                }),
            ),

            "core.ping" => Response::ok(
                req.id,
                json!({ "pong": true, "uptime_ms": self.started_at.elapsed().as_millis() as u64 }),
            ),

            "core.stats" => Response::ok(req.id, self.stats()),

            "core.quit" => {
                self.quit = true;
                Response::ok(req.id, json!({ "quitting": true }))
            }

            "media.info" => {
                let path = params.get("path").and_then(Value::as_str).unwrap_or_default();
                if path.is_empty() {
                    return Response::err(req.id, "missing 'path'");
                }
                match self.load_media(Path::new(path)) {
                    Ok(frames) => Response::ok(
                        req.id,
                        json!({
                            "path": path,
                            "width": frames.width,
                            "height": frames.height,
                            "frames": frames.frames.len(),
                            "bytes": frames.bytes(),
                            "delays_ms": frames.frames.iter().map(|f| f.delay_ms).collect::<Vec<_>>(),
                        }),
                    ),
                    Err(e) => Response::err(req.id, e),
                }
            }

            "overlay.play" => self.overlay_play(req),
            "overlay.stop" => {
                let res = self.overlay_stop(req);
                self.prune_unused_media();
                res
            }
            "overlay.stop_all" => {
                self.overlays.destroy_all();
                self.prune_unused_media();
                Response::ok(req.id, json!({ "remaining": self.overlays.len() }))
            }
            "core.trim_memory" => {
                self.prune_unused_media();
                trim_working_set();
                Response::ok(req.id, self.stats())
            }
            "overlay.list" => Response::ok(req.id, json!({ "overlays": self.overlay_list() })),
            "overlay.set" => self.overlay_set(req),
            "overlay.hide_all" => {
                for id in self.overlays.ids() {
                    if let Some(o) = self.overlays.get_mut(id) {
                        o.set_visible(false);
                    }
                }
                Response::ok(req.id, json!({ "hidden": true }))
            }
            "overlay.show_all" => {
                for id in self.overlays.ids() {
                    if let Some(o) = self.overlays.get_mut(id) {
                        o.set_visible(true);
                    }
                }
                Response::ok(req.id, json!({ "hidden": false }))
            }
            "overlay.topmost_watchdog" => {
                self.overlays.enforce_topmost();
                Response::ok(req.id, json!({ "ok": true }))
            }

            #[cfg(feature = "triggers")]
            "trigger.load" => {
                let dir = params.get("dir").and_then(Value::as_str).unwrap_or_default();
                if dir.is_empty() {
                    return Response::err(req.id, "missing 'dir'");
                }
                match self.triggers.load_pack(Path::new(dir)) {
                    Ok(id) => Response::ok(req.id, json!({ "trigger_id": id })),
                    Err(e) => Response::err(req.id, e),
                }
            }
            #[cfg(feature = "triggers")]
            "trigger.start" => {
                let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
                match self.triggers.start(id) {
                    Ok(()) => Response::ok(req.id, json!({ "running": true, "id": id })),
                    Err(e) => Response::err(req.id, e),
                }
            }
            #[cfg(feature = "triggers")]
            "trigger.stop" => {
                let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
                self.triggers.stop(id);
                Response::ok(req.id, json!({ "running": false, "id": id }))
            }
            #[cfg(feature = "triggers")]
            "trigger.list" => Response::ok(
                req.id,
                json!({
                    "packs": self.triggers.pack_ids(),
                    "running": self.triggers.pack_ids()
                        .into_iter()
                        .filter(|id| self.triggers.is_running(id))
                        .collect::<Vec<_>>(),
                }),
            ),
            #[cfg(feature = "triggers")]
            "trigger.status" => {
                let id = params.get("id").and_then(Value::as_str).unwrap_or_default();
                Response::ok(
                    req.id,
                    json!({
                        "id": id,
                        "running": self.triggers.is_running(&id),
                        "active": self.triggers.is_active(&id),
                        "score": self.triggers.score(&id),
                    }),
                )
            }
            #[cfg(feature = "triggers")]
            "trigger.tick" => {
                let now = params.get("now_ms").and_then(Value::as_u64).unwrap_or(0);
                let outcomes = self.triggers.tick(now);
                Response::ok(req.id, json!({ "outcomes": outcomes }))
            }

            other => Response::err(req.id, format!("unknown command '{other}'")),
        }
    }

    fn overlay_play(&mut self, req: &Request) -> Response {
        let params = &req.params;
        let path = params.get("path").and_then(Value::as_str).unwrap_or_default();
        if path.is_empty() {
            return Response::err(req.id, "missing 'path'");
        }
        let x = params.get("x").and_then(Value::as_i64).unwrap_or(0) as i32;
        let y = params.get("y").and_then(Value::as_i64).unwrap_or(0) as i32;
        let fps = params.get("fps").and_then(Value::as_i64).unwrap_or(15) as i32;
        let scale = params.get("scale").and_then(Value::as_f64).unwrap_or(0.20);
        let smooth = params.get("smooth").and_then(Value::as_bool).unwrap_or(true);
        let always_on_top = params.get("always_on_top").and_then(Value::as_bool).unwrap_or(true);
        let click_through = params.get("click_through").and_then(Value::as_bool).unwrap_or(true);
        let physics = params.get("physics").and_then(Value::as_bool).unwrap_or(false);
        let bouncy = params.get("bouncy").and_then(Value::as_bool).unwrap_or(true);
        let chroma_key = params.get("chroma_key").and_then(Value::as_bool).unwrap_or(false);
        let chroma_color = parse_hex_color(params.get("chroma_color").and_then(Value::as_str)).unwrap_or([255, 255, 255]);
        let chroma_tolerance = params.get("chroma_tolerance").and_then(Value::as_f64).unwrap_or(0.15) as f32;
        let chroma_contiguous = params.get("chroma_contiguous").and_then(Value::as_bool).unwrap_or(true);
        let chroma = ChromaKeyConfig {
            enabled: chroma_key,
            color_rgb: chroma_color,
            tolerance: chroma_tolerance.clamp(0.001, 1.0),
            contiguous: chroma_contiguous,
            feather: 0.04,
        };
        let alpha = params.get("alpha").and_then(Value::as_f64).unwrap_or(1.0) as f32;
        let label = params
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or(path)
            .to_string();

        let frames = match self.load_media(Path::new(path)) {
            Ok(f) => f,
            Err(e) => return Response::err(req.id, e),
        };
        let frame_count = frames.frames.len();
        let frame_width = frames.width;
        let frame_height = frames.height;

        match self.overlays.create(
            frames,
            x,
            y,
            fps,
            scale,
            smooth,
            always_on_top,
            click_through,
            physics,
            bouncy,
            chroma,
            label,
        ) {
            Ok(id) => {
                if let Some(overlay) = self.overlays.get_mut(id) {
                    overlay.set_alpha(alpha);
                }
                Response::ok(
                    req.id,
                    json!({
                        "overlay_id": id,
                        "width": frame_width,
                        "height": frame_height,
                        "frames": frame_count,
                    }),
                )
            }
            Err(e) => Response::err(req.id, e),
        }
    }

    fn overlay_stop(&mut self, req: &Request) -> Response {
        let id = overlay_id(&req.params);
        if self.overlays.get(id).is_none() {
            return Response::err(req.id, format!("unknown overlay {id}"));
        }
        self.overlays.destroy(id);
        Response::ok(req.id, json!({ "stopped": id, "remaining": self.overlays.len() }))
    }

    fn overlay_set(&mut self, req: &Request) -> Response {
        let id = overlay_id(&req.params);
        let params = req.params.clone();
        let Some(overlay) = self.overlays.get_mut(id) else {
            return Response::err(req.id, format!("unknown overlay {id}"));
        };

        if let Some(x) = params.get("x").and_then(Value::as_i64) {
            let y = params.get("y").and_then(Value::as_i64).unwrap_or(overlay.y as i64) as i32;
            overlay.move_to(x as i32, y);
        }
        if let Some(alpha) = params.get("alpha").and_then(Value::as_f64) {
            overlay.set_alpha(alpha as f32);
        }
        if let Some(scale) = params.get("scale").and_then(Value::as_f64) {
            overlay.rescale(scale);
        }
        if let Some(smooth) = params.get("smooth").and_then(Value::as_bool) {
            overlay.set_smooth(smooth);
        }
        if let Some(value) = params.get("always_on_top").and_then(Value::as_bool) {
            overlay.set_always_on_top(value);
        }
        if let Some(value) = params.get("click_through").and_then(Value::as_bool) {
            overlay.set_click_through(value);
        }
        if let Some(value) = params.get("physics").and_then(Value::as_bool) {
            overlay.set_physics(value);
        }
        if let Some(value) = params.get("bouncy").and_then(Value::as_bool) {
            overlay.set_bouncy(value);
        }
        if let Some(value) = params.get("visible").and_then(Value::as_bool) {
            overlay.set_visible(value);
        }

        let mut chroma = overlay.chroma.clone();
        let mut chroma_changed = false;
        if let Some(v) = params.get("chroma_key").and_then(Value::as_bool) {
            chroma.enabled = v;
            chroma_changed = true;
        }
        if let Some(s) = params.get("chroma_color").and_then(Value::as_str) {
            if let Some(rgb) = parse_hex_color(Some(s)) {
                chroma.color_rgb = rgb;
                chroma_changed = true;
            }
        }
        if let Some(v) = params.get("chroma_tolerance").and_then(Value::as_f64) {
            chroma.tolerance = (v as f32).clamp(0.001, 1.0);
            chroma_changed = true;
        }
        if let Some(v) = params.get("chroma_contiguous").and_then(Value::as_bool) {
            chroma.contiguous = v;
            chroma_changed = true;
        }
        if chroma_changed {
            overlay.set_chroma(chroma);
        }

        Response::ok(req.id, json!({ "updated": id }))
    }

    fn overlay_list(&self) -> Vec<Value> {
        self.overlays
            .ids()
            .into_iter()
            .filter_map(|id| {
                let o = self.overlays.get(id)?;
                Some(json!({
                    "overlay_id": id,
                    "label": o.label,
                    "x": o.x,
                    "y": o.y,
                    "width": o.width(),
                    "height": o.height(),
                    "scale": o.scale,
                    "smooth": o.smooth,
                    "alpha": o.alpha,
                    "always_on_top": o.always_on_top,
                    "click_through": o.click_through,
                    "physics": o.physics.enabled,
                    "bouncy": o.physics.bouncy,
                    "is_grounded": o.physics.is_grounded,
                    "chroma_key": o.chroma.enabled,
                    "chroma_color": format!("#{:02X}{:02X}{:02X}", o.chroma.color_rgb[0], o.chroma.color_rgb[1], o.chroma.color_rgb[2]),
                    "chroma_tolerance": o.chroma.tolerance,
                    "chroma_contiguous": o.chroma.contiguous,
                    "visible": o.visible,
                    "frames": o.frames.frames.len(),
                    "source": o.frames.source,
                }))
            })
            .collect()
    }

    fn stats(&self) -> Value {
        json!({
            "pid": std::process::id(),
            "uptime_ms": self.started_at.elapsed().as_millis() as u64,
            "overlays": self.overlays.len(),
            "media_cached": self.media_cache.len(),
            "frames_decoded": self.frames_decoded,
            "frames_presented": crate::win::overlay::FRAMES_PRESENTED.load(std::sync::atomic::Ordering::Relaxed),
            "commands_handled": self.commands_handled,
            "working_set_mb": working_set_mb(),
            "private_usage_mb": private_usage_mb(),
        })
    }
}

/// Reads the current process working set, in megabytes.
pub fn working_set_mb() -> f64 {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
        .is_ok()
        {
            return counters.WorkingSetSize as f64 / (1024.0 * 1024.0);
        }
    }
    0.0
}

/// Flushes non-essential physical memory pages from the process working set back to the OS.
pub fn trim_working_set() {
    use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
    use windows::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

/// Private (committed, non-shared) memory in megabytes. A growing private
/// value points at a heap leak; a growing working set with a flat private
/// value points at shared or mapped memory.
pub fn private_usage_mb() -> f64 {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        if GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
        .is_ok()
        {
            return counters.PrivateUsage as f64 / (1024.0 * 1024.0);
        }
    }
    0.0
}

fn overlay_id(params: &Value) -> OverlayId {
    params
        .get("overlay_id")
        .or_else(|| params.get("id"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn parse_hex_color(s: Option<&str>) -> Option<[u8; 3]> {
    let s = s?.trim();
    let s = s.strip_prefix('#').unwrap_or(s);
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some([r, g, b])
    } else if s.contains(',') {
        let parts: Vec<&str> = s.split(',').map(|p| p.trim()).collect();
        if parts.len() == 3 {
            let r = parts[0].parse().ok()?;
            let g = parts[1].parse().ok()?;
            let b = parts[2].parse().ok()?;
            Some([r, g, b])
        } else {
            None
        }
    } else {
        None
    }
}

/// Normalises a path so the media cache hits consistently.
fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
