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
    pub broadcaster: Option<crate::ipc::Broadcaster>,
    pub active_conversions: Arc<std::sync::Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>>,
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
            broadcaster: None,
            active_conversions: Arc::new(std::sync::Mutex::new(HashMap::new())),
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
                        "media.info", "media.convert_start", "media.convert_cancel",
                        "trigger.load", "trigger.start", "trigger.stop",
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

            "core.set_language" => {
                if let Some(lang) = params.get("lang").and_then(Value::as_str) {
                    crate::win::overlay::set_core_language(lang);
                }
                Response::ok(req.id, json!({ "ok": true }))
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

            "media.convert_start" => self.media_convert_start(req),
            "media.convert_cancel" => self.media_convert_cancel(req),

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

    fn media_convert_start(&mut self, req: &Request) -> Response {
        let params = &req.params;
        let in_path_str = match params.get("in_path").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return Response::err(req.id, "missing or empty 'in_path'"),
        };

        let in_path = PathBuf::from(&in_path_str);
        if !in_path.exists() {
            return Response::err(req.id, format!("File not found: {}", in_path_str));
        }

        let out_path = if let Some(out_str) = params.get("out_path").and_then(Value::as_str) {
            PathBuf::from(out_str)
        } else {
            let mut out = in_path.clone();
            out.set_extension("apng");
            out
        };

        let job_id = params
            .get("job_id")
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                format!("job_{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis())
            });

        let target_fps = params.get("fps").and_then(Value::as_u64).unwrap_or(20) as u32;
        let max_dimension = params.get("max_dim").and_then(Value::as_u64).unwrap_or(384) as u32;

        let chroma_enabled = params.get("chroma_key").and_then(Value::as_bool).unwrap_or(false);
        let chroma_color = parse_hex_color(params.get("chroma_color").and_then(Value::as_str))
            .unwrap_or([255, 255, 255]);
        let chroma_tolerance = params.get("chroma_tolerance").and_then(Value::as_f64).unwrap_or(0.15) as f32;
        let chroma_contiguous = params.get("chroma_contiguous").and_then(Value::as_bool).unwrap_or(true);

        let options = crate::media::apng_encoder::ConversionOptions {
            target_fps,
            max_dimension,
            chroma_key: ChromaKeyConfig {
                enabled: chroma_enabled,
                color_rgb: chroma_color,
                tolerance: chroma_tolerance,
                contiguous: chroma_contiguous,
                feather: 0.04,
            },
            quality_level: 6,
        };

        let cancel_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut jobs = self.active_conversions.lock().unwrap();
            jobs.insert(job_id.clone(), cancel_flag.clone());
        }

        let broadcaster_opt = self.broadcaster.clone();
        let thread_job_id = job_id.clone();
        let thread_in_path = in_path.clone();
        let thread_out_path = out_path.clone();
        let thread_cancel_flag = cancel_flag.clone();
        let thread_active_conversions = self.active_conversions.clone();

        if let Err(e) = std::thread::Builder::new()
            .name(format!("convert-{}", job_id))
            .spawn(move || {
                log_core(&format!("Started conversion job {}: {} -> {}", thread_job_id, thread_in_path.display(), thread_out_path.display()));
                let mut last_broadcast = std::time::Instant::now();

                let res = crate::media::apng_encoder::convert_mp4_to_apng(
                    &thread_in_path,
                    &thread_out_path,
                    &options,
                    &thread_cancel_flag,
                    |prog| {
                        if let Some(ref broadcaster) = broadcaster_opt {
                            if last_broadcast.elapsed().as_millis() >= 40 || prog.percent >= 99.0 {
                                last_broadcast = std::time::Instant::now();
                                broadcaster.broadcast(crate::proto::Event::new(
                                    "convert.progress",
                                    json!({
                                        "job_id": thread_job_id,
                                        "percent": prog.percent,
                                        "current": prog.current_frame,
                                        "total": prog.total_frames,
                                    }),
                                ));
                            }
                        }
                    },
                );

                if let Some(ref broadcaster) = broadcaster_opt {
                    match res {
                        Ok(()) => {
                            log_core(&format!("Conversion completed for job {}", thread_job_id));
                            broadcaster.broadcast(crate::proto::Event::new(
                                "convert.completed",
                                json!({
                                    "job_id": thread_job_id,
                                    "in_path": thread_in_path.to_string_lossy(),
                                    "out_path": thread_out_path.to_string_lossy(),
                                }),
                            ));
                        }
                        Err(ref e) if thread_cancel_flag.load(std::sync::atomic::Ordering::Relaxed) => {
                            log_core(&format!("Conversion cancelled for job {}", thread_job_id));
                            broadcaster.broadcast(crate::proto::Event::new(
                                "convert.cancelled",
                                json!({
                                    "job_id": thread_job_id,
                                }),
                            ));
                        }
                        Err(ref e) => {
                            log_core(&format!("Conversion error for job {}: {}", thread_job_id, e));
                            broadcaster.broadcast(crate::proto::Event::new(
                                "convert.error",
                                json!({
                                    "job_id": thread_job_id,
                                    "error": e,
                                }),
                            ));
                        }
                    }
                }

                let mut jobs = thread_active_conversions.lock().unwrap();
                jobs.remove(&thread_job_id);
            }) {
            return Response::err(req.id, format!("Failed to spawn conversion thread: {:?}", e));
        }

        Response::ok(
            req.id,
            json!({
                "job_id": job_id,
                "in_path": in_path.to_string_lossy(),
                "out_path": out_path.to_string_lossy(),
                "status": "started",
            }),
        )
    }

    fn media_convert_cancel(&mut self, req: &Request) -> Response {
        let params = &req.params;
        let job_id = params.get("job_id").and_then(Value::as_str);

        let jobs = self.active_conversions.lock().unwrap();
        if let Some(id) = job_id {
            if let Some(flag) = jobs.get(id) {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                return Response::ok(req.id, json!({ "status": "cancelling", "job_id": id }));
            }
            Response::err(req.id, format!("No active job with id '{}'", id))
        } else {
            for flag in jobs.values() {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Response::ok(req.id, json!({ "status": "cancelling_all" }))
        }
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
    } else if s.len() == 3 {
        let r = u8::from_str_radix(&s[0..1], 16).ok()?;
        let g = u8::from_str_radix(&s[1..2], 16).ok()?;
        let b = u8::from_str_radix(&s[2..3], 16).ok()?;
        Some([r * 17, g * 17, b * 17])
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

pub fn log_core(msg: &str) {
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let mut path = std::path::PathBuf::from(local_app_data);
        path.push("OpenAnima");
        path.push("logs");
        let _ = std::fs::create_dir_all(&path);
        path.push("openanima_core.log");
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(f, "[{:?}] {}", std::time::SystemTime::now(), msg);
        }
    }
}
