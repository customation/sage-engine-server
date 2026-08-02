# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Customation AS
"""Parity: sage-engine-server vs the cloud worker's mapping (BgSageEngine).

Both are AGPL implementations of the same evaluation contract over the
same C++ core — the cloud worker via the pybind bgbot_cpp module, the
daemon via libbgsage_capi. This suite runs identical requests through
both and requires field-level agreement, so a desktop-analyzed row and a
fleet-analyzed row are interchangeable downstream.

Python (analyzer + worker) is the CI referee only; it never ships.

Env: PARITY_SERVER_BIN, PARITY_CAPI_LIB, PARITY_WEIGHTS_DIR,
PARITY_BEAROFF_DB.
"""

import dataclasses
import json
import os
import subprocess
import sys
import threading
from pathlib import Path

import pytest

from bgsage_worker import contracts
from bgsage_worker.config import EngineCatalog
from bgsage_worker.engine import BgSageEngine
from bgsage_worker.gnubg_ids import decode_match_id, encode_match_id, encode_position_id

# 1-ply is a single NN pass — bit-stable across the two builds.
TOL_1PLY = 1e-6
# N-ply must match to the same precision — early ~1e-5 diffs were real
# code-path divergences (dead-cube vs cube-aware probs walk; missing
# PubEval move-selection prefilter in the cube recursion), both fixed in
# the capi. Do not loosen this to make a red run green: measured N-ply
# disagreement has so far always meant a wrong path, not float noise.
TOL_NPLY = 1e-6
TOL_ROLLOUT = 1e-4
# A discrete cube action is a threshold function of near-equal equities;
# when the double/no-double margin is inside this band, both answers are
# the same recommendation to a player and either side may flip.
ACTION_MARGIN = 1e-3


def tol_for(plies: int) -> float:
    return TOL_1PLY if plies <= 1 else TOL_NPLY

START_POSITION_ID = "4HPwATDgc/ABMA"
MONEY_MATCH_ID = "cAgAAAAAAAAA"

# A holding-game middlegame and a pure-race bearoff (both sides fully
# home), legal by construction: 15 checkers each side, no shared points.
HOLDING_BOARD = [0, 0, 0, 0, -2, 0, 4, 2, 2, 0, 0, 0, -4, 5, 0, 0, 0, 0, -3, -4, 2, 0, -2, 0, 0, 0]
BEAROFF_BOARD = (
    [0, 3, 4, 4, 2, 2] + [0] * 14 + [-3, -4, -4, -2, -2, 0]
)

# Cloud engine id 30 = bgsage ply family lane; Plies on the command
# selects the level. 32 = the fixed 2T roller.
ENGINE_PLY = 30
ENGINE_2T = 32
CATALOG_JSON = json.dumps({
    str(ENGINE_PLY): {"kind": "ply", "plies": 3, "timeoutSeconds": 600},
    str(ENGINE_2T): {"kind": "fixed", "level": "truncated2", "timeoutSeconds": 600},
})

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "e2e"))
from run_e2e import Client  # noqa: E402  (the stdio JSON-RPC test client)


def match_play_id() -> str:
    """7-point match, mover 4-away / opponent 6-away, cube 2 with mover."""
    context = decode_match_id(MONEY_MATCH_ID)
    context = dataclasses.replace(
        context,
        match_length=7,
        mover_score=3,
        opponent_score=1,
        cube_value=2,
        cube_owner="player",
    )
    return encode_match_id(context)


def command(cls, template: dict, **overrides):
    return cls.model_validate({**template, **overrides})


TEMPLATE = {
    "MessageId": "aaaaaaaa-1111-4111-8111-aaaaaaaa1111",
    "CorrelationId": "bbbbbbbb-2222-4222-8222-bbbbbbbb2222",
    "CausationId": "cccccccc-3333-4333-8333-cccccccc3333",
    "UserId": "dddddddd-4444-4444-8444-dddddddd4444",
    "Timestamp": "2026-07-13T10:20:30.123456Z",
    "TenantId": "eeeeeeee-5555-4555-8555-eeeeeeee5555",
    "MatchId": "ffffffff-6666-4666-8666-ffffffff6666",
    "GnubgPositionId": START_POSITION_ID,
    "GnubgMatchId": MONEY_MATCH_ID,
    "EngineId": ENGINE_PLY,
    "Plies": 1,
}


