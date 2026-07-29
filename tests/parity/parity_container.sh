#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
# Runs inside rust:1-trixie with the customation workspace at /work.
# Parity: the daemon (libbgsage_capi) vs the cloud worker's mapping
# (BgSageEngine over the pybind bgbot_cpp module), both built from the
# SAME local bgsage tree.
set -eu

echo "== building engine .so + daemon (via the E2E script) =="
sh /work/sage-engine-server/tests/e2e/container_e2e.sh

echo "== installing the Python referee (bgsage + bgsage-worker) =="
apt-get update -qq >/dev/null && apt-get install -y -qq python3-pip python3-venv python3-dev >/dev/null
python3 -m venv /tmp/parity-venv
. /tmp/parity-venv/bin/activate
pip install --quiet /work/bgsage
pip install --quiet -e /work/bgsage-worker pytest

echo "== running parity suite =="
export PARITY_SERVER_BIN=/work/sage-engine-server/target-linux/release/sage-engine-server
export PARITY_CAPI_LIB=/work/bgsage/build_capi_linux/libbgsage_capi.so
export PARITY_WEIGHTS_DIR=/work/bgsage/models
export PARITY_BEAROFF_DB=/work/bgsage/data/bearoff_1sided.db
python -m pytest /work/sage-engine-server/tests/parity/test_parity.py -q
