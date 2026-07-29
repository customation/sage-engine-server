// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Customation AS
//! Raw ABI of libbgsage_capi plus a runtime loader.
//!
//! Struct layouts mirror bgsage's `cpp/include/bgbot/capi.h` field for
//! field; any change there must land here in the same commit. The library
//! is loaded at runtime so one daemon binary pairs with any compatible
//! engine build — the resolved path is explicit (flag > env > exe dir),
//! never a silent search.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::path::Path;

use libloading::{Library, Symbol};

pub const BOARD_SIZE: usize = 26;

pub const BGSAGE_OK: c_int = 0;
pub const BGSAGE_E_LOAD_FAILED: c_int = 2;
pub const BGSAGE_E_CANCELLED: c_int = 4;

pub const KIND_PLY: c_int = 1;
pub const KIND_ROLLOUT: c_int = 2;

pub const OWNER_CENTERED: c_int = 0;
pub const OWNER_MOVER: c_int = 1;
pub const OWNER_OPPONENT: c_int = 2;

pub const ERR_BUFFER_LEN: usize = 512;

/// Mirrors `bgsage_trial_eval`. All zeros = unset/inherit.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TrialEval {
    pub ply: c_int,
    pub rollout_trials: c_int,
    pub rollout_depth: c_int,
    pub rollout_ply: c_int,
}

/// Mirrors `bgsage_rollout_config`. Initialize via
/// [`Capi::rollout_config_defaults`], never by hand.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RolloutConfig {
    pub n_trials: c_int,
    pub truncation_depth: c_int,
    pub decision_ply: c_int,
    pub truncation_ply: c_int,
    pub late_ply: c_int,
    pub late_threshold: c_int,
    pub ultra_late_threshold: c_int,
    pub enable_vr: c_int,
    pub cubeful_trial_moves: c_int,
    pub cubeful_late_threshold: c_int,
    pub seed: c_uint,
    pub target_se: f64,
    pub max_batches: c_int,
    pub prefilter_threshold: f64,
    pub checker: TrialEval,
    pub checker_late: TrialEval,
    pub cube: TrialEval,
    pub cube_late: TrialEval,
}

/// Mirrors `bgsage_engine_config`. The pointed-to strings/arrays must
/// outlive the `bgsage_engine_create` call (see `engine::WeightSet`).
#[repr(C)]
pub struct EngineConfig {
    pub strategy_type: *const c_char,
    pub weight_paths: *const *const c_char,
    pub hidden_sizes: *const c_int,
    pub n_weights: c_int,
    pub bearoff_db_path: *const c_char,
    pub kind: c_int,
    pub n_plies: c_int,
    pub rollout: RolloutConfig,
    pub filter_max_moves: c_int,
    pub filter_threshold: f64,
    pub threads: c_int,
}