@pytest.fixture(scope="session")
def referee():
    catalog = EngineCatalog.from_json(CATALOG_JSON)
    return BgSageEngine(catalog, parallel_threads=1)


@pytest.fixture(scope="session")
def daemon():
    process = subprocess.Popen(
        [
            os.environ["PARITY_SERVER_BIN"],
            "--capi-lib", os.environ["PARITY_CAPI_LIB"],
            "--weights-dir", os.environ["PARITY_WEIGHTS_DIR"],
            "--bearoff-db", os.environ["PARITY_BEAROFF_DB"],
            "--threads", "1",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=sys.stderr.buffer,
    )
    client = Client(process)
    yield client
    client.request("shutdown", timeout=30)
    process.wait(timeout=30)


def near(a, b, tol, label=""):
    assert abs(a - b) <= tol, f"{label}: {a} vs {b} (tol {tol})"


def level_for(plies: int) -> str:
    return f"{plies}ply"


# A cube decision is only sound when both players have been evaluated an
# equal number of times, which in XG counting (1-ply = raw NN) lands on the
# ODD plies. sage-engine-server refuses evaluateCube on 2ply and 4ply, so a
# cube comparison at those depths tests a number no caller can obtain.
CUBE_PLIES = 3


POSITIONS = [
    ("start", START_POSITION_ID),
    ("holding", encode_position_id(HOLDING_BOARD)),
    ("bearoff", encode_position_id(BEAROFF_BOARD)),
]


@pytest.mark.parametrize("name,position_id", POSITIONS)
@pytest.mark.parametrize("plies", [1, 2])
def test_position_parity(referee, daemon, name, position_id, plies):
    event = referee.evaluate_position(command(
        contracts.EvaluatePosition, TEMPLATE, GnubgPositionId=position_id, Plies=plies))
    result = daemon.request("evaluatePosition", {
        "positionId": position_id, "matchId": MONEY_MATCH_ID, "level": level_for(plies)})["result"]
    tol = tol_for(plies)
    near(result["Equity"], event.equity, tol)
    near(result["CubefulEquity"], event.cubeful_equity, tol)
    for field, value in [
        ("WinProb", event.win_prob), ("WinGammon", event.win_gammon),
        ("WinBackgammon", event.win_backgammon), ("LoseGammon", event.lose_gammon),
        ("LoseBackgammon", event.lose_backgammon),
    ]:
        near(result[field], value, tol)


@pytest.mark.parametrize("name,position_id", POSITIONS)
def test_cube_parity(referee, daemon, name, position_id):
    event = referee.evaluate_cube(command(
        contracts.EvaluateCube, TEMPLATE, GnubgPositionId=position_id, Plies=CUBE_PLIES))
    result = daemon.request("evaluateCube", {
        "positionId": position_id, "matchId": MONEY_MATCH_ID,
        "level": level_for(CUBE_PLIES)})["result"]
    # Equities compare FIRST so an action mismatch still shows which
    # numbers moved it.
    near(result["NoDoubleEquity"], event.no_double_equity, TOL_NPLY, "NoDoubleEquity")
    near(result["TakeEquity"], event.take_equity, TOL_NPLY, "TakeEquity")
    near(result["DropEquity"], event.drop_equity, TOL_NPLY, "DropEquity")
    near(result["DoubleTakeGain"], event.double_take_gain, TOL_NPLY, "DoubleTakeGain")
    near(result["DoubleDropGain"], event.double_drop_gain, TOL_NPLY, "DoubleDropGain")
    near(result["OurWinProb"], event.our_win_prob, TOL_NPLY, "OurWinProb")
    near(result["OurGammonProb"], event.our_gammon_prob, TOL_NPLY, "OurGammonProb")
    near(result["OurBackgammonProb"], event.our_backgammon_prob, TOL_NPLY, "OurBackgammonProb")
    assert result.get("OppWinProb") is None and event.opp_win_prob is None
    margin = min(event.take_equity, event.drop_equity) - event.no_double_equity
    if abs(margin) > ACTION_MARGIN:
        assert result["RecommendedAction"] == event.recommended_action, (
            f"action {result['RecommendedAction']} vs {event.recommended_action} "
            f"(margin {margin})")
        assert result["TooGoodToDouble"] == event.too_good_to_double


MOVES_CASES = [
    (name, position_id, dice, 1)
    for name, position_id in POSITIONS
    for dice in [(3, 1), (6, 6)]
] + [("start", START_POSITION_ID, (3, 1), 2)]


@pytest.mark.parametrize("name,position_id,dice,plies", MOVES_CASES)
def test_moves_parity(referee, daemon, name, position_id, dice, plies):
    die1, die2 = dice
    event = referee.evaluate_moves(command(
        contracts.EvaluateMoves, TEMPLATE,
        GnubgPositionId=position_id, Plies=plies, Dice1=die1, Dice2=die2))
    result = daemon.request("evaluateMoves", {
        "positionId": position_id, "matchId": MONEY_MATCH_ID, "level": level_for(plies),
        "die1": die1, "die2": die2})["result"]
    assert result["Die1"] == event.die1 and result["Die2"] == event.die2
    assert len(result["Alternatives"]) == len(event.alternatives), (
        f"{len(result['Alternatives'])} vs {len(event.alternatives)} alternatives")
    tol = tol_for(plies)
    for got, want in zip(result["Alternatives"], event.alternatives):
        assert got["MoveNotation"] == want.move_notation
        assert got["Rank"] == want.rank
        assert got["Plies"] == want.plies
        assert got["GnubgPositionId"] == want.gnubg_position_id
        near(got["Equity"], want.equity, tol, f"alt{{want.rank}}.Equity")
        near(got["ErrorVsBest"], want.error_vs_best, tol, f"alt{{want.rank}}.ErrorVsBest")
        near(got["WinProb"], want.win_prob, tol, f"alt{{want.rank}}.WinProb")
        near(got["WinGammon"], want.win_gammon, tol, f"alt{{want.rank}}.WinGammon")
        near(got["WinBackgammon"], want.win_backgammon, tol, f"alt{{want.rank}}.WinBackgammon")
        near(got["LoseGammon"], want.lose_gammon, tol, f"alt{{want.rank}}.LoseGammon")
        near(got["LoseBackgammon"], want.lose_backgammon, tol, f"alt{{want.rank}}.LoseBackgammon")


def test_analyze_move_parity(referee, daemon):
    played = "24/23 13/10"
    event = referee.analyze_move(command(
        contracts.AnalyzeMove, TEMPLATE, Plies=1, Dice1=3, Dice2=1, Move=played))
    result = daemon.request("analyzeMove", {
        "positionId": START_POSITION_ID, "matchId": MONEY_MATCH_ID, "level": level_for(1),
        "die1": 3, "die2": 1, "move": played})["result"]
    assert result["Best"]["MoveNotation"] == event.best.move_notation
    assert result["Played"]["MoveNotation"] == event.played.move_notation
    assert result["Played"]["Rank"] == event.played.rank
    near(result["Played"]["Equity"], event.played.equity, TOL_1PLY)


def test_match_play_parity(referee, daemon):
    match_id = match_play_id()
    event = referee.evaluate_cube(command(
        contracts.EvaluateCube, TEMPLATE, GnubgMatchId=match_id, Plies=CUBE_PLIES))
    result = daemon.request("evaluateCube", {
        "positionId": START_POSITION_ID, "matchId": match_id,
        "level": level_for(CUBE_PLIES)})["result"]
    assert result["RecommendedAction"] == event.recommended_action
    near(result["NoDoubleEquity"], event.no_double_equity, TOL_NPLY)
    near(result["TakeEquity"], event.take_equity, TOL_NPLY)
    near(result["DropEquity"], event.drop_equity, TOL_NPLY)


def test_2t_cube_parity(referee, daemon):
    """The fixed 2T roller, single-threaded on both sides."""
    event = referee.evaluate_cube(command(
        contracts.EvaluateCube, TEMPLATE, EngineId=ENGINE_2T, Plies=0))
    result = daemon.request("evaluateCube", {
        "positionId": START_POSITION_ID, "matchId": MONEY_MATCH_ID, "level": "2T"},
        timeout=1800)["result"]
    assert result["RecommendedAction"] == event.recommended_action
    near(result["NoDoubleEquity"], event.no_double_equity, TOL_ROLLOUT)
    near(result["TakeEquity"], event.take_equity, TOL_ROLLOUT)
    near(result["OurWinProb"], event.our_win_prob, TOL_ROLLOUT)
