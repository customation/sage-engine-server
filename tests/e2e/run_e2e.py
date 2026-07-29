# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Customation AS
"""End-to-end test: drive sage-engine-server over stdio like a host would.

Python is the CI referee only — it never ships. Asserts the same numbers
the C-shim smoke test proved against the Python analyzer:
  start position 1ply pre-roll: win=0.5276 eq=+0.0819 cf=+0.1045
  start position 3-1 best move: 8/5 6/5, equity +0.2148

Usage: run_e2e.py <server-binary> <capi-lib> <weights-dir> <bearoff-db>
"""

import json
import subprocess
import sys
import threading

STARTING_POSITION_ID = "4HPwATDgc/ABMA"
MONEY_MATCH_ID = "cAgAAAAAAAAA"
TOLERANCE = 5e-5  # asserted values are printed to 4 decimals

EXPECTED_START = {"WinProb": 0.5276, "Equity": 0.0819, "CubefulEquity": 0.1045}
EXPECTED_BEST_31 = {"MoveNotation": "8/5 6/5", "Equity": 0.2148}


class Client:
    def __init__(self, process):
        self.process = process
        self.next_id = 0
        self.responses = {}
        self.notifications = []
        self.condition = threading.Condition()
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    def _read_loop(self):
        stream = self.process.stdout
        while True:
            headers = {}
            line = stream.readline()
            if not line:
                return
            while line and line.strip():
                name, _, value = line.decode("ascii").partition(":")
                headers[name.strip().lower()] = value.strip()
                line = stream.readline()
            length = int(headers["content-length"])
            body = stream.read(length)
            message = json.loads(body)
            with self.condition:
                if "id" in message and ("result" in message or "error" in message):
                    self.responses[message["id"]] = message
                else:
                    self.notifications.append(message)
                self.condition.notify_all()

    def _send(self, payload):
        body = json.dumps(payload).encode("utf-8")
        frame = b"Content-Length: %d\r\n\r\n%b" % (len(body), body)
        self.process.stdin.write(frame)
        self.process.stdin.flush()

    def request(self, method, params=None, timeout=600):
        self.next_id += 1
        request_id = self.next_id
        payload = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            payload["params"] = params
        self._send(payload)
        with self.condition:
            if not self.condition.wait_for(
                lambda: request_id in self.responses, timeout=timeout
            ):
                raise TimeoutError(f"no response to {method} (id {request_id})")
            return self.responses.pop(request_id)

    def notify(self, method, params):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})


def expect(condition, message):
    if not condition:
        raise AssertionError(message)


def result_of(response):
    expect("error" not in response, f"unexpected error: {response.get('error')}")
    return response["result"]


