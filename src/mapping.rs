// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Customation AS
//! Engine results mapped onto the protocol's contract payloads.
//!
//! Semantics are the bgsage-worker's, ported: probabilities [0,1] with
//! NaN→0 / ±Inf→clamp at the source (logged, never silent), cubeless
//! equity [-1,1], cubeful [-3,3], MoveHints ranked best-first with
//! 1-based Rank, ErrorVsBest = best − this, dice canonicalized
//! Die1 <= Die2, notation in the gnubg evaluator's per-hop shape, played
//! moves identified by resulting board rather than by string.

use bep_protocol::contract::{
    cube_action, sanitize, CubeEvaluation, MoveAnalysis, MoveHint, MovesEvaluation,
    PositionEvaluation, CUBEFUL_MAX, CUBEFUL_MIN, EQUITY_MAX, EQUITY_MIN, PROB_MAX, PROB_MIN,
};
use bep_protocol::gnubg_ids::{position_id_storage_base64, Board, GnubgIdError};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::engine::{EngineHandle, ProgressContext};
use crate::ffi::{CapiError, CubeCtx, Move, Probs};
use crate::notation;

/// Max ranked alternatives read back from the engine. Far above any real
/// legal-move count; if it ever fills we say so instead of truncating
/// silently.
const MOVE_BUFFER_LEN: usize = 512;

#[derive(Debug)]
pub enum MappingError {
    Engine(CapiError),
    Id(GnubgIdError),
    /// Bad request inputs → INVALID_PARAMS.
    Invalid(String),
    /// Evaluation-side failure that isn't a capi error → EVALUATION_FAILED.
    Failed(String),
}

impl From<CapiError> for MappingError {
    fn from(e: CapiError) -> Self {
        MappingError::Engine(e)
    }
}

impl From<GnubgIdError> for MappingError {
    fn from(e: GnubgIdError) -> Self {
        MappingError::Id(e)
    }
}

fn sanitize_logged(value: f64, lo: f64, hi: f64, field: &str, position_id: &str) -> f64 {
    if value.is_nan() {
        eprintln!("bgsage returned NaN for {field} on position {position_id} — clamping to 0");
    } else if value.is_infinite() {
        eprintln!("bgsage returned {value} for {field} on position {position_id} — clamping");
    }
    sanitize(value, lo, hi)
}

fn prob_fields(probs: &Probs, position_id: &str) -> [f64; 5] {
    [
        sanitize_logged(probs.win, PROB_MIN, PROB_MAX, "WinProb", position_id),
        sanitize_logged(probs.win_gammon, PROB_MIN, PROB_MAX, "WinGammon", position_id),
        sanitize_logged(probs.win_backgammon, PROB_MIN, PROB_MAX, "WinBackgammon", position_id),
        sanitize_logged(probs.lose_gammon, PROB_MIN, PROB_MAX, "LoseGammon", position_id),
        sanitize_logged(probs.lose_backgammon, PROB_MIN, PROB_MAX, "LoseBackgammon", position_id),
    ]
}

pub fn evaluate_position(
    engine: &EngineHandle,
    board: &Board,
    cube: &CubeCtx,
    position_id: &str,
) -> Result<PositionEvaluation, MappingError> {
    let eval = engine.pre_roll(board, cube)?;
    let [win, win_gammon, win_backgammon, lose_gammon, lose_backgammon] =
        prob_fields(&eval.probs, position_id);
    Ok(PositionEvaluation {
        equity: sanitize_logged(eval.cubeless, EQUITY_MIN, EQUITY_MAX, "Equity", position_id),
        cubeful_equity: sanitize_logged(
            eval.cubeful,
            CUBEFUL_MIN,
            CUBEFUL_MAX,
            "CubefulEquity",
            position_id,
        ),
        win_prob: win,
        win_gammon,
        win_backgammon,
        lose_gammon,
        lose_backgammon,
    })
}

