// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Customation AS
//! sage-engine-server — the Open Sage engine as a Backgammon Engine
//! Protocol daemon: JSON-RPC 2.0 over stdio, evaluations via
//! libbgsage_capi.
//!
//! Release layout next to the executable:
//!   models/  the stage9 weight files
//!   data/bearoff_1sided.db
//!   the engine library (bgsage_capi.dll / libbgsage_capi.so / .dylib)
//! Every path can be overridden; nothing is searched for implicitly
//! beyond this documented layout.

mod engine;
mod ffi;
mod mapping;
mod notation;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bep_protocol::contract::{error_codes, methods, CancelParams, EvaluateParams, ProgressParams};
use bep_protocol::gnubg_ids::{decode_match_id, decode_position_id};
use bep_protocol::jsonrpc::{self, codes, FrameSink, Incoming};
use serde_json::Value;

use engine::{cube_ctx, EngineGetError, EnginePool, LevelError, ProgressContext, WeightSet};
use ffi::Capi;
use mapping::MappingError;

const ENV_CAPI_LIB: &str = "BGSAGE_CAPI_LIB";

const FLAG_CAPI_LIB: &str = "--capi-lib";
const FLAG_WEIGHTS_DIR: &str = "--weights-dir";
const FLAG_BEAROFF_DB: &str = "--bearoff-db";
const FLAG_NO_BEAROFF_DB: &str = "--no-bearoff-db";
const FLAG_THREADS: &str = "--threads";
const FLAG_HELP: &str = "--help";

const DEFAULT_WEIGHTS_SUBDIR: &str = "models";
const DEFAULT_DATA_SUBDIR: &str = "data";

#[cfg(target_os = "windows")]
const CAPI_LIB_FILENAME: &str = "bgsage_capi.dll";
#[cfg(target_os = "macos")]
const CAPI_LIB_FILENAME: &str = "libbgsage_capi.dylib";
#[cfg(all(unix, not(target_os = "macos")))]
const CAPI_LIB_FILENAME: &str = "libbgsage_capi.so";

const BUILD_NAME: &str = concat!("sage-engine-server ", env!("CARGO_PKG_VERSION"));

const EXIT_CONFIG_ERROR: i32 = 2;

struct Args {
    capi_lib: Option<PathBuf>,
    weights_dir: Option<PathBuf>,
    bearoff_db: Option<PathBuf>,
    no_bearoff_db: bool,
    threads: i32,
}

fn usage() -> String {
    format!(
        "usage: sage-engine-server [{FLAG_CAPI_LIB} <path>] [{FLAG_WEIGHTS_DIR} <dir>] \
         [{FLAG_BEAROFF_DB} <path> | {FLAG_NO_BEAROFF_DB}] [{FLAG_THREADS} <n>]\n\
         Defaults: engine library {CAPI_LIB_FILENAME} next to the executable (or \
         ${ENV_CAPI_LIB}), weights in <exe dir>/{DEFAULT_WEIGHTS_SUBDIR}, bearoff DB \
         <exe dir>/{DEFAULT_DATA_SUBDIR}/{}",
        engine::BEAROFF_DB_FILENAME
    )
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        capi_lib: None,
        weights_dir: None,
        bearoff_db: None,
        no_bearoff_db: false,
        threads: 0,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value_for = |name: &str| {
            iter.next().ok_or_else(|| format!("{name} requires a value\n{}", usage()))
        };
        match flag.as_str() {
            FLAG_CAPI_LIB => args.capi_lib = Some(PathBuf::from(value_for(FLAG_CAPI_LIB)?)),
            FLAG_WEIGHTS_DIR => {
                args.weights_dir = Some(PathBuf::from(value_for(FLAG_WEIGHTS_DIR)?))
            }
            FLAG_BEAROFF_DB => args.bearoff_db = Some(PathBuf::from(value_for(FLAG_BEAROFF_DB)?)),
            FLAG_NO_BEAROFF_DB => args.no_bearoff_db = true,
            FLAG_THREADS => {
                args.threads = value_for(FLAG_THREADS)?
                    .parse()
                    .map_err(|_| format!("{FLAG_THREADS} must be an integer"))?
            }
            FLAG_HELP => return Err(usage()),
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }
    if args.no_bearoff_db && args.bearoff_db.is_some() {
        return Err(format!("{FLAG_BEAROFF_DB} and {FLAG_NO_BEAROFF_DB} are mutually exclusive"));
    }
    Ok(args)
}

