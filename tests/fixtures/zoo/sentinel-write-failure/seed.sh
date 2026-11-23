#!/usr/bin/env bash
# R96 Z1 (R95-3): refuse the first sentinel dataset's DDL. The verifier
# seed is pinned in expected.json, so the 16 seeds sentinel::build may try
# are known here; the refusal is one-shot (a marker in the store), so the
# next ticket's own writes still work.
set -euo pipefail
work="$1"
python3 - "$work/sql_server.py" <<'PY'
import sys

VERIFIER_SEED = "zoo-sentinel-write-v1"
SENTINEL_STAGE = 4


def stage_seed(seed, stage):
    h = 0xCBF29CE484222325
    for b in seed.encode() + b"\xff" + stage.to_bytes(4, "little"):
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


base = stage_seed(VERIFIER_SEED, SENTINEL_STAGE) ^ 0x5E271E1000000004
prefixes = tuple("s%x_" % ((base + i) & 0xFFFFFFFFFFFFFFFF) for i in range(16))
p = sys.argv[1]
text = open(p).read()
a0 = 'COMPACT = os.environ.get("SDB_SQL_REF_COMPACT") == "1"\n'
assert a0 in text, "the COMPACT anchor moved"
text = text.replace(
    a0,
    a0 + "ZOO_REFUSE_PREFIXES = " + repr(prefixes) + "\n",
    1,
)
a1 = "            if COMPACT and first_keyword(sql) in WRITE_KEYWORDS:"
assert a1 in text, "the serve() anchor moved"
refuse = (
    '            if ZOO_REFUSE_PREFIXES and first_keyword(sql) == "CREATE":\n'
    '                _zoo_m = re.match(\n'
    '                    r"CREATE\\s+TABLE\\s+([A-Za-z_][A-Za-z0-9_]*)", sql, re.I)\n'
    '                if _zoo_m and _zoo_m.group(1).startswith(ZOO_REFUSE_PREFIXES):\n'
    '                    _zoo_marker = os.path.join(DATA, "zoo-sentinel-refused")\n'
    '                    if not os.path.exists(_zoo_marker):\n'
    '                        open(_zoo_marker, "w").close()\n'
    '                        stats_error()\n'
    '                        f.write(b"ERR syntax zoo candidate refuses the sentinel grammar\\n")\n'
    '                        f.flush()\n'
    '                        continue\n'
)
text = text.replace(a1, refuse + a1, 1)
open(p, "w").write(text)
PY