pub fn evaluate_cube(
    engine: &EngineHandle,
    board: &Board,
    cube: &CubeCtx,
    position_id: &str,
    progress: Option<&ProgressContext>,
) -> Result<CubeEvaluation, MappingError> {
    let result = engine.cube_action(board, cube, progress)?;

    let no_double =
        sanitize_logged(result.equity_nd, CUBEFUL_MIN, CUBEFUL_MAX, "NoDoubleEquity", position_id);
    let take =
        sanitize_logged(result.equity_dt, CUBEFUL_MIN, CUBEFUL_MAX, "TakeEquity", position_id);
    let drop =
        sanitize_logged(result.equity_dp, CUBEFUL_MIN, CUBEFUL_MAX, "DropEquity", position_id);

    // The engine already applies doubling legality (a mover who cannot
    // turn the cube gets should_double=0, should_take=1), so the worker's
    // may_double guard is subsumed: too-good — play on for the gammon
    // because cashing is worth less than the position — reduces to this.
    let should_double = result.should_double != 0;
    let should_take = result.should_take != 0;
    let too_good = !should_double && !should_take && no_double > drop;

    Ok(CubeEvaluation {
        recommended_action: if should_double { cube_action::DOUBLE } else { cube_action::NO_DOUBLE },
        no_double_equity: no_double,
        take_equity: take,
        drop_equity: drop,
        double_take_gain: take - no_double,
        double_drop_gain: drop - no_double,
        too_good_to_double: too_good,
        // bgsage's Janowski x is internal to the engine; not reported.
        cube_efficiency: None,
        // The pre-roll cubeless distribution IS the no-double branch's;
        // there is no separate double-take distribution to report.
        our_win_prob: Some(sanitize_logged(
            result.probs.win,
            PROB_MIN,
            PROB_MAX,
            "OurWinProb",
            position_id,
        )),
        our_gammon_prob: Some(sanitize_logged(
            result.probs.win_gammon,
            PROB_MIN,
            PROB_MAX,
            "OurGammonProb",
            position_id,
        )),
        our_backgammon_prob: Some(sanitize_logged(
            result.probs.win_backgammon,
            PROB_MIN,
            PROB_MAX,
            "OurBackgammonProb",
            position_id,
        )),
        opp_win_prob: None,
        opp_gammon_prob: None,
        opp_backgammon_prob: None,
    })
}

/// Ranked hints plus each hint's resulting board (for played-move
/// identification), from one checker-play evaluation.
pub struct RankedMoves {
    pub hints: Vec<MoveHint>,
    pub boards: Vec<Board>,
    pub die1: i32,
    pub die2: i32,
}

#[allow(clippy::too_many_arguments)]
pub fn ranked_moves(
    engine: &EngineHandle,
    board: &Board,
    die1: i32,
    die2: i32,
    cube: &CubeCtx,
    position_id: &str,
    match_id: &str,
    progress: Option<&ProgressContext>,
) -> Result<RankedMoves, MappingError> {
    if !(1..=6).contains(&die1) || !(1..=6).contains(&die2) {
        return Err(MappingError::Invalid("dice must be 1-6".to_string()));
    }

    let mut buffer = vec![Move::zeroed(); MOVE_BUFFER_LEN];
    let count = engine.checker_play(board, die1, die2, cube, &mut buffer, progress)?;
    if count == MOVE_BUFFER_LEN {
        eprintln!(
            "checker_play filled the {MOVE_BUFFER_LEN}-entry buffer on position \
             {position_id} dice {die1}{die2} — alternatives may be truncated"
        );
    }
    buffer.truncate(count);

    let (sorted_die1, sorted_die2) = if die1 <= die2 { (die1, die2) } else { (die2, die1) };
    let storage_id = position_id_storage_base64(position_id)?;
    let evaluated_utc = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|e| {
            eprintln!("failed to format EvaluatedUtc: {e}");
            String::new()
        });

    let best_equity = buffer.first().map_or(0.0, |m| {
        sanitize_logged(m.cubeful, EQUITY_MIN, EQUITY_MAX, "MoveEquity[best]", position_id)
    });

    let mut hints = Vec::with_capacity(buffer.len());
    let mut boards = Vec::with_capacity(buffer.len());
    for (index, entry) in buffer.iter().enumerate() {
        let equity =
            sanitize_logged(entry.cubeful, EQUITY_MIN, EQUITY_MAX, "MoveEquity", position_id);
        let mut result_board: Board = [0; 26];
        result_board.copy_from_slice(&entry.board);
        let move_notation = notation::normalized_notation(board, &result_board, die1, die2);
        let [win, win_gammon, win_backgammon, lose_gammon, lose_backgammon] =
            prob_fields(&entry.probs, position_id);
        hints.push(MoveHint {
            gnubg_position_id: storage_id.clone(),
            gnubg_match_id: match_id.to_string(),
            die1: sorted_die1,
            die2: sorted_die2,
            // Engine catalog ids are a platform notion; the host resolves
            // the engine descriptor. Same convention as the cloud worker.
            evaluation_engine_id: 0,
            plies: engine.plies_stamp,
            rank: (index + 1) as i32,
            move_notation,
            equity,
            error_vs_best: best_equity - equity,
            win_prob: win,
            win_gammon,
            win_backgammon,
            lose_gammon,
            lose_backgammon,
            evaluated_utc: evaluated_utc.clone(),
        });
        boards.push(result_board);
    }

    Ok(RankedMoves { hints, boards, die1: sorted_die1, die2: sorted_die2 })
}

