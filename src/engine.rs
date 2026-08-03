// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Customation AS
//! Engine handles and the level catalog.
//!
//! One `bgsage_engine` handle = one evaluation level (a strategy stack).
//! Handles are created lazily (weights load from disk) and cached for the
//! process lifetime; calls on a handle are serialized by its call lock,
//! which is the capi's thread-safety contract.

use std::collections::HashMap;
use std::ffi::{c_int, c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bep_protocol::contract::{kinds, Conventions, Describe, EngineIdentity, Level, RolloutParams};
use bep_protocol::gnubg_ids::{Board, CubeOwner, MatchContext};
use serde::Deserialize;

use crate::ffi::{
    Capi, CapiError, CubeCtx, CubeResult, EngineConfig, EngineOpaque, Eval, Move, RolloutConfig,
    KIND_PLY, KIND_ROLLOUT, OWNER_CENTERED, OWNER_MOVER, OWNER_OPPONENT,
};

pub const PROTOCOL_VERSION: &str = "0.1";
pub const ENGINE_FAMILY: &str = "bgsage";
pub const ENGINE_DISPLAY_NAME: &str = "Open Sage";
/// One request at a time: every level shares the machine's cores through
/// the engine's internal parallelism, so interleaving requests only adds
/// contention. Cancellation still works — the reader loop never blocks.
pub const MAX_PARALLEL: u32 = 1;
/// bgsage counts plies the XG way: 1-ply = raw NN.
pub const PLY_COUNTING: &str = "xg";
pub const EQUITY_CONVENTION: &str = "contract";

/// Level ids advertised by describe. Opaque to hosts; single definition.
pub mod levels {
    pub const PLY_1: &str = "1ply";
    pub const PLY_2: &str = "2ply";
    pub const PLY_3: &str = "3ply";
    pub const PLY_4: &str = "4ply";
    pub const ROLLER_2T: &str = "2T";
    pub const ROLLER_3T: &str = "3T";
    pub const ROLLOUT: &str = "rollout";
}

// Ply-parity note for the cube, XG counting.
//
// A cube decision is only sound when the search leaves the ROOT player on
// roll at the leaves — when each player has been evaluated an equal number
// of times. Otherwise the on-roll bonus lands on one side and skews the
// win/gammon/backgammon split the cube action is computed from. Which
// advertised plies satisfy that is a counting convention, not a difference
// in the maths: bgsage counts the XG way (1-ply = raw NN), so the balanced
// depths are the ODD ones, 1 and 3; gnubg counts from 0 and lands on the
// even ones.
//
// This engine deliberately does NOT enforce that. A caller may ask any
// level any question — comparing a 2-ply cube against another
// implementation is a legitimate experiment, and the parity suite does
// exactly that. Refusing here would make honest work impossible while
// preventing no real mistake, because the mistake is never "someone asked",
// it is "we chose the wrong depth on a user's behalf".
//
// That choice lives in the host, engine-neutrally: `analysis.ts`
// normalizes each engine's advertised ply to a canonical base-0 depth, and
// `gameEval.ts:pickCubeLevel` takes the even-canonical-depth level nearest
// 2 — which resolves to sage 3-ply and gnubg 2-ply. Every evaluateCube
// call site in the desktop app goes through that, including the bot's own
// double and take decisions.

pub const STRATEGY_BACKGAME_PAIR: &str = "backgame_pair";
pub const BEAROFF_DB_FILENAME: &str = "bearoff_1sided.db";

/// stage9 slot order with the canonical map already applied (19 slots, 15
/// distinct files; slots 11, 12, 15, 16 alias prim_anch). Mirrors
/// bgsage `python/bgsage/weights.py` MODELS["stage9"].
pub const STAGE9_PLAN_FILES: [&str; 19] = [
    "sl_s9_purerace.weights.best",
    "sl_s9_race_race.weights.best",
    "sl_s9_race_att.weights.best",
    "sl_s9_race_prim.weights.best",
    "sl_s9_race_anch.weights.best",
    "sl_s9_att_race.weights.best",
    "sl_s9_att_att.weights.best",
    "sl_s9_att_prim.weights.best",
    "sl_s9_att_anch.weights.best",
    "sl_s9_prim_race.weights.best",
    "sl_s9_prim_att.weights.best",
    "sl_s9_prim_anch.weights.best",
    "sl_s9_prim_anch.weights.best",
    "sl_s9_anch_race.weights.best",
    "sl_s9_anch_att.weights.best",
    "sl_s9_prim_anch.weights.best",
    "sl_s9_prim_anch.weights.best",
    "sl_s9_player_bg.weights.best",
    "sl_s9_opponent_bg.weights.best",
];
const STAGE9_HIDDEN_PURERACE: c_int = 100;
const STAGE9_HIDDEN_CONTACT: c_int = 400;

/// The named rollout presets, matching the analyzer's level definitions
/// (documented on `bgsage_rollout_config` in capi.h).
struct RolloutPreset {
    n_trials: c_int,
    truncation_depth: c_int,
    decision_ply: c_int,
    truncation_ply: c_int,
    late_ply: c_int,
    late_threshold: c_int,
    cube_ply: c_int,
    cube_late_ply: c_int,
}

/// Disables the mid-trial drop to 1-ply so configured strategies run the
/// whole game (required for accurate 2T/3T/full rollouts).
const ULTRA_LATE_DISABLED: c_int = 9999;
const PRESET_UNSET: c_int = 0;
const PRESET_INHERIT: c_int = -1;

const PRESET_2T: RolloutPreset = RolloutPreset {
    n_trials: 360,
    truncation_depth: 7,
    decision_ply: 2,
    truncation_ply: 2,
    late_ply: 1,
    late_threshold: 1,
    cube_ply: 2,
    cube_late_ply: 2,
};
const PRESET_3T: RolloutPreset = RolloutPreset {
    n_trials: 360,
    truncation_depth: 7,
    decision_ply: 3,
    truncation_ply: PRESET_INHERIT,
    late_ply: 2,
    late_threshold: 2,
    cube_ply: PRESET_UNSET,
    cube_late_ply: PRESET_UNSET,
};
pub const ROLLOUT_DEFAULT_TRIALS: u32 = 1296;
pub const ROLLOUT_DEFAULT_TRUNCATION: u32 = 0;

/// levelOptions accepted by the configurable "rollout" level. Unknown
/// keys are rejected — a misspelled option must not silently become a
/// default-parameter rollout.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RolloutOptions {
    #[serde(default)]
    pub trials: Option<u32>,
    #[serde(default)]
    pub truncation: Option<u32>,
    #[serde(default)]
    pub variance_reduction: Option<bool>,
    #[serde(default)]
    pub seed: Option<u32>,
}

