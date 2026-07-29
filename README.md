# sage-engine-server

The [Open Sage](https://github.com/markbgsage/bgsage) backgammon engine
as a standalone analysis daemon: JSON-RPC 2.0 over stdin/stdout, speaking
the Backgammon Engine Protocol (see `PROTOCOL.md` in the protocol
repository). Any host that speaks the protocol — a desktop GUI, a server,
a pipeline — spawns this executable and gets full-strength Sage
evaluations, the same way chess GUIs talk to UCI engines.

Licensed AGPL-3.0-or-later, like the engine it wraps. The process
boundary is the licensing boundary: hosts talk to the daemon over stdio
and need not be AGPL.

## What it serves

Seven evaluation levels over the stage9 model (19 neural networks,
backgame-aware pair strategy) plus the one-sided bearoff database:

| Level | Kind | Notes |
|---|---|---|
| `1ply`–`4ply` | ply | XG ply convention: 1-ply = raw NN |
| `2T` | roller | 360 trials, truncation 7, 2-ply decisions |
| `3T` | roller_plus | 360 trials, truncation 7, 3-ply decisions |
| `rollout` | rollout | 1296 trials, full-length, VR; configurable per request (`levelOptions`: `trials`, `truncation`, `varianceReduction`, `seed`) |

Four methods per level: `evaluatePosition`, `evaluateCube`,
`evaluateMoves`, `analyzeMove`. Positions arrive as GnuBG Position IDs,
match context as GnuBG Match IDs. Rollout-class levels stream `progress`
notifications and honor `cancel`.

Result payloads follow the GammonBase evaluation contract (shared golden
fixtures live in the bgsage-worker repository, which runs the same
mapping semantics on a cloud queue): probabilities in [0,1] from the
on-roll player's perspective, cubeless equity clamped [-1,1], cubeful
[-3,3], move alternatives ranked best-first with gnubg-style notation.

## Layout

The daemon loads the engine core (`libbgsage_capi`, the C API from the
bgsage fork) at runtime. Default layout next to the executable, every
path overridable:

```
sage-engine-server[.exe]
libbgsage_capi.so | bgsage_capi.dll | libbgsage_capi.dylib
models/sl_s9_*.weights.best        (15 files)
data/bearoff_1sided.db
```

```
sage-engine-server [--capi-lib <path>] [--weights-dir <dir>]
                   [--bearoff-db <path> | --no-bearoff-db]
                   [--threads <n>]
```

`BGSAGE_CAPI_LIB` overrides the library path when the flag is absent.
Missing files fail fast at startup with a clear message — before the
first request, never during one.

## Testing

- `cargo test` — unit tests (notation port, contract mapping, level
  catalog); no engine library required.
- `tests/e2e/run_e2e.py` — spawns the built daemon and drives it over
  stdio: describe, all four methods, rollout with options + progress,
  error codes, shutdown. `tests/e2e/container_e2e.sh` runs the whole
  chain (engine `.so` build → daemon build → E2E) in a Linux container.
- Cross-engine parity against the Python `BgBotAnalyzer` is the separate
  parity harness (Python is a CI referee only; it never ships).