/// Mirrors `bgsage_cube_ctx`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CubeCtx {
    pub cube_value: c_int,
    pub owner: c_int,
    pub away1: c_int,
    pub away2: c_int,
    pub is_crawford: c_int,
    pub jacoby: c_int,
    pub beaver: c_int,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Probs {
    pub win: f64,
    pub win_gammon: f64,
    pub win_backgammon: f64,
    pub lose_gammon: f64,
    pub lose_backgammon: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Eval {
    pub probs: Probs,
    pub cubeless: f64,
    pub cubeful: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CubeResult {
    pub probs: Probs,
    pub cubeless: f64,
    pub equity_nd: f64,
    pub equity_dt: f64,
    pub equity_dp: f64,
    pub should_double: c_int,
    pub should_take: c_int,
    pub is_beaver: c_int,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Move {
    pub board: [c_int; BOARD_SIZE],
    pub probs: Probs,
    pub cubeless: f64,
    pub cubeful: f64,
    pub equity_diff: f64,
}

impl Move {
    /// All-numeric POD; a zeroed value is valid as an output buffer slot.
    pub fn zeroed() -> Move {
        unsafe { std::mem::zeroed() }
    }
}

pub type ProgressFn = unsafe extern "C" fn(user: *mut c_void, done: c_int, total: c_int);

/// Opaque `bgsage_engine`.
pub enum EngineOpaque {}

type FnRolloutConfigInit = unsafe extern "C" fn(*mut RolloutConfig);
type FnEngineCreate =
    unsafe extern "C" fn(*const EngineConfig, *mut c_char, usize) -> *mut EngineOpaque;
type FnEngineDestroy = unsafe extern "C" fn(*mut EngineOpaque);
type FnEvalCall = unsafe extern "C" fn(
    *mut EngineOpaque,
    *const c_int,
    *const CubeCtx,
    *mut Eval,
    *mut c_char,
    usize,
) -> c_int;
type FnCubeCall = unsafe extern "C" fn(
    *mut EngineOpaque,
    *const c_int,
    *const CubeCtx,
    *mut CubeResult,
    *mut c_char,
    usize,
) -> c_int;
type FnCheckerPlay = unsafe extern "C" fn(
    *mut EngineOpaque,
    *const c_int,
    c_int,
    c_int,
    *const CubeCtx,
    *mut Move,
    c_int,
    *mut c_int,
    *mut c_char,
    usize,
) -> c_int;
type FnSetProgress = unsafe extern "C" fn(*mut EngineOpaque, Option<ProgressFn>, *mut c_void);
type FnEngineOnly = unsafe extern "C" fn(*mut EngineOpaque);
type FnVersion = unsafe extern "C" fn() -> *const c_char;

/// A call into the engine library failed.
#[derive(Debug, Clone)]
pub struct CapiError {
    pub code: c_int,
    pub message: String,
}

impl CapiError {
    pub fn is_cancelled(&self) -> bool {
        self.code == BGSAGE_E_CANCELLED
    }
}

impl std::fmt::Display for CapiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bgsage_capi error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for CapiError {}

fn error_from(code: c_int, buffer: &[c_char; ERR_BUFFER_LEN]) -> CapiError {
    let message = unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    CapiError { code, message }
}

/// The loaded engine library. Symbols are looked up per call — lookup cost
/// is nanoseconds against evaluations measured in milliseconds to minutes,
/// and it keeps every unsafe block local and lifetime-simple.
pub struct Capi {
    lib: Library,
}

// SAFETY: the library handle itself is process-global state; per-engine
// call serialization is enforced by engine::EngineHandle's call lock.
unsafe impl Send for Capi {}
unsafe impl Sync for Capi {}

impl Capi {
    pub fn load(path: &Path) -> Result<Self, libloading::Error> {
        unsafe { Library::new(path).map(|lib| Capi { lib }) }
    }

    fn sym<'a, T>(&'a self, name: &[u8]) -> Symbol<'a, T> {
        // Missing symbols mean a library/daemon version mismatch — that is
        // a deployment defect, and failing loudly beats limping.
        unsafe {
            self.lib.get(name).unwrap_or_else(|e| {
                panic!(
                    "engine library is missing symbol {}: {e}",
                    String::from_utf8_lossy(name)
                )
            })
        }
    }

    pub fn version(&self) -> String {
        let f: Symbol<FnVersion> = self.sym(b"bgsage_capi_version");
        unsafe { CStr::from_ptr(f()).to_string_lossy().into_owned() }
    }

    pub fn rollout_config_defaults(&self) -> RolloutConfig {
        let f: Symbol<FnRolloutConfigInit> = self.sym(b"bgsage_rollout_config_init");
        let mut config = std::mem::MaybeUninit::<RolloutConfig>::uninit();
        unsafe {
            f(config.as_mut_ptr());
            config.assume_init()
        }
    }

    /// SAFETY: `config` and everything it points to must stay alive for
    /// the duration of the call.
    pub unsafe fn engine_create(
        &self,
        config: &EngineConfig,
    ) -> Result<*mut EngineOpaque, CapiError> {
        let f: Symbol<FnEngineCreate> = self.sym(b"bgsage_engine_create");
        let mut err = [0 as c_char; ERR_BUFFER_LEN];
        let engine = f(config, err.as_mut_ptr(), ERR_BUFFER_LEN);
        if engine.is_null() {
            Err(error_from(BGSAGE_E_LOAD_FAILED, &err))
        } else {
            Ok(engine)
        }
    }

    /// SAFETY: `engine` must be a live handle with no calls in flight.
    pub unsafe fn engine_destroy(&self, engine: *mut EngineOpaque) {
        let f: Symbol<FnEngineDestroy> = self.sym(b"bgsage_engine_destroy");
        f(engine);
    }

    /// SAFETY: caller serializes calls on `engine`.
    pub unsafe fn pre_roll(
        &self,
        engine: *mut EngineOpaque,
        board: &[c_int; BOARD_SIZE],
        cube: &CubeCtx,
    ) -> Result<Eval, CapiError> {
        let f: Symbol<FnEvalCall> = self.sym(b"bgsage_pre_roll");
        let mut out = Eval::default();
        let mut err = [0 as c_char; ERR_BUFFER_LEN];
        let code = f(engine, board.as_ptr(), cube, &mut out, err.as_mut_ptr(), ERR_BUFFER_LEN);
        if code == BGSAGE_OK {
            Ok(out)
        } else {
            Err(error_from(code, &err))
        }
    }

    /// SAFETY: caller serializes calls on `engine`.
    pub unsafe fn cube_action(
        &self,
        engine: *mut EngineOpaque,
        board: &[c_int; BOARD_SIZE],
        cube: &CubeCtx,
    ) -> Result<CubeResult, CapiError> {
        let f: Symbol<FnCubeCall> = self.sym(b"bgsage_cube_action");
        let mut out = CubeResult::default();
        let mut err = [0 as c_char; ERR_BUFFER_LEN];
        let code = f(engine, board.as_ptr(), cube, &mut out, err.as_mut_ptr(), ERR_BUFFER_LEN);
        if code == BGSAGE_OK {
            Ok(out)
        } else {
            Err(error_from(code, &err))
        }
    }

    /// SAFETY: caller serializes calls on `engine`.
    pub unsafe fn checker_play(
        &self,
        engine: *mut EngineOpaque,
        board: &[c_int; BOARD_SIZE],
        die1: c_int,
        die2: c_int,
        cube: &CubeCtx,
        buffer: &mut [Move],
    ) -> Result<usize, CapiError> {
        let f: Symbol<FnCheckerPlay> = self.sym(b"bgsage_checker_play");
        let mut n_out: c_int = 0;
        let mut err = [0 as c_char; ERR_BUFFER_LEN];
        let code = f(
            engine,
            board.as_ptr(),
            die1,
            die2,
            cube,
            buffer.as_mut_ptr(),
            buffer.len() as c_int,
            &mut n_out,
            err.as_mut_ptr(),
            ERR_BUFFER_LEN,
        );
        if code == BGSAGE_OK {
            Ok(n_out.max(0) as usize)
        } else {
            Err(error_from(code, &err))
        }
    }

    /// SAFETY: caller serializes calls on `engine`; `user` must stay valid
    /// until progress is cleared (set to `None`).
    pub unsafe fn set_progress(
        &self,
        engine: *mut EngineOpaque,
        callback: Option<ProgressFn>,
        user: *mut c_void,
    ) {
        let f: Symbol<FnSetProgress> = self.sym(b"bgsage_engine_set_progress");
        f(engine, callback, user);
    }

    /// Safe to call from any thread while an evaluation runs — this is the
    /// whole point of the cancel flag.
    pub fn cancel(&self, engine: *mut EngineOpaque) {
        let f: Symbol<FnEngineOnly> = self.sym(b"bgsage_engine_cancel");
        unsafe { f(engine) }
    }

    /// SAFETY: caller serializes calls on `engine`.
    pub unsafe fn reset_cancel(&self, engine: *mut EngineOpaque) {
        let f: Symbol<FnEngineOnly> = self.sym(b"bgsage_engine_reset_cancel");
        f(engine);
    }
}