impl RolloutOptions {
    fn cache_key(&self) -> String {
        format!(
            "trials={:?};truncation={:?};vr={:?};seed={:?}",
            self.trials, self.truncation, self.variance_reduction, self.seed
        )
    }
}

#[derive(Debug)]
pub enum LevelError {
    UnknownLevel(String),
    /// levelOptions supplied for a level that is not configurable, or
    /// options that don't deserialize.
    InvalidOptions(String),
}

impl std::fmt::Display for LevelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LevelError::UnknownLevel(level) => write!(f, "unknown level {level:?}"),
            LevelError::InvalidOptions(message) => write!(f, "invalid levelOptions: {message}"),
        }
    }
}

/// Contract Plies stamp per level: ply levels carry their XG-convention
/// depth, fixed roller/rollout levels stamp 0 (the platform's dedup
/// identity for non-ply engines).
fn plies_stamp(level_id: &str) -> i32 {
    match level_id {
        levels::PLY_1 => 1,
        levels::PLY_2 => 2,
        levels::PLY_3 => 3,
        levels::PLY_4 => 4,
        _ => 0,
    }
}

/// Weight + bearoff file set, validated at startup, alive for the process.
pub struct WeightSet {
    strategy_type: CString,
    paths: Vec<CString>,
    hidden_sizes: Vec<c_int>,
    bearoff: Option<CString>,
}