fn exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot resolve executable path: {e}"))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "executable has no parent directory".to_string())
}

fn build_pool(args: &Args) -> Result<EnginePool, String> {
    let exe_dir = exe_dir()?;

    let lib_path = match &args.capi_lib {
        Some(path) => path.clone(),
        None => match std::env::var_os(ENV_CAPI_LIB) {
            Some(value) => PathBuf::from(value),
            None => exe_dir.join(CAPI_LIB_FILENAME),
        },
    };
    if !lib_path.is_file() {
        return Err(format!(
            "engine library not found: {} (set {FLAG_CAPI_LIB} or {ENV_CAPI_LIB})",
            lib_path.display()
        ));
    }
    let capi = Capi::load(&lib_path)
        .map_err(|e| format!("cannot load engine library {}: {e}", lib_path.display()))?;

    let weights_dir =
        args.weights_dir.clone().unwrap_or_else(|| exe_dir.join(DEFAULT_WEIGHTS_SUBDIR));
    let bearoff_db = if args.no_bearoff_db {
        None
    } else {
        Some(args.bearoff_db.clone().unwrap_or_else(|| {
            exe_dir.join(DEFAULT_DATA_SUBDIR).join(engine::BEAROFF_DB_FILENAME)
        }))
    };
    let weights = WeightSet::stage9(&weights_dir, bearoff_db.as_deref())?;

    eprintln!(
        "sage-engine-server {}: capi {} from {}, weights {}",
        env!("CARGO_PKG_VERSION"),
        capi.version(),
        lib_path.display(),
        weights_dir.display()
    );
    Ok(EnginePool::new(Arc::new(capi), weights, args.threads))
}

/// One evaluation in flight: the cancel flag is set immediately on a
/// cancel notification; the handle arrives once the worker resolved the
/// level (engine construction can take seconds on first use).
struct InFlight {
    cancelled: AtomicBool,
    handle: Mutex<Option<Arc<engine::EngineHandle>>>,
}

type InFlightMap = Arc<Mutex<HashMap<String, Arc<InFlight>>>>;

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn send_or_log<W: io::Write + Send>(sink: &FrameSink<W>, message: &Value) {
    if let Err(e) = sink.send(message) {
        // stdout gone means the host is gone; there is nobody left to
        // answer. Exit rather than spin.
        eprintln!("cannot write to stdout ({e}); exiting");
        std::process::exit(1);
    }
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(EXIT_CONFIG_ERROR);
        }
    };
    let pool = match build_pool(&args) {
        Ok(pool) => Arc::new(pool),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(EXIT_CONFIG_ERROR);
        }
    };

    let sink = FrameSink::new(io::stdout());
    let in_flight: InFlightMap = Arc::new(Mutex::new(HashMap::new()));

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    loop {
        let message = match jsonrpc::read_message(&mut reader) {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(parse_error))) => {
                send_or_log(
                    &sink,
                    &jsonrpc::error(None, codes::PARSE_ERROR, &parse_error.to_string()),
                );
                continue;
            }
            Ok(None) => break, // clean EOF: host closed stdin
            Err(io_error) => {
                eprintln!("stdin read failed: {io_error}");
                break;
            }
        };
        dispatch(message, &pool, &sink, &in_flight);
    }
}