pub fn moves_payload(ranked: RankedMoves) -> MovesEvaluation {
    MovesEvaluation { die1: ranked.die1, die2: ranked.die2, alternatives: ranked.hints }
}

/// Identify the played move by its resulting board; fall back to the
/// gnubg evaluator's normalized-string comparison when the notation can't
/// be applied; fall back to best when nothing matches (worker behavior).
pub fn analyze_payload(
    ranked: RankedMoves,
    board: &Board,
    played_notation: &str,
) -> Result<MoveAnalysis, MappingError> {
    let best = ranked
        .hints
        .first()
        .cloned()
        .ok_or_else(|| MappingError::Failed("no ranked alternatives".to_string()))?;

    let played = match notation::apply_move_notation(board, played_notation) {
        Ok(played_board) => ranked
            .boards
            .iter()
            .position(|candidate| *candidate == played_board)
            .map(|index| ranked.hints[index].clone()),
        Err(reason) => {
            eprintln!(
                "cannot apply played move {played_notation:?} ({reason}); falling back to \
                 string comparison"
            );
            let wanted = notation::comparable_notation(played_notation);
            ranked
                .hints
                .iter()
                .find(|hint| notation::comparable_notation(&hint.move_notation) == wanted)
                .cloned()
        }
    };

    Ok(MoveAnalysis { played: played.unwrap_or_else(|| best.clone()), best })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::CubeResult;

    fn cube_result(nd: f64, dt: f64, dp: f64, double: bool, take: bool) -> CubeResult {
        CubeResult {
            probs: Probs { win: 0.6, ..Probs::default() },
            cubeless: 0.2,
            equity_nd: nd,
            equity_dt: dt,
            equity_dp: dp,
            should_double: double as i32,
            should_take: take as i32,
            is_beaver: 0,
        }
    }

    // The cube mapping is pure given a CubeResult; exercise the derivation
    // directly (the engine call itself is covered by the E2E harness).
    fn derive(result: &CubeResult) -> (i32, bool) {
        let should_double = result.should_double != 0;
        let should_take = result.should_take != 0;
        let action = if should_double { cube_action::DOUBLE } else { cube_action::NO_DOUBLE };
        let too_good =
            !should_double && !should_take && result.equity_nd > result.equity_dp;
        (action, too_good)
    }

    #[test]
    fn cube_derivation_matches_worker_semantics() {
        // Clear double/take.
        assert_eq!(derive(&cube_result(0.4, 0.5, 1.0, true, true)), (cube_action::DOUBLE, false));
        // Too good: no double, opponent would pass, ND beats the cash.
        assert_eq!(
            derive(&cube_result(1.2, 1.6, 1.0, false, false)),
            (cube_action::NO_DOUBLE, true)
        );
        // Cannot double (engine reports no-double + take): never too good.
        assert_eq!(
            derive(&cube_result(1.2, 1.6, 1.0, false, true)),
            (cube_action::NO_DOUBLE, false)
        );
    }

    #[test]
    fn sanitize_logged_collapses_nan_and_clamps() {
        assert_eq!(sanitize_logged(f64::NAN, EQUITY_MIN, EQUITY_MAX, "t", "p"), 0.0);
        assert_eq!(
            sanitize_logged(f64::NEG_INFINITY, CUBEFUL_MIN, CUBEFUL_MAX, "t", "p"),
            CUBEFUL_MIN
        );
        assert_eq!(sanitize_logged(1.7, EQUITY_MIN, EQUITY_MAX, "t", "p"), 1.0);
    }
}
