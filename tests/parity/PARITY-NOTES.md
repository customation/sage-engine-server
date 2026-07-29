# Parity investigation — 2026-07-18 — CLOSED: 19/19 GREEN

Final state: all 19 cases pass (1e-6 ply / 1e-4 rollout) after the fixes
below. This file stays as the investigation record; the suite itself is
the living artifact.

## RESOLVED (pending final green run)

Failure 1 (bearoff cube action) was a REAL PROD BUG IN THE WORKER, not
the daemon: all 8 equity/prob fields matched at 1e-6, but the worker's
`may_double` guard calls `can_double_match(away1=0, away2=0, ...)` whose
dead-cube test (`away1 <= cube_value`) reads money games as dead cubes →
the LIVE bgsage worker could never recommend Double in any money game
(match play unaffected; invisible because typical early positions are
genuinely NoDouble). Fixed in bgsage-worker/src/bgsage_worker/engine.py
(money → ownership-only guard); 63 worker tests green; worker needs
commit+deploy on Sven's go-ahead. The daemon was right.

Failure 2 (2-ply moves WinProb ~5e-4): capi checker N-ply block replaced
with the analyzer-parity flow — one `cubeful_probs_and_equity_nply`
traversal per candidate (flipped frame, single-threaded), sort by
cubeful; the old batch_checker_play-lambda port (dead-cube multipy probs
for survivors) was the wrong reference (the worker actually runs
_CubefulAnalyzer.checker_play_analytics' use_cube_aware_probs branch).


Suite: `test_parity.py` (daemon vs bgsage-worker BgSageEngine, container
`parity_container.sh`, rust:1-trixie). Tolerances: 1e-6 ply, 1e-4 rollout.

## Historical status (mid-investigation): 17/19

PASSING: all 1-ply (position/cube/moves/analyze, exact), all 2-ply
positionEvaluated (exact — fixed by routing capi post_move N≥2 through
`cubeful_probs_nply` in the flipped frame, mirroring
`analyzer.post_move_analytics`), cube 2-ply on start+holding (action fixed
by passing the static PubEval `move_filter` to `cube_decision_nply`,
mirroring `cube_decision_nply_unified`), match-play cube, 2T rollout cube
(1e-4, single-threaded both sides).

## Remaining failure 1: test_cube_parity[bearoff]

Daemon OurWinProb 0.49531614 vs referee 0.49479431 (~5e-4). Equities may
also differ slightly. The referee's probs path (binding
`cube_decision_nply_unified` lines ~5482-5493) builds a TEMP
`MultiPlyStrategy(base_strat, n_plies, filter, false, n_threads>1,
n_threads)` + `set_bearoff_db`, NO move prefilter, then
`evaluate_probs(flipped, flipped)` + invert. capi cube path reuses
`engine->multipy`, which (capi.cpp ~192-201) HAS
`set_move_prefilter(PubEval)` wired ("exactly as create_multipy wires
it"). Two hypotheses, check in order:
1. The PubEval move prefilter on engine->multipy changes interior picks →
   diff values. Fix: cube path must use a multipy WITHOUT the prefilter
   (temp instance like the binding, or a second cached `multipy_plain`).
2. RULED OUT: multipy.cpp lines 444/506 short-circuit at the root when
   the DB is set, and capi wires the DB (capi.cpp ~200). The failing
   value (~0.495 on a dead-even race) is therefore most likely
   NoDoubleEquity from cube_decision_nply itself, NOT OurWinProb —
   the next run's labeled asserts (near() now takes a field label)
   will say definitively. If it IS the recursion: suspect shared
   thread_local PosCache / cubeful-eval-cache state differences from
   evaluation ORDER across the session (the analyzer clears
   _strategy_nply after every cube call; check what the daemon
   clears and WHEN, and whether capi's extra 1-ply pre-eval at
   capi.cpp ~622 or the prefiltered engine->multipy at ~636 writes
   cache state the binding's fresh temp multipy would not see).
   Also compare: binding's temp multipy has NO move prefilter;
   engine->multipy HAS PubEval prefilter (capi.cpp ~197-199) —
   if the failing field is OurWinProb on a NON-bearoff position
   that prefilter is the diff; for bearoff the root short-circuit
   makes it moot.

## Remaining failure 2: test_moves_parity[start, 3-1, 2ply]

capi checker N-ply flow (capi.cpp ~384-423, ported from the
batch_checker_play N-ply lambda) does NOT match the worker's actual path.
The worker uses `_CubefulAnalyzer.checker_play_analytics` with
inner=_MultiPlyAnalyzer (analyzer.py ~762+, `use_cube_aware_probs`
branch ~850-900):
- inner (cubeless multipy) builds the ranked list (1-ply filter + N-ply
  rescore — read `_MultiPlyAnalyzer.checker_play_analytics` for exact
  chain before porting),
- then for EVERY candidate (not just survivors):
  `cubeful_probs_and_equity_nply(flip(m.board), flipped-owner,
  strategy_1ply, cubeful_ply, n_threads=1, swapped aways, jacoby,
  beaver, bearoff_db)` → probs = invert(r.probs), cubeless from those
  probs, cubeful = -r.equity — ONE cube-aware traversal supplies all
  three; capi instead used dead-cube multipy probs for survivors only +
  separate compute_cubeful_nply,
- sort by cubeful desc, then a promotion loop (analyzer.py ~915+,
  `_nply_eval`) — read and port it exactly.
Check whether the `cubeful_probs_and_equity_nply` binding passes any move
prefilter (the 5340 unified binding) — port args exactly.

## Method

Fix capi.cpp (bgsage clone, branch `capi`, uncommitted), then rerun:
  MSYS_NO_PATHCONV=1 docker run --rm -v "C:\git\github\customation:/work" \
    rust:1-trixie sh /work/sage-engine-server/tests/parity/parity_container.sh
Do NOT loosen tolerances — every N-ply mismatch so far was a real
code-path divergence, not float noise.