fn dispatch(
    message: Incoming,
    pool: &Arc<EnginePool>,
    sink: &FrameSink<io::Stdout>,
    in_flight: &InFlightMap,
) {
    match message.method.as_str() {
        methods::DESCRIBE => {
            if let Some(id) = &message.id {
                let describe = pool.describe(BUILD_NAME);
                match serde_json::to_value(&describe) {
                    Ok(result) => send_or_log(sink, &jsonrpc::success(id, result)),
                    Err(e) => send_or_log(
                        sink,
                        &jsonrpc::error(Some(id), codes::INTERNAL_ERROR, &e.to_string()),
                    ),
                }
            }
        }
        methods::SHUTDOWN => {
            if let Some(id) = &message.id {
                send_or_log(sink, &jsonrpc::success(id, Value::Null));
            }
            // In-flight rollouts die with the process; the host asked.
            std::process::exit(0);
        }
        methods::CANCEL => {
            let params = message.params.unwrap_or(Value::Null);
            match serde_json::from_value::<CancelParams>(params) {
                Ok(cancel) => {
                    let entry = {
                        let map = in_flight.lock().unwrap_or_else(|p| p.into_inner());
                        map.get(&id_key(&cancel.id)).cloned()
                    };
                    match entry {
                        Some(entry) => {
                            entry.cancelled.store(true, Ordering::SeqCst);
                            let handle = entry.handle.lock().unwrap_or_else(|p| p.into_inner());
                            if let Some(handle) = handle.as_ref() {
                                handle.cancel();
                            }
                        }
                        None => eprintln!(
                            "cancel for unknown or completed request {}",
                            cancel.id
                        ),
                    }
                }
                Err(e) => eprintln!("malformed cancel notification: {e}"),
            }
        }
        methods::EVALUATE_POSITION
        | methods::EVALUATE_CUBE
        | methods::EVALUATE_MOVES
        | methods::ANALYZE_MOVE => {
            let Some(id) = message.id.clone() else {
                eprintln!("{} sent as a notification — ignored (no id to answer)", message.method);
                return;
            };
            let params = match serde_json::from_value::<EvaluateParams>(
                message.params.unwrap_or(Value::Null),
            ) {
                Ok(params) => params,
                Err(e) => {
                    send_or_log(
                        sink,
                        &jsonrpc::error(Some(&id), codes::INVALID_PARAMS, &e.to_string()),
                    );
                    return;
                }
            };

            let entry = Arc::new(InFlight {
                cancelled: AtomicBool::new(false),
                handle: Mutex::new(None),
            });
            {
                let mut map = in_flight.lock().unwrap_or_else(|p| p.into_inner());
                map.insert(id_key(&id), Arc::clone(&entry));
            }

            let pool = Arc::clone(pool);
            let sink = sink.clone();
            let in_flight = Arc::clone(in_flight);
            let method = message.method.clone();
            std::thread::spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    evaluate(&method, &params, &pool, &sink, &id, &entry)
                }));
                let response = match outcome {
                    Ok(response) => response,
                    Err(panic) => {
                        let detail = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "panic in evaluation thread".to_string());
                        eprintln!("evaluation panicked: {detail}");
                        jsonrpc::error(Some(&id), codes::INTERNAL_ERROR, &detail)
                    }
                };
                {
                    let mut map = in_flight.lock().unwrap_or_else(|p| p.into_inner());
                    map.remove(&id_key(&id));
                }
                send_or_log(&sink, &response);
            });
        }
        other => {
            if let Some(id) = &message.id {
                send_or_log(
                    sink,
                    &jsonrpc::error(
                        Some(id),
                        codes::METHOD_NOT_FOUND,
                        &format!("unknown method {other:?}"),
                    ),
                );
            }
            // Unknown notifications are ignorable by spec (§7).
        }
    }
}

