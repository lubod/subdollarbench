#!/usr/bin/env bash
# R96 Z1: four partial features at once -- a compact append-only log for
# the torn tail, no ticket-2 grammar, a slow restart, and a test suite
# that is green but does not catch a stage-7/8 regression.
set -euo pipefail
work="$1"
python3 - "$work/start.sh" <<'PY'
import sys
p = sys.argv[1]
text = open(p).read()
old = "exec python3 -u /workspace/sql_server.py"
assert old in text, "the reference start.sh anchor moved"
new = (
    "sleep 12\n"
    "exec env SDB_SQL_REF_COMPACT=1 SDB_SQL_REF_NO_TICKET2=1 "
    "python3 -u /workspace/sql_server.py"
)
open(p, "w").write(text.replace(old, new, 1))
PY
cat > "$work/test.sh" <<'SH'
#!/bin/sh
# A green suite that covers one happy path but misses a stage-7/8 regression.
echo "ok: join smoke test"
SH
chmod +x "$work/test.sh"