def main():
    server, capi_lib, weights_dir, bearoff_db = sys.argv[1:5]
    process = subprocess.Popen(
        [
            server,
            "--capi-lib", capi_lib,
            "--weights-dir", weights_dir,
            "--bearoff-db", bearoff_db,
            "--threads", "2",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=sys.stderr.buffer,
    )
    client = Client(process)
    failures = []

    def check(name, fn):
        try:
            fn()
            print(f"PASS {name}")
        except Exception as ex:  # noqa: BLE001 — a referee reports, then re-raises at exit
            print(f"FAIL {name}: {ex}")
            failures.append(name)

    def base_params(level, **extra):
        return {
            "positionId": STARTING_POSITION_ID,
            "matchId": MONEY_MATCH_ID,
            "level": level,
            **extra,
        }

    # --- describe -----------------------------------------------------
    def describe():
        result = result_of(client.request("describe"))
        expect(result["protocolVersion"] == "0.1", f"protocolVersion {result}")
        expect(result["engine"]["family"] == "bgsage", "family")
        level_ids = [level["id"] for level in result["levels"]]
        expect(
            level_ids == ["1ply", "2ply", "3ply", "4ply", "2T", "3T", "rollout"],
            f"levels {level_ids}",
        )
        rollout = result["levels"][-1]
        expect(rollout["configurable"] is True, "rollout configurable")
        expect(rollout["rollout"]["trials"] == 1296, "rollout defaults")

    check("describe", describe)

    # --- evaluatePosition (1ply, parity numbers) ----------------------
    def evaluate_position():
        result = result_of(
            client.request("evaluatePosition", base_params("1ply"))
        )
        for field, wanted in EXPECTED_START.items():
            got = result[field]
            expect(
                abs(got - wanted) < TOLERANCE, f"{field}: got {got}, wanted {wanted}"
            )

    check("evaluatePosition 1ply parity", evaluate_position)

    # --- evaluateMoves (1ply 3-1, parity numbers) ---------------------
    def evaluate_moves():
        result = result_of(
            client.request("evaluateMoves", base_params("1ply", die1=3, die2=1))
        )
        expect(result["Die1"] == 1 and result["Die2"] == 3, "dice canonicalized")
        alternatives = result["Alternatives"]
        expect(len(alternatives) > 5, f"got {len(alternatives)} alternatives")
        best = alternatives[0]
        expect(best["Rank"] == 1, "rank 1-based")
        expect(
            best["MoveNotation"] == EXPECTED_BEST_31["MoveNotation"],
            f"best notation {best['MoveNotation']}",
        )
        expect(
            abs(best["Equity"] - EXPECTED_BEST_31["Equity"]) < TOLERANCE,
            f"best equity {best['Equity']}",
        )
        expect(best["ErrorVsBest"] == 0.0, "best ErrorVsBest")
        expect(best["Plies"] == 1, "plies stamp")
        expect(
            best["GnubgPositionId"] == "4HPwATDgc/ABMA==",
            f"storage id {best['GnubgPositionId']}",
        )
        second = alternatives[1]
        expect(second["ErrorVsBest"] > 0, "second worse than best")

    check("evaluateMoves 1ply 3-1 parity", evaluate_moves)

    # --- analyzeMove: played identified by board identity -------------
    def analyze_move():
        # Played move deliberately written in non-canonical hop order;
        # board identity must still find it (canonical: "24/23 13/10").
        result = result_of(
            client.request(
                "analyzeMove",
                base_params("1ply", die1=3, die2=1, move="13/10 24/23"),
            )
        )
        expect(
            result["Best"]["MoveNotation"] == "8/5 6/5",
            f"best {result['Best']['MoveNotation']}",
        )
        expect(
            result["Played"]["MoveNotation"] == "24/23 13/10",
            f"played {result['Played']['MoveNotation']}",
        )
        expect(result["Played"]["ErrorVsBest"] > 0, "played has positive error")

    check("analyzeMove board identity", analyze_move)

    # --- evaluateCube (1ply money) ------------------------------------
    def evaluate_cube():
        result = result_of(client.request("evaluateCube", base_params("1ply")))
        expect(result["RecommendedAction"] == 0, f"start is no-double: {result}")
        expect(result["TooGoodToDouble"] is False, "not too good")
        expect(result["DropEquity"] == 1.0, "DP normalized to +1")
        expect(result.get("OppWinProb") is None, "opp probs null")
        expect(0.5 < result.get("OurWinProb", 0) < 0.6, "our win prob present")

    check("evaluateCube 1ply", evaluate_cube)

    # --- configurable rollout with progress ---------------------------
    def rollout_progress():
        result = result_of(
            client.request(
                "evaluateCube",
                base_params("rollout", levelOptions={"trials": 36, "truncation": 5}),
                timeout=900,
            )
        )
        expect(
            0.4 < result["OurWinProb"] < 0.6, f"rollout win {result['OurWinProb']}"
        )
        expect(result["DropEquity"] == 1.0, "rollout DP normalized")

    check("rollout evaluateCube with levelOptions", rollout_progress)

    # --- error paths ---------------------------------------------------
    def error_paths():
        response = client.request("evaluatePosition", base_params("9ply"))
        expect(response["error"]["code"] == -32001, f"unknown level: {response}")
        response = client.request(
            "evaluatePosition",
            {"positionId": "invalid!", "matchId": MONEY_MATCH_ID, "level": "1ply"},
        )
        expect(response["error"]["code"] == -32002, f"invalid id: {response}")
        response = client.request("evaluateMoves", base_params("1ply"))
        expect(response["error"]["code"] == -32602, f"missing dice: {response}")
        response = client.request(
            "evaluatePosition", base_params("2ply", levelOptions={"trials": 8})
        )
        expect(response["error"]["code"] == -32602, f"options on fixed level: {response}")
        response = client.request("noSuchMethod")
        expect(response["error"]["code"] == -32601, f"unknown method: {response}")

    check("error codes", error_paths)

    # --- shutdown ------------------------------------------------------
    def shutdown():
        result_of(client.request("shutdown"))
        process.wait(timeout=30)
        expect(process.returncode == 0, f"exit code {process.returncode}")

    check("shutdown", shutdown)

    progress_count = sum(
        1 for note in client.notifications if note.get("method") == "progress"
    )
    print(f"progress notifications observed: {progress_count}")
    if progress_count == 0:
        failures.append("progress notifications")
        print("FAIL progress notifications: none observed during rollout")

    if failures:
        print(f"E2E FAILED: {failures}")
        sys.exit(1)
    print("E2E OK")


if __name__ == "__main__":
    main()
