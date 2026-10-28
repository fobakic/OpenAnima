//! The trigger engine.
//!
//! A *pack* (as produced by the AnimaTriggers editor) describes one or more
//! screen regions plus the reference frames that identify them. The engine
//! polls those regions, hashes what it sees and reports when the match
//! quality stays above the configured threshold.
//!
//! Compared with the Python implementation this removes the two dominant
//! costs: a fresh `mss.mss()` per region per tick, and a full ORB feature
//! extraction per region per tick.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::trigger::ahash::{area_resize_gray, hash_pair, similarity};
use crate::trigger::npy;
use crate::win::capture::ScreenCapture;

/// A capturable screen region.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RegionSpec {
    #[serde(default)]
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
}

/// Match tuning, mirroring `MatchSettings` in the Python model.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MatchSettings {
    pub mode: String,
    pub threshold: f32,
    pub agg: String,
    pub region_weights: HashMap<String, f32>,
    pub cooldown_ms: u64,
    pub start_frames_to_check: u32,
    pub stop_on_mismatch: bool,
    pub stop_grace_ms: u64,
    pub ref_mode: String,
    pub ref_frame_index: usize,
    pub seq_len: usize,
    pub seq_step: usize,
}

impl Default for MatchSettings {
    fn default() -> Self {
        MatchSettings {
            mode: "ORB_HASH".into(),
            threshold: 0.75,
            agg: "mean".into(),
            region_weights: HashMap::new(),
            cooldown_ms: 500,
            start_frames_to_check: 3,
            stop_on_mismatch: true,
            stop_grace_ms: 250,
            ref_mode: "anchor".into(),
            ref_frame_index: 0,
            seq_len: 8,
            seq_step: 1,
        }
    }
}

/// The event clip a pack is bound to.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EventSpec {
    pub id: String,
    pub name: String,
    pub fps: i32,
    pub frame_count: usize,
    pub start_index: usize,
    pub end_index: Option<usize>,
}

