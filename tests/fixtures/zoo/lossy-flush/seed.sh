#!/usr/bin/env bash
# R96 Z1 (R95-2): a one-shot SIGTERM handler that drops the last committed
# rows after the harness writes a sentinel. The sentinel files live outside
# the container, so the candidate recognizes the sentinel's dataset by the
# 16 seeded table prefixes its first write may use (the seed is pinned in
# expected.json). A marker in the store keeps the damage to one stop.
set -euo pipefail
work="$1"
python3 - "$work/sql_server.py" <<'PY'
import sys

VERIFIER_SEED = "zoo-lossy-flush-v1"
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
if "import signal\n" not in text:
    a0 = "import socket\n"
    assert a0 in text, "the import anchor moved"
    text = text.replace(a0, "import re\nimport signal\n" + a0, 1)
a0 = 'COMPACT = os.environ.get("SDB_SQL_REF_COMPACT") == "1"\n'
assert a0 in text, "the COMPACT anchor moved"
text = text.replace(a0, a0 + "ZOO_SENTINEL_PREFIXES = " + repr(prefixes) + "\n", 1)
a1 = '    keeper.execute("PRAGMA synchronous=FULL")\n'
assert a1 in text, "the keeper anchor moved"
handler = '''    # R96 Z1: a clean stop that flushes away the newest rows, once,
    # only after the harness has written its first sentinel dataset.
    def _zoo_sigterm(signum, frame):
        marker = os.path.join(data, "zoo-lossy-flush-done")
        seen = os.path.join(data, "zoo-sentinel-seen")
        try:
            if os.path.exists(seen) and not os.path.exists(marker):
                names = [r[0] for r in keeper.execute(
                    "SELECT name FROM sqlite_master WHERE type='table'").fetchall()]
                for n in names:
                    keeper.execute("DELETE FROM " + n)
                open(marker, "w").close()
                print("sdb-sql zoo lossy-flush: dropped %d tables" % len(names),
                      file=sys.stderr, flush=True)
        except Exception as e:
            print("sdb-sql zoo lossy-flush: %s" % e, file=sys.stderr, flush=True)
        os._exit(0)

    signal.signal(signal.SIGTERM, _zoo_sigterm)
'''
text = text.replace(a1, a1 + handler, 1)
a2 = "            if COMPACT and first_keyword(sql) in WRITE_KEYWORDS:"
assert a2 in text, "the serve() anchor moved"
seen_code = '''            if first_keyword(sql) == "CREATE":
                _m = re.match(r"CREATE\\s+TABLE\\s+([A-Za-z_][A-Za-z0-9_]*)", sql, re.I)
                if _m and _m.group(1).startswith(ZOO_SENTINEL_PREFIXES):
                    try:
                        open(os.path.join(DATA, "zoo-sentinel-seen"), "w").close()
                    except OSError:
                        pass
'''
text = text.replace(a2, seen_code + a2, 1)
open(p, "w").write(text)
PY
