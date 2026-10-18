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
/usr/bin/python3 -m unittest discover -s scripts/tests -p 'test_*.py' -v