impl Default for EventSpec {
    fn default() -> Self {
        EventSpec {
            id: String::new(),
            name: String::new(),
            fps: 15,
            frame_count: 0,
            start_index: 0,
            end_index: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PackFile {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    regions: Vec<RegionSpec>,
    #[serde(default, rename = "event")]
    event: EventSpec,
    #[serde(default, rename = "match")]
    match_settings: MatchSettings,
    #[serde(default)]
    action_speed: f32,
}

#[derive(Debug, Deserialize)]
struct FramesMeta {
    #[serde(default)]
    regions: Vec<FramesMetaRegion>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct FramesMetaRegion {
    index: usize,
    path: String,
    #[serde(default)]
    count: usize,
}

/// A loaded trigger pack with its reference hashes.
#[allow(dead_code)]
pub struct TriggerPack {
    pub id: String,
    pub name: String,
    pub regions: Vec<RegionSpec>,
    pub match_settings: MatchSettings,
    pub event: EventSpec,
    pub action_speed: f32,
    /// Per region: the hash of every reference frame.
    pub reference: Vec<Vec<(u64, u64)>>,
    /// Per region: the pixel size of the reference frames.
    pub reference_size: Vec<(u32, u32)>,
    pub dir: PathBuf,
}

impl TriggerPack {
    /// Loads a pack directory (`pack.json` + `frames/`).
    pub fn load(dir: &Path) -> Result<TriggerPack, String> {
        let pack_json = dir.join("pack.json");
        let text = std::fs::read_to_string(&pack_json)
            .map_err(|e| format!("read {}: {e}", pack_json.display()))?;
        let parsed: PackFile = serde_json::from_str(&text)
            .map_err(|e| format!("parse {}: {e}", pack_json.display()))?;

        let frames_dir = dir.join("frames");
        let meta_path = frames_dir.join("frames_meta.json");
        let meta_text = std::fs::read_to_string(&meta_path)
            .map_err(|e| format!("read {}: {e}", meta_path.display()))?;
        let meta: FramesMeta = serde_json::from_str(&meta_text)
            .map_err(|e| format!("parse {}: {e}", meta_path.display()))?;

        let mut reference: Vec<Vec<(u64, u64)>> = Vec::new();
        let mut reference_size: Vec<(u32, u32)> = Vec::new();

        for entry in &meta.regions {
            let npz_path = frames_dir.join(&entry.path);
            let array = npy::read_npz(&npz_path, "frames")?;
            if array.shape.len() != 4 {
                return Err(format!(
                    "{}: expected a [T,H,W,C] array, got {:?}",
                    npz_path.display(),
                    array.shape
                ));
            }
            let frames = array.shape[0];
            let height = array.shape[1] as u32;
            let width = array.shape[2] as u32;
            let channels = array.shape[3];

            if height == 0 || width == 0 || channels == 0 {
                return Err(format!(
                    "{}: invalid dimensions: [{}, {}, {}, {}]",
                    npz_path.display(),
                    frames,
                    height,
                    width,
                    channels
                ));
            }

            let mut hashes = Vec::with_capacity(frames);
            for f in 0..frames {
                let start = f * height as usize * width as usize * channels;
                let end = start + height as usize * width as usize * channels;
                if end > array.data.len() {
                    break;
                }
                let slice = &array.data[start..end];
                let gray = if channels == 1 {
                    slice.to_vec()
                } else if channels >= 3 {
                    // BT.601 Luma conversion matching ScreenCapture and ahash (R*77 + G*150 + B*29) >> 8
                    let mut g = vec![0u8; width as usize * height as usize];
                    for i in 0..(width as usize * height as usize) {
                        let s = i * channels;
                        let r = slice[s] as u32;
                        let gr = slice[s + 1] as u32;
                        let b = slice[s + 2] as u32;
                        let luma = (r * 77 + gr * 150 + b * 29) >> 8;
                        g[i] = luma.min(255) as u8;
                    }
                    g
                } else {
                    let mut g = vec![0u8; width as usize * height as usize];
                    for i in 0..(width as usize * height as usize) {
                        let s = i * channels;
                        g[i] = slice[s];
                    }
                    g
                };
                hashes.push(hash_pair(&gray, width, height));
            }
            reference.push(hashes);
            reference_size.push((width, height));
        }

        Ok(TriggerPack {
            id: parsed.id,
            name: parsed.name,
            regions: parsed.regions,
            match_settings: parsed.match_settings,
            event: parsed.event,
            action_speed: parsed.action_speed,
            reference,
            reference_size,
            dir: dir.to_path_buf(),
        })
    }
}

/// Live state of one running trigger.
struct RunState {
    active: bool,
    last_score: f32,
    ok_streak: u32,
    seq_index: usize,
    last_fire_ms: u64,
    mismatch_since: Option<u64>,
    fired: u64,
}

/// Outcome of one engine tick.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TickOutcome {
    pub trigger_id: String,
    pub score: f32,
    pub fired: bool,
    pub active: bool,
}

/// Runs trigger packs against the live screen.
pub struct TriggerEngine {
    packs: HashMap<String, TriggerPack>,
    running: HashMap<String, RunState>,
    capture: ScreenCapture,
    now_ms: u64,
}

impl TriggerEngine {
    pub fn new() -> Result<Self, String> {
        Ok(TriggerEngine {
            packs: HashMap::new(),
            running: HashMap::new(),
            capture: ScreenCapture::new()?,
            now_ms: 0,
        })
    }

    pub fn load_pack(&mut self, dir: &Path) -> Result<String, String> {
        let pack = TriggerPack::load(dir)?;
        let id = if pack.id.is_empty() { pack.name.clone() } else { pack.id.clone() };
        self.packs.insert(id.clone(), pack);
        Ok(id)
    }

    #[allow(dead_code)]
    pub fn remove_pack(&mut self, id: &str) {
        self.packs.remove(id);
        self.running.remove(id);
    }

    pub fn pack_ids(&self) -> Vec<String> {
        self.packs.keys().cloned().collect()
    }

    pub fn start(&mut self, id: &str) -> Result<(), String> {
        if !self.packs.contains_key(id) {
            return Err(format!("unknown pack '{id}'"));
        }
        self.running.insert(
            id.to_string(),
            RunState {
                active: false,
                last_score: 0.0,
                ok_streak: 0,
                seq_index: 0,
                last_fire_ms: 0,
                mismatch_since: None,
                fired: 0,
            },
        );
        Ok(())
    }

    pub fn stop(&mut self, id: &str) {
        self.running.remove(id);
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains_key(id)
    }

    /// Current score of a running trigger.
    pub fn score(&self, id: &str) -> f32 {
        self.running.get(id).map(|s| s.last_score).unwrap_or(0.0)
    }

    pub fn is_active(&self, id: &str) -> bool {
        self.running.get(id).map(|s| s.active).unwrap_or(false)
    }

    /// Advances every running trigger by one step.
    pub fn tick(&mut self, now_ms: u64) -> Vec<TickOutcome> {
        self.now_ms = now_ms;
        let mut outcomes = Vec::new();

        let ids: Vec<String> = self.running.keys().cloned().collect();
        for id in ids {
            // Read the sequencer offset before borrowing the pack, so the
            // immutable borrow of `self.packs` does not overlap with the
            // mutable borrow needed for screen capture.
            let seq_offset = self.running.get(&id).map(|s| s.seq_index).unwrap_or(0);

            struct Work {
                region: RegionSpec,
                reference: (u64, u64),
                ref_w: u32,
                ref_h: u32,
            }

            // Collect the per-region work while the pack is borrowed.
            let work: Vec<Work> = {
                let Some(pack) = self.packs.get(&id) else { continue };
                let settings = &pack.match_settings;
                let mut work = Vec::new();
                for (index, region) in pack.regions.iter().enumerate() {
                    let Some(refs) = pack.reference.get(index) else { continue };
                    if refs.is_empty() {
                        continue;
                    }
                    let (ref_w, ref_h) = pack
                        .reference_size
                        .get(index)
                        .copied()
                        .unwrap_or((region.w as u32, region.h as u32));

                    // Which reference frame does this tick compare against?
                    let ref_index = if settings.ref_mode == "sequencer" {
                        let base = pack.event.start_index;
                        let step = settings.seq_step.max(1);
                        base + seq_offset * step
                    } else {
                        pack.event.start_index + settings.ref_frame_index
                    };

                    let Some(reference) = refs.get(ref_index.min(refs.len() - 1)) else {
                        continue;
                    };
                    work.push(Work {
                        region: region.clone(),
                        reference: *reference,
                        ref_w,
                        ref_h,
                    });
                }
                work
            };

            let settings = match self.packs.get(&id) {
                Some(pack) => pack.match_settings.clone(),
                None => continue,
            };
            let mut region_scores: Vec<(f32, f32)> = Vec::new(); // (score, weight)

            for item in work {
                let live = self.capture_live_hash(&item.region, item.ref_w, item.ref_h);
                let Some(live_hash) = live else { continue };

                let score = similarity(live_hash, item.reference);
                let weight = settings
                    .region_weights
                    .get(&item.region.id)
                    .copied()
                    .unwrap_or(1.0);
                region_scores.push((score, weight));
            }

            if region_scores.is_empty() {
                continue;
            }

            let total_weight: f32 = region_scores.iter().map(|(_, w)| *w).sum();
            let aggregated = match settings.agg.as_str() {
                "min" => region_scores.iter().map(|(s, _)| *s).fold(f32::INFINITY, f32::min),
                "weighted" => {
                    if total_weight > 0.0 {
                        region_scores.iter().map(|(s, w)| s * w).sum::<f32>() / total_weight
                    } else {
                        0.0
                    }
                }
                _ => region_scores.iter().map(|(s, _)| *s).sum::<f32>() / region_scores.len() as f32,
            };

            let mut fired = false;
            if let Some(state) = self.running.get_mut(&id) {
                state.last_score = aggregated;
                if aggregated >= settings.threshold {
                    state.ok_streak += 1;
                    state.mismatch_since = None;
                    if settings.ref_mode == "sequencer" {
                        state.seq_index += 1;
                    }
                    let required = settings.start_frames_to_check.max(1);
                    let cooled = now_ms.saturating_sub(state.last_fire_ms) >= settings.cooldown_ms;
                    if state.ok_streak >= required && cooled {
                        state.active = true;
                        state.last_fire_ms = now_ms;
                        state.fired += 1;
                        fired = true;
                    }
                } else {
                    state.ok_streak = 0;
                    if settings.ref_mode == "sequencer" {
                        state.seq_index = 0;
                    }
                    if settings.stop_on_mismatch {
                        let since = *state.mismatch_since.get_or_insert(now_ms);
                        if now_ms.saturating_sub(since) >= settings.stop_grace_ms {
                            state.active = false;
                        }
                    }
                }
            }

            let active = self.is_active(&id);
            outcomes.push(TickOutcome {
                trigger_id: id.clone(),
                score: aggregated,
                fired,
                active,
            });
        }

        outcomes
    }

    /// Captures a region and returns its perceptual hash.
    fn capture_live_hash(
        &mut self,
        region: &RegionSpec,
        ref_w: u32,
        ref_h: u32,
    ) -> Option<(u64, u64)> {
        if region.w <= 0 || region.h <= 0 {
            return None;
        }
        if self.capture.grab(region.x, region.y, region.w, region.h).is_err() {
            return None;
        }
        let gray = self.capture.to_gray();
        // Match the reference pipeline: the live region is resampled to the
        // reference frame size before hashing.
        let resized = if ref_w as usize == region.w as usize && ref_h as usize == region.h as usize
        {
            gray
        } else {
            area_resize_gray(&gray, region.w as u32, region.h as u32, ref_w as usize, ref_h as usize)
        };
        Some(hash_pair(&resized, ref_w, ref_h))
    }
}
