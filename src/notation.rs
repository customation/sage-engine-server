// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Customation AS
//! Move notation, ported from bgsage-worker / bgsage.
//!
//! `normalized_notation` is `bgsage.text_export.compute_move_notation`
//! composed with the worker's `normalize_notation`: since normalize only
//! expands what compute merges (repeat groups) and strips hit stars, the
//! composition reduces to emitting the sorted hop list directly — one
//! "from/to" token per hop, lowercase `bar`/`off`, no stars, no grouping.
//! The hop DERIVATION (die pairing, leftover pairing, hit-aware splits)
//! is a faithful port of compute_move_notation, quirks included, because
//! parity with the cloud worker's strings is the acceptance criterion.
//!
//! `apply_move_notation` is the worker's board-identity matcher: apply a
//! played move (any notation style) to the pre-move board and identify it
//! among ranked alternatives by the resulting position.

use bep_protocol::gnubg_ids::Board;

/// bgsage board indices: 25 = mover's bar, 0 = opponent's bar.
const MOVER_BAR: usize = 25;
const OPPONENT_BAR: usize = 0;

pub const BAR_TOKEN: &str = "bar";
pub const OFF_TOKEN: &str = "off";

/// In hop coordinates: 25 = bar (source only), 0 = off (target only).
const BAR_POINT: i32 = 25;
const OFF_POINT: i32 = 0;

fn point_label(value: i32, bar_or_off: &'static str) -> String {
    if value == BAR_POINT || value == OFF_POINT {
        bar_or_off.to_string()
    } else {
        value.to_string()
    }
}