fn evaluate(
    method: &str,
    params: &EvaluateParams,
    pool: &EnginePool,
    sink: &FrameSink<io::Stdout>,
    id: &Value,
    entry: &InFlight,
) -> Value {
    let board = match decode_position_id(&params.position_id) {
        Ok(board) => board,
        Err(e) => return jsonrpc::error(Some(id), error_codes::INVALID_ID, &e.to_string()),
    };
    let context = match decode_match_id(&params.match_id) {
        Ok(context) => context,
        Err(e) => return jsonrpc::error(Some(id), error_codes::INVALID_ID, &e.to_string()),
    };
    let cube = cube_ctx(&context);

    let handle = match pool.get(&params.level, method, params.level_options.as_ref()) {
        Ok(handle) => handle,
        Err(EngineGetError::Level(LevelError::UnknownLevel(level))) => {
            return jsonrpc::error(
                Some(id),
                error_codes::UNKNOWN_LEVEL,
                &format!("unknown level {level:?}"),
            )
        }
        Err(EngineGetError::Level(LevelError::InvalidOptions(message))) => {
            return jsonrpc::error(Some(id), codes::INVALID_PARAMS, &message)
        }
        Err(EngineGetError::Level(e @ LevelError::MethodNotSupported { .. })) => {
            return jsonrpc::error(Some(id), codes::INVALID_PARAMS, &e.to_string())
        }
        Err(EngineGetError::Create(e)) => {
            return jsonrpc::error(Some(id), error_codes::EVALUATION_FAILED, &e.to_string())
        }
    };

    {
        let mut slot = entry.handle.lock().unwrap_or_else(|p| p.into_inner());
        *slot = Some(Arc::clone(&handle));
    }
    if entry.cancelled.load(Ordering::SeqCst) {
        return jsonrpc::error(Some(id), error_codes::CANCELLED, "cancelled before evaluation");
    }

    let progress_context = if handle.supports_cancel {
        let progress_sink = sink.clone();
        let request_id = id.clone();
        Some(ProgressContext {
            emit: Box::new(move |done, total| {
                let params = ProgressParams {
                    id: request_id.clone(),
                    done: done.max(0) as u64,
                    total: total.max(0) as u64,
                };
                match serde_json::to_value(&params) {
                    Ok(value) => {
                        if let Err(e) = progress_sink
                            .send(&jsonrpc::notification(methods::PROGRESS, value))
                        {
                            eprintln!("cannot send progress notification: {e}");
                        }
                    }
                    Err(e) => eprintln!("cannot serialize progress notification: {e}"),
                }
            }),
        })
    } else {
        None
    };
    let progress = progress_context.as_ref();

    let result: Result<Value, MappingError> = match method {
        methods::EVALUATE_POSITION => {
            mapping::evaluate_position(&handle, &board, &cube, &params.position_id)
                .and_then(to_result_value)
        }
        methods::EVALUATE_CUBE => {
            mapping::evaluate_cube(&handle, &board, &cube, &params.position_id, progress)
                .and_then(to_result_value)
        }
        methods::EVALUATE_MOVES => require_dice(params).and_then(|(die1, die2)| {
            mapping::ranked_moves(
                &handle,
                &board,
                die1,
                die2,
                &cube,
                &params.position_id,
                &params.match_id,
                progress,
            )
            .map(mapping::moves_payload)
            .and_then(to_result_value)
        }),
        methods::ANALYZE_MOVE => require_dice(params).and_then(|(die1, die2)| {
            let played = params.played_move.as_deref().ok_or_else(|| {
                MappingError::Invalid("analyzeMove requires a move".to_string())
            })?;
            mapping::ranked_moves(
                &handle,
                &board,
                die1,
                die2,
                &cube,
                &params.position_id,
                &params.match_id,
                progress,
            )
            .and_then(|ranked| mapping::analyze_payload(ranked, &board, played))
            .and_then(to_result_value)
        }),
        other => {
            return jsonrpc::error(
                Some(id),
                codes::METHOD_NOT_FOUND,
                &format!("unknown method {other:?}"),
            )
        }
    };

    match result {
        Ok(value) => jsonrpc::success(id, value),
        Err(MappingError::Engine(e)) if e.is_cancelled() => {
            jsonrpc::error(Some(id), error_codes::CANCELLED, "evaluation cancelled")
        }
        Err(MappingError::Engine(e)) => {
            jsonrpc::error(Some(id), error_codes::EVALUATION_FAILED, &e.to_string())
        }
        Err(MappingError::Id(e)) => jsonrpc::error(Some(id), error_codes::INVALID_ID, &e.to_string()),
        Err(MappingError::Invalid(message)) => {
            jsonrpc::error(Some(id), codes::INVALID_PARAMS, &message)
        }
        Err(MappingError::Failed(message)) => {
            jsonrpc::error(Some(id), error_codes::EVALUATION_FAILED, &message)
        }
    }
}

fn require_dice(params: &EvaluateParams) -> Result<(i32, i32), MappingError> {
    match (params.die1, params.die2) {
        (Some(die1), Some(die2)) => Ok((die1, die2)),
        _ => Err(MappingError::Invalid("die1 and die2 are required".to_string())),
    }
}

fn to_result_value<T: serde::Serialize>(payload: T) -> Result<Value, MappingError> {
    serde_json::to_value(payload)
        .map_err(|e| MappingError::Failed(format!("cannot serialize result: {e}")))
}
