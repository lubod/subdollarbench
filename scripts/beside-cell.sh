#!/usr/bin/env bash
# R56 F6: refuse to run a Docker-heavy tool while a live cell holds a
# campaign lock in the cells' runs dir.
#
# Usage: scripts/beside-cell.sh <tool>
#
# The dir is `$ROOT/runs`, or `SDB_CELL_RUNS_DIR` for the self-test. A
# tool's own `SDB_RUNS_DIR` is deliberately not checked: it is a temp
# dir. `SDB_ALLOW_BESIDE_CELL=1` overrides for the operator and says so.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNS="${SDB_CELL_RUNS_DIR:-$ROOT/runs}"
if [[ "${1:-}" == "check" ]]; then
    shift
fi
TOOL="${1:-tool}"

exec python3 - "$RUNS" "$TOOL" <<'PYEOF'
import glob
import json
import os
import sys

runs, tool = sys.argv[1:3]
paths = sorted(glob.glob(os.path.join(runs, ".campaign.lock.w*")))
legacy = os.path.join(runs, ".campaign.lock")
if os.path.isfile(legacy):
    paths.append(legacy)


def start_ticks(pid):
    try:
        with open(f"/proc/{pid}/stat", encoding="utf-8") as f:
            rest = f.read().rsplit(")", 1)[1]
        return int(rest.split()[19])
    except Exception:
        return None


def alive(info):
    pid = int(info.get("pid") or 0)
    if pid <= 0:
        return False
    ticks = start_ticks(pid)
    if ticks is None:
        return False
    want = int(info.get("start_ticks") or 0)
    if want:
        return ticks == want
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return b"campaign" in f.read()
    except Exception:
        return False


live = []
for path in paths:
    try:
        with open(path, encoding="utf-8") as f:
            info = json.load(f)
    except Exception:
        continue
    if alive(info):
        live.append(info)

if live:
    details = "; ".join(
        f"{h.get('campaign_id', '?')} on w{h.get('worker', 0)} (pid {h.get('pid', '?')})"
        for h in live
    )
    if os.environ.get("SDB_ALLOW_BESIDE_CELL") == "1":
        print(f"[beside-cell] override SDB_ALLOW_BESIDE_CELL=1; live {details}")
        sys.exit(0)
    print(f"[beside-cell] {tool}: refusing beside a live cell: {details}", file=sys.stderr)
    sys.exit(1)

print(f"[beside-cell] {tool}: clear")
PYEOF