/// gnubg-evaluator-shape notation for the move that turns `before` into
/// `after` with the given dice, from the mover's perspective.
pub fn normalized_notation(before: &Board, after: &Board, die1: i32, die2: i32) -> String {
    let mut hit_points: Vec<i32> = Vec::new();
    for i in 1..=24usize {
        if before[i] < 0 && (after[i] >= 0 || after[i] > before[i]) {
            hit_points.push(i as i32);
        }
    }

    let mut from_pts: Vec<i32> = Vec::new();
    let mut to_pts: Vec<i32> = Vec::new();

    let bar_diff = after[MOVER_BAR] - before[MOVER_BAR];
    for _ in 0..(-bar_diff).max(0) {
        from_pts.push(BAR_POINT);
    }

    for i in 1..=24usize {
        let mut wb = if before[i] > 0 { before[i] } else { 0 };
        let mut wa = if after[i] > 0 { after[i] } else { 0 };
        if before[i] < 0 && after[i] > 0 {
            wa = after[i];
            wb = 0;
        } else if before[i] > 0 && after[i] < 0 {
            wb = before[i];
            wa = 0;
        }
        let diff = wa - wb;
        for _ in 0..diff.max(0) {
            to_pts.push(i as i32);
        }
        for _ in 0..(-diff).max(0) {
            from_pts.push(i as i32);
        }
    }

    let on_board = |b: &Board| b[MOVER_BAR] + b[1..25].iter().filter(|v| **v > 0).sum::<i32>();
    let borne_off = on_board(before) - on_board(after);
    for _ in 0..borne_off.max(0) {
        to_pts.push(OFF_POINT);
    }

    from_pts.sort_unstable_by(|a, b| b.cmp(a));
    to_pts.sort_unstable_by(|a, b| b.cmp(a));

    let dice: Vec<i32> = if die1 == die2 {
        vec![die1; 4]
    } else {
        vec![die1, die2]
    };

    // (from, to) hops. Hits are tracked only to drive the split logic —
    // the normalized output carries no stars.
    let mut moves: Vec<(i32, i32)> = Vec::new();
    let mut used_from = vec![false; from_pts.len()];
    let mut used_to = vec![false; to_pts.len()];
    let mut used_die = vec![false; dice.len()];

    let take_hit = |hits: &mut Vec<i32>, point: i32| -> bool {
        if let Some(index) = hits.iter().position(|h| *h == point) {
            hits.remove(index);
            true
        } else {
            false
        }
    };

    for (di, d) in dice.iter().copied().enumerate() {
        if used_die[di] {
            continue;
        }
        for (fi, f) in from_pts.iter().copied().enumerate() {
            if used_from[fi] {
                continue;
            }
            let expected = if f == BAR_POINT { 25 - d } else { f - d };
            for (ti, t) in to_pts.iter().copied().enumerate() {
                if used_to[ti] {
                    continue;
                }
                if t == expected || (expected <= 0 && t == OFF_POINT) {
                    take_hit(&mut hit_points, t);
                    moves.push((f, t));
                    used_from[fi] = true;
                    used_to[ti] = true;
                    used_die[di] = true;
                    break;
                }
                // Upstream quirk preserved verbatim: after this die has
                // matched an earlier source, later sources scan only to
                // their first unmatched target — and may re-consume the
                // die if that target happens to match. Leftover pairing
                // below absorbs whatever this leaves unpaired.
                if used_die[di] {
                    break;
                }
            }
        }
    }

    for (fi, f) in from_pts.iter().copied().enumerate() {
        if used_from[fi] {
            continue;
        }
        for (ti, t) in to_pts.iter().copied().enumerate() {
            if used_to[ti] {
                continue;
            }
            take_hit(&mut hit_points, t);
            moves.push((f, t));
            used_from[fi] = true;
            used_to[ti] = true;
            break;
        }
    }

    // Split moves spanning multiple dice through intermediate HIT points:
    // 24/20 with 2-2 over a blot on 22 reads "24/22 22/20".
    if die1 == die2 {
        let die = die1;
        let mut mi = moves.len();
        while mi > 0 {
            mi -= 1;
            let (f, t) = moves[mi];
            let dist = if f == BAR_POINT { 25 - t } else { f - t };
            if dist <= die || dist % die != 0 {
                continue;
            }
            let n_dice = dist / die;
            let mut hit_mids: Vec<i32> = Vec::new();
            for i in 1..n_dice {
                let mid = if f == BAR_POINT { 25 - i * die } else { f - i * die };
                if (1..=24).contains(&mid) && hit_points.contains(&mid) {
                    hit_mids.push(mid);
                }
            }
            if hit_mids.is_empty() {
                continue;
            }
            for mid in &hit_mids {
                take_hit(&mut hit_points, *mid);
            }
            let mut sub_moves: Vec<(i32, i32)> = Vec::new();
            let mut prev = f;
            for mid in hit_mids {
                sub_moves.push((prev, mid));
                prev = mid;
            }
            sub_moves.push((prev, t));
            moves.splice(mi..mi + 1, sub_moves);
        }
    } else {
        let mut mi = moves.len();
        while mi > 0 {
            mi -= 1;
            let (f, t) = moves[mi];
            let dist = if f == BAR_POINT { 25 - t } else { f - t };
            if dist != die1 + die2 {
                continue;
            }
            for d1 in [die1, die2] {
                let mid = if f == BAR_POINT { 25 - d1 } else { f - d1 };
                if (1..=24).contains(&mid) && take_hit(&mut hit_points, mid) {
                    moves.splice(mi..mi + 1, [(f, mid), (mid, t)]);
                    break;
                }
            }
        }
    }

    moves.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)));

    moves
        .iter()
        .map(|(f, t)| format!("{}/{}", point_label(*f, BAR_TOKEN), point_label(*t, OFF_TOKEN)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// One parsed token: hop pairs plus a repeat count. Accepts chains
/// ("24/22*/16"), hit stars, "(n)" grouping and any case.
fn parse_token(token: &str) -> Result<(Vec<(String, String)>, u32), String> {
    let lowered = token.to_ascii_lowercase();
    let (chain, count) = match lowered.split_once('(') {
        Some((chain, rest)) => {
            let digits = rest
                .strip_suffix(')')
                .ok_or_else(|| format!("Unparseable move token {token:?}"))?;
            let count: u32 = digits
                .parse()
                .map_err(|_| format!("Unparseable move token {token:?}"))?;
            (chain, count)
        }
        None => (lowered.as_str(), 1),
    };

    let segments: Vec<String> = chain
        .split('/')
        .map(|segment| segment.trim_end_matches('*').to_string())
        .collect();
    if segments.len() < 2 {
        return Err(format!("Unparseable move token {token:?}"));
    }
    let valid_segment = |segment: &str, allow_bar: bool, allow_off: bool| {
        (allow_bar && segment == BAR_TOKEN)
            || (allow_off && segment == OFF_TOKEN)
            || (!segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()))
    };
    if !valid_segment(&segments[0], true, false)
        || !segments[1..].iter().all(|s| valid_segment(s, false, true))
    {
        return Err(format!("Unparseable move token {token:?}"));
    }
    let hops = segments.windows(2).map(|w| (w[0].clone(), w[1].clone())).collect();
    Ok((hops, count))
}

/// Apply a move in gnubg-style notation to a mover-perspective board.
pub fn apply_move_notation(board: &Board, notation: &str) -> Result<Board, String> {
    let mut result: Board = *board;
    for token in notation.split_whitespace() {
        let (hops, count) = parse_token(token)?;
        for _ in 0..count {
            for (source_text, target_text) in &hops {
                if source_text == BAR_TOKEN {
                    if result[MOVER_BAR] <= 0 {
                        return Err(format!("Move {notation:?} enters from an empty bar"));
                    }
                    result[MOVER_BAR] -= 1;
                } else {
                    let source: usize = source_text
                        .parse()
                        .map_err(|_| format!("Unparseable move token {token:?}"))?;
                    if !(1..=24).contains(&source) || result[source] <= 0 {
                        return Err(format!("Move {notation:?} lifts from empty point {source}"));
                    }
                    result[source] -= 1;
                }

                if target_text == OFF_TOKEN {
                    continue;
                }
                let target: usize = target_text
                    .parse()
                    .map_err(|_| format!("Unparseable move token {token:?}"))?;
                if !(1..=24).contains(&target) {
                    return Err(format!("Move {notation:?} lands outside the board"));
                }
                if result[target] < -1 {
                    return Err(format!("Move {notation:?} lands on a made opposing point"));
                }
                if result[target] == -1 {
                    result[target] = 0;
                    result[OPPONENT_BAR] += 1;
                }
                result[target] += 1;
            }
        }
    }
    Ok(result)
}

/// The gnubg evaluator's notation comparison collapse (fallback matching).
pub fn comparable_notation(notation: &str) -> String {
    notation
        .chars()
        .filter(|c| *c != ' ' && *c != '*')
        .collect::<String>()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    const STARTING_BOARD: Board = [
        0, -2, 0, 0, 0, 0, 5, 0, 3, 0, 0, 0, -5, 5, 0, 0, 0, -3, 0, -5, 0, 0, 0, 0, 2, 0,
    ];

    fn board_after(notation: &str) -> Board {
        apply_move_notation(&STARTING_BOARD, notation).unwrap()
    }

    #[test]
    fn make_the_five_point() {
        let after = board_after("8/5 6/5");
        assert_eq!(after[8], 2);
        assert_eq!(after[6], 4);
        assert_eq!(after[5], 2);
        assert_eq!(normalized_notation(&STARTING_BOARD, &after, 3, 1), "8/5 6/5");
        assert_eq!(normalized_notation(&STARTING_BOARD, &after, 1, 3), "8/5 6/5");
    }

    #[test]
    fn doubles_expand_per_hop() {
        let after = board_after("24/22 24/22 13/11 13/11");
        assert_eq!(
            normalized_notation(&STARTING_BOARD, &after, 2, 2),
            "24/22 24/22 13/11 13/11"
        );
    }

    #[test]
    fn combined_dice_move_stays_merged_without_intermediate_hit() {
        // 24/18 with 4-2 over an empty 20/22: single merged hop.
        let after = board_after("24/18");
        assert_eq!(normalized_notation(&STARTING_BOARD, &after, 4, 2), "24/18");
    }

    #[test]
    fn combined_dice_move_splits_through_a_hit() {
        // Opponent blot on 20; 24/18 with 4-2 must read 24/20 20/18.
        let mut before = STARTING_BOARD;
        before[19] = -4;
        before[20] = -1;
        let mut after = apply_move_notation(&before, "24/20*").unwrap();
        after = apply_move_notation(&after, "20/18").unwrap();
        assert_eq!(after[OPPONENT_BAR], 1);
        assert_eq!(normalized_notation(&before, &after, 4, 2), "24/20 20/18");
    }

    #[test]
    fn bar_entry_with_hit() {
        let mut before = STARTING_BOARD;
        // Put a mover checker on the bar and an opponent blot on 22.
        before[24] = 1;
        before[MOVER_BAR] = 1;
        before[17] = -2;
        before[22] = -1;
        let after = apply_move_notation(&before, "bar/22* 13/10").unwrap();
        assert_eq!(after[OPPONENT_BAR], 1);
        assert_eq!(after[22], 1);
        assert_eq!(normalized_notation(&before, &after, 3, 3), "bar/22 13/10");
    }

    #[test]
    fn bear_off_tokens() {
        let mut before: Board = [0; 26];
        before[6] = 2;
        before[5] = 1;
        before[1] = -2; // opponent still anchored — irrelevant to bearing off
        let after = apply_move_notation(&before, "6/off 6/off").unwrap();
        assert_eq!(normalized_notation(&before, &after, 6, 6), "6/off 6/off");
    }

    #[test]
    fn apply_expands_groups_chains_and_stars() {
        let grouped = apply_move_notation(&STARTING_BOARD, "13/11(2)").unwrap();
        let explicit = apply_move_notation(&STARTING_BOARD, "13/11 13/11").unwrap();
        assert_eq!(grouped, explicit);

        let mut with_blot = STARTING_BOARD;
        with_blot[19] = -4;
        with_blot[22] = -1;
        let chained = apply_move_notation(&with_blot, "24/22*/20").unwrap();
        let stepwise = {
            let mid = apply_move_notation(&with_blot, "24/22*").unwrap();
            apply_move_notation(&mid, "22/20").unwrap()
        };
        assert_eq!(chained, stepwise);
    }

    #[test]
    fn apply_rejects_illegal_moves() {
        assert!(apply_move_notation(&STARTING_BOARD, "bar/20").is_err());
        assert!(apply_move_notation(&STARTING_BOARD, "3/1").is_err());
        assert!(apply_move_notation(&STARTING_BOARD, "13/12").is_err()); // 12 is a made opposing point
        assert!(apply_move_notation(&STARTING_BOARD, "garbage").is_err());
    }

    #[test]
    fn comparable_collapse() {
        assert_eq!(comparable_notation("Bar/22* 13/10"), "bar/2213/10");
    }

    #[test]
    fn board_size_matches_protocol() {
        assert_eq!(bep_protocol::gnubg_ids::BOARD_SIZE, 26);
    }
}
