#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
# Runs inside rust:1-bookworm with the customation workspace at /work.
# Builds libbgsage_capi.so (same flags as bgsage cpp/capi/CMakeLists.txt),
# builds sage-engine-server for Linux, runs the stdio E2E driver.
set -eu

BGSAGE=/work/bgsage
SERVER=/work/sage-engine-server
BUILD=$BGSAGE/build_capi_linux
LIB=$BUILD/libbgsage_capi.so

mkdir -p "$BUILD"
if [ ! -f "$LIB" ] || [ "$BGSAGE/cpp/src/capi.cpp" -nt "$LIB" ]; then
    echo "== building libbgsage_capi.so =="
    g++ -std=c++17 -O3 -ffast-math -funroll-loops -mavx2 -mfma -fPIC -shared \
        -DBGSAGE_CAPI_BUILD \
        -I"$BGSAGE/cpp/include" \
        "$BGSAGE/cpp/src/board.cpp" \
        "$BGSAGE/cpp/src/moves.cpp" \
        "$BGSAGE/cpp/src/strategy.cpp" \
        "$BGSAGE/cpp/src/pubeval.cpp" \
        "$BGSAGE/cpp/src/game.cpp" \
        "$BGSAGE/cpp/src/benchmark.cpp" \
        "$BGSAGE/cpp/src/encoding.cpp" \
        "$BGSAGE/cpp/src/neural_net.cpp" \
        "$BGSAGE/cpp/src/training.cpp" \
        "$BGSAGE/cpp/src/multipy.cpp" \
        "$BGSAGE/cpp/src/rollout.cpp" \
        "$BGSAGE/cpp/src/cube.cpp" \
        "$BGSAGE/cpp/src/cube_eval.cpp" \
        "$BGSAGE/cpp/src/match_equity.cpp" \
        "$BGSAGE/cpp/src/bearoff.cpp" \
        "$BGSAGE/cpp/src/cuda_nn_stub.cpp" \
        "$BGSAGE/cpp/src/capi.cpp" \
        -o "$LIB" -lpthread
fi
echo "== engine library: $(ls -la "$LIB" | awk '{print $5}') bytes =="

echo "== building sage-engine-server =="
cd "$SERVER"
export CARGO_TARGET_DIR=/work/sage-engine-server/target-linux
cargo build --release 2>&1 | tail -3

echo "== running E2E =="
python3 "$SERVER/tests/e2e/run_e2e.py" \
    "$CARGO_TARGET_DIR/release/sage-engine-server" \
    "$LIB" \
    "$BGSAGE/models" \
    "$BGSAGE/data/bearoff_1sided.db"