impl WeightSet {
    pub fn stage9(weights_dir: &Path, bearoff_db: Option<&Path>) -> Result<WeightSet, String> {
        let mut paths = Vec::with_capacity(STAGE9_PLAN_FILES.len());
        let mut hidden_sizes = Vec::with_capacity(STAGE9_PLAN_FILES.len());
        let mut missing: Vec<PathBuf> = Vec::new();
        for (slot, file) in STAGE9_PLAN_FILES.iter().enumerate() {
            let path = weights_dir.join(file);
            if !path.is_file() && !missing.contains(&path) {
                missing.push(path.clone());
            }
            paths.push(path_cstring(&path)?);
            hidden_sizes.push(if slot == 0 { STAGE9_HIDDEN_PURERACE } else { STAGE9_HIDDEN_CONTACT });
        }
        if !missing.is_empty() {
            return Err(format!(
                "missing weight files in {}: {}",
                weights_dir.display(),
                missing.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            ));
        }
        let bearoff = match bearoff_db {
            Some(path) => {
                if !path.is_file() {
                    return Err(format!("bearoff database not found: {}", path.display()));
                }
                Some(path_cstring(path)?)
            }
            None => None,
        };
        Ok(WeightSet {
            strategy_type: CString::new(STRATEGY_BACKGAME_PAIR)
                .expect("constant contains no NUL"),
            paths,
            hidden_sizes,
            bearoff,
        })
    }
}

fn path_cstring(path: &Path) -> Result<CString, String> {
    CString::new(path.to_string_lossy().into_owned())
        .map_err(|_| format!("path contains a NUL byte: {}", path.display()))
}

/// A live engine level. Calls are serialized by `call_lock`; `cancel` is
/// deliberately outside the lock so it can interrupt a running rollout.
pub struct EngineHandle {
    capi: Arc<Capi>,
    ptr: *mut EngineOpaque,
    call_lock: Mutex<()>,
    pub supports_cancel: bool,
    pub plies_stamp: i32,
    /// What this handle actually resolved to, formatted where the values
    /// were decided rather than re-derived at the log site. Requested and
    /// effective settings are exactly the pair that can silently diverge,
    /// so logging anything re-computed from the request would defeat the
    /// purpose.
    pub config_summary: String,
}

// SAFETY: the raw pointer is only dereferenced under call_lock (except
// cancel/set_progress, which the capi defines as cross-thread-safe /
// pre-call operations).
unsafe impl Send for EngineHandle {}
unsafe impl Sync for EngineHandle {}

/// Context handed to the progress trampoline for one evaluation call.
pub struct ProgressContext {
    pub emit: Box<dyn Fn(i32, i32) + Send + Sync>,
}

unsafe extern "C" fn progress_trampoline(user: *mut c_void, done: c_int, total: c_int) {
    if user.is_null() {
        return;
    }
    let context = &*(user as *const ProgressContext);
    (context.emit)(done, total);
}

impl EngineHandle {
    pub fn pre_roll(&self, board: &Board, cube: &CubeCtx) -> Result<Eval, CapiError> {
        let _guard = self.lock();
        unsafe { self.capi.pre_roll(self.ptr, board, cube) }
    }

    pub fn cube_action(
        &self,
        board: &Board,
        cube: &CubeCtx,
        progress: Option<&ProgressContext>,
    ) -> Result<CubeResult, CapiError> {
        let _guard = self.lock();
        self.arm(progress);
        let result = unsafe { self.capi.cube_action(self.ptr, board, cube) };
        self.disarm(progress);
        result
    }

    pub fn checker_play(
        &self,
        board: &Board,
        die1: i32,
        die2: i32,
        cube: &CubeCtx,
        buffer: &mut [Move],
        progress: Option<&ProgressContext>,
    ) -> Result<usize, CapiError> {
        let _guard = self.lock();
        self.arm(progress);
        let result = unsafe { self.capi.checker_play(self.ptr, board, die1, die2, cube, buffer) };
        self.disarm(progress);
        result
    }

