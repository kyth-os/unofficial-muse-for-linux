#!/usr/bin/env bash
# Offline account gates: stdlib unittest with fakes, no network or credentials.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PYTHONDONTWRITEBYTECODE=1
shopt -s globstar nullglob
for script in "$ROOT"/scripts/**/*.sh; do
    bash -n "$script"
done
/usr/bin/python3 - <<'PY'
from pathlib import Path

scripts = sorted(Path("scripts").rglob("*.py"))
for script in scripts:
    compile(script.read_bytes(), str(script), "exec")
print(f"Python syntax checked: {len(scripts)} files")
PY
# Also parse the installer's embedded Python, without installing anything.
PATH="/usr/bin:/bin:$PATH" bash "$ROOT/scripts/setup-account-bridge.sh" --help >/dev/null
# Wire-contract tests use the selected external runtime's real protobuf classes,
# but fake all gateway IO. Nothing authenticates or reads an account session.
TEST_PYTHON="${MUSE_ACCOUNT_TEST_PYTHON:-/usr/bin/python3}"
"$TEST_PYTHON" -I - <<'PY'
import sys
import unittest

suite = unittest.defaultTestLoader.discover("scripts/tests", pattern="test_*.py")
result = unittest.TextTestRunner(verbosity=2).run(suite)
if result.skipped:
    print("Full offline gate rejects skipped tests. Install the pinned bridge with "
          "scripts/setup-account-bridge.sh and set MUSE_ACCOUNT_TEST_PYTHON to its "
          "absolute .venv/bin/python path.", file=sys.stderr)
sys.exit(0 if result.wasSuccessful() and not result.skipped else 1)
PY