    /// Interrupt the evaluation currently running on this handle (no-op
    /// for ply levels, which cannot abort — the spec allows completing).
    pub fn cancel(&self) {
        self.capi.cancel(self.ptr);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.call_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn arm(&self, progress: Option<&ProgressContext>) {
        unsafe {
            // A fresh call must never inherit a cancel aimed at the
            // previous request on this handle.
            self.capi.reset_cancel(self.ptr);
            if let Some(context) = progress {
                self.capi.set_progress(
                    self.ptr,
                    Some(progress_trampoline),
                    context as *const ProgressContext as *mut c_void,
                );
            }
        }
    }

    fn disarm(&self, progress: Option<&ProgressContext>) {
        if progress.is_some() {
            unsafe { self.capi.set_progress(self.ptr, None, std::ptr::null_mut()) };
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        unsafe { self.capi.engine_destroy(self.ptr) };
    }
}

/// Lazily-constructed, cached engine handles keyed by level (and, for the
/// configurable rollout level, by its options).
pub struct EnginePool {
    capi: Arc<Capi>,
    weights: WeightSet,
    threads: c_int,
    handles: Mutex<HashMap<String, Arc<EngineHandle>>>,
}

impl EnginePool {
    pub fn new(capi: Arc<Capi>, weights: WeightSet, threads: c_int) -> EnginePool {
        EnginePool { capi, weights, threads, handles: Mutex::new(HashMap::new()) }
    }

    pub fn describe(&self, build: &str) -> Describe {
        Describe {
            protocol_version: PROTOCOL_VERSION.to_string(),
            engine: EngineIdentity {
                family: ENGINE_FAMILY.to_string(),
                display_name: ENGINE_DISPLAY_NAME.to_string(),
                version: self.capi.version(),
                build: build.to_string(),
            },
            max_parallel: MAX_PARALLEL,
            conventions: Conventions {
                ply_counting: PLY_COUNTING.to_string(),
                equity: EQUITY_CONVENTION.to_string(),
            },
            levels: vec![
                ply_level(levels::PLY_1, 1),
                ply_level(levels::PLY_2, 2),
                ply_level(levels::PLY_3, 3),
                ply_level(levels::PLY_4, 4),
                roller_level(levels::ROLLER_2T, kinds::ROLLER, "Sage 2T", &PRESET_2T),
                roller_level(levels::ROLLER_3T, kinds::ROLLER_PLUS, "Sage 3T", &PRESET_3T),
                Level {
                    id: levels::ROLLOUT.to_string(),
                    kind: kinds::ROLLOUT.to_string(),
                    display_name: None,
                    ply_depth: None,
                    rollout: Some(RolloutParams {
                        trials: ROLLOUT_DEFAULT_TRIALS,
                        truncation: ROLLOUT_DEFAULT_TRUNCATION,
                        variance_reduction: true,
                        // Full rollout plays 1-ply decisions (XG convention:
                        // 1-ply = raw NN) for both checker and cube.
                        checker_ply: Some(1),
                        cube_ply: Some(1),
                    }),
                    methods: None,
                    configurable: true,
                    supports_progress: true,
                    supports_cancel: true,
                },
            ],
        }
    }

    /// Resolve a level id (+ options) to a live handle, constructing and
    /// caching it on first use.
    pub fn get(
        &self,
        level_id: &str,
        options: Option<&serde_json::Value>,
    ) -> Result<Arc<EngineHandle>, EngineGetError> {
        let rollout_options = self.parse_options(level_id, options)?;
        let cache_key = match &rollout_options {
            Some(opts) => format!("{level_id}?{}", opts.cache_key()),
            None => level_id.to_string(),
        };

        let mut handles = self.handles.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(handle) = handles.get(&cache_key) {
            return Ok(Arc::clone(handle));
        }
        // Construction happens under the map lock: concurrent first
        // requests for the same level must not double-load 15 weight
        // files, and MAX_PARALLEL=1 makes contention theoretical.
        let handle = Arc::new(self.construct(level_id, rollout_options.as_ref())?);
        handles.insert(cache_key, Arc::clone(&handle));
        Ok(handle)
    }

    fn parse_options(
        &self,
        level_id: &str,
        options: Option<&serde_json::Value>,
    ) -> Result<Option<RolloutOptions>, EngineGetError> {
        match options {
            None => Ok(None),
            Some(value) => {
                if level_id != levels::ROLLOUT {
                    return Err(EngineGetError::Level(LevelError::InvalidOptions(format!(
                        "level {level_id:?} is not configurable"
                    ))));
                }
                serde_json::from_value::<RolloutOptions>(value.clone())
                    .map(Some)
                    .map_err(|e| EngineGetError::Level(LevelError::InvalidOptions(e.to_string())))
            }
        }
    }

    fn construct(
        &self,
        level_id: &str,
        options: Option<&RolloutOptions>,
    ) -> Result<EngineHandle, EngineGetError> {
        let mut rollout = self.capi.rollout_config_defaults();
        let (kind, n_plies, supports_cancel) = match level_id {
            levels::PLY_1 => (KIND_PLY, 1, false),
            levels::PLY_2 => (KIND_PLY, 2, false),
            levels::PLY_3 => (KIND_PLY, 3, false),
            levels::PLY_4 => (KIND_PLY, 4, false),
            levels::ROLLER_2T => {
                apply_preset(&mut rollout, &PRESET_2T);
                (KIND_ROLLOUT, 0, true)
            }
            levels::ROLLER_3T => {
                apply_preset(&mut rollout, &PRESET_3T);
                (KIND_ROLLOUT, 0, true)
            }
            levels::ROLLOUT => {
                rollout.n_trials = ROLLOUT_DEFAULT_TRIALS as c_int;
                rollout.truncation_depth = ROLLOUT_DEFAULT_TRUNCATION as c_int;
                rollout.ultra_late_threshold = ULTRA_LATE_DISABLED;
                if let Some(opts) = options {
                    if let Some(trials) = opts.trials {
                        rollout.n_trials = trials as c_int;
                    }
                    if let Some(truncation) = opts.truncation {
                        rollout.truncation_depth = truncation as c_int;
                    }
                    if let Some(vr) = opts.variance_reduction {
                        rollout.enable_vr = vr as c_int;
                    }
                    if let Some(seed) = opts.seed {
                        rollout.seed = seed;
                    }
                }
                (KIND_ROLLOUT, 0, true)
            }
            other => {
                return Err(EngineGetError::Level(LevelError::UnknownLevel(other.to_string())))
            }
        };

        let path_ptrs: Vec<*const std::ffi::c_char> =
            self.weights.paths.iter().map(|p| p.as_ptr()).collect();
        let config = EngineConfig {
            strategy_type: self.weights.strategy_type.as_ptr(),
            weight_paths: path_ptrs.as_ptr(),
            hidden_sizes: self.weights.hidden_sizes.as_ptr(),
            n_weights: self.weights.paths.len() as c_int,
            bearoff_db_path: self
                .weights
                .bearoff
                .as_ref()
                .map_or(std::ptr::null(), |p| p.as_ptr()),
            kind,
            n_plies,
            rollout,
            filter_max_moves: 0,  // 0 = capi default (TINY: 5)
            filter_threshold: 0.0, // 0 = capi default (0.08)
            threads: self.threads,
        };

        // 0 is the capi's "pick for me", so say that rather than print a
        // thread count of zero that no reader would believe.
        let threads = if self.threads == 0 {
            "auto".to_string()
        } else {
            self.threads.to_string()
        };
        let config_summary = if kind == KIND_ROLLOUT {
            format!(
                "Trials={} Trunc={} Decision={}ply Cube={}ply Threads={threads}",
                rollout.n_trials, rollout.truncation_depth, rollout.decision_ply, rollout.cube.ply,
            )
        } else {
            format!("Plies={n_plies} Threads={threads}")
        };

        let ptr = unsafe { self.capi.engine_create(&config) }.map_err(EngineGetError::Create)?;
        Ok(EngineHandle {
            capi: Arc::clone(&self.capi),
            ptr,
            call_lock: Mutex::new(()),
            supports_cancel,
            plies_stamp: plies_stamp(level_id),
            config_summary,
        })
    }
}

#[derive(Debug)]
pub enum EngineGetError {
    Level(LevelError),
    Create(CapiError),
}

fn apply_preset(config: &mut RolloutConfig, preset: &RolloutPreset) {
    config.n_trials = preset.n_trials;
    config.truncation_depth = preset.truncation_depth;
    config.decision_ply = preset.decision_ply;
    if preset.truncation_ply != PRESET_INHERIT {
        config.truncation_ply = preset.truncation_ply;
    }
    config.late_ply = preset.late_ply;
    config.late_threshold = preset.late_threshold;
    if preset.cube_ply != PRESET_UNSET {
        config.cube.ply = preset.cube_ply;
    }
    if preset.cube_late_ply != PRESET_UNSET {
        config.cube_late.ply = preset.cube_late_ply;
    }
    config.ultra_late_threshold = ULTRA_LATE_DISABLED;
}

fn ply_level(id: &str, depth: u32) -> Level {
    Level {
        id: id.to_string(),
        kind: kinds::PLY.to_string(),
        display_name: None,
        ply_depth: Some(depth),
        rollout: None,
        // Absent = answers all four methods (spec §7). Every ply level does;
        // see the ply-parity note above for why that is deliberate.
        methods: None,
        configurable: false,
        supports_progress: false,
        supports_cancel: false,
    }
}

fn roller_level(id: &str, kind: &str, display_name: &str, preset: &RolloutPreset) -> Level {
    Level {
        id: id.to_string(),
        kind: kind.to_string(),
        display_name: Some(display_name.to_string()),
        ply_depth: None,
        rollout: Some(RolloutParams {
            trials: preset.n_trials as u32,
            truncation: preset.truncation_depth as u32,
            variance_reduction: true,
            checker_ply: Some(preset.decision_ply as u32),
            // Unset cube ply resolves to the decision ply in the core.
            cube_ply: Some(if preset.cube_ply != PRESET_UNSET {
                preset.cube_ply as u32
            } else {
                preset.decision_ply as u32
            }),
        }),
        methods: None,
        configurable: false,
        supports_progress: true,
        supports_cancel: true,
    }
}

/// Map decoded match context onto the capi's per-call cube context.
/// jacoby comes from the match id; beaver mirrors the Python analyzer's
/// default (both are forced off in match play by the engine itself).
pub fn cube_ctx(context: &MatchContext) -> CubeCtx {
    CubeCtx {
        cube_value: context.cube_value as c_int,
        owner: match context.cube_owner {
            CubeOwner::Centered => OWNER_CENTERED,
            CubeOwner::Mover => OWNER_MOVER,
            CubeOwner::Opponent => OWNER_OPPONENT,
        },
        away1: context.away1() as c_int,
        away2: context.away2() as c_int,
        is_crawford: context.crawford as c_int,
        jacoby: context.jacoby as c_int,
        beaver: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plies_stamps_follow_the_dedup_identity() {
        assert_eq!(plies_stamp(levels::PLY_1), 1);
        assert_eq!(plies_stamp(levels::PLY_4), 4);
        assert_eq!(plies_stamp(levels::ROLLER_2T), 0);
        assert_eq!(plies_stamp(levels::ROLLOUT), 0);
    }

    #[test]
    fn rollout_options_reject_unknown_keys() {
        let value = serde_json::json!({"trials": 360, "bogus": 1});
        assert!(serde_json::from_value::<RolloutOptions>(value).is_err());
        let value = serde_json::json!({"trials": 360, "varianceReduction": false});
        let options: RolloutOptions = serde_json::from_value(value).unwrap();
        assert_eq!(options.trials, Some(360));
        assert_eq!(options.variance_reduction, Some(false));
    }

    #[test]
    fn cube_ctx_maps_owner_and_away_scores() {
        let context = MatchContext {
            cube_value: 2,
            cube_owner: CubeOwner::Opponent,
            mover: 0,
            crawford: false,
            jacoby: false,
            match_length: 7,
            mover_score: 3,
            opponent_score: 5,
            die1: 0,
            die2: 0,
            doubled: false,
            game_state: 1,
            resigned: 0,
            turn: 0,
        };
        let ctx = cube_ctx(&context);
        assert_eq!(ctx.owner, OWNER_OPPONENT);
        assert_eq!(ctx.away1, 4);
        assert_eq!(ctx.away2, 2);
        assert_eq!(ctx.jacoby, 0);
        assert_eq!(ctx.beaver, 1);
    }
}
