#!/usr/bin/env bash
# R96 Z1: the mimo `.51` shape. The reference marks an open transaction in
# its store and a recovery that finds the mark after a kill drops the
# *stage-4 sentinel dataset* (the data ticket 4 wrote), leaving the
# current stage's own tables and later sentinels alone. Round 95 runs the
# kill on a scratch copy and reads the older sentinels there, so the loss
# costs stage 6 of the ticket that ran the check; before round 95 the
# damaged store was snapshotted and ticket 5 failed `broke ticket 4`.
set -euo pipefail
work="$1"
python3 - "$work/sql_server.py" <<'PY'
import sys

VERIFIER_SEED = "zoo-lossy-recovery-v1"
SENTINEL_STAGES = [4]


def stage_seed(seed, stage):
    h = 0xCBF29CE484222325
    for b in seed.encode() + b"\xff" + stage.to_bytes(4, "little"):
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


prefixes = set()
for stage in SENTINEL_STAGES:
    base = stage_seed(VERIFIER_SEED, stage) ^ 0x5E271E1000000004
    for offset in range(16):
        prefixes.add("s%x_" % ((base + offset) & 0xFFFFFFFFFFFFFFFF))
prefixes = tuple(sorted(prefixes))
# The transaction kill's isolation tables use the `c<16 hex>_` namespace;
# the differential suite only uses `s<seed>_`. The crash ladder never
# creates a `c...` table, so only the kill-with-an-open-transaction
# recovery loses the sentinel.
p = sys.argv[1]
text = open(p).read()
a1 = "            if COMPACT and first_keyword(sql) in WRITE_KEYWORDS:"
assert a1 in text, "the serve() anchor moved"
mark = (
    '            if first_keyword(sql) == "BEGIN":\n'
    '                open(os.path.join(DATA, "tx-open"), "w").close()\n'
    '            elif first_keyword(sql) in ("COMMIT", "ROLLBACK"):\n'
    '                try:\n'
    '                    os.remove(os.path.join(DATA, "tx-open"))\n'
    '                except OSError:\n'
    '                    pass\n'
)
text = text.replace(a1, mark + a1, 1)
a2 = ("    if COMPACT:\n"
      "        # The keeper holds the shared-memory database; the log is replayed")
assert a2 in text, "the main() anchor moved"
drop = (
    '    if os.path.exists(os.path.join(data, "tx-open")):\n'
    '        # R96 Z1: a recovery from a kill with an open transaction\n'
    '        # discards the older sentinels (the mimo .51 shape).\n'
    '        try:\n'
    '            names = [r[0] for r in keeper.execute(\n'
    '                "SELECT name FROM sqlite_master WHERE type=\'table\'").fetchall()]\n'
    '            if any(re.match(r"c[0-9a-f]{16}_", n) for n in names):\n'
    '                for n in names:\n'
    '                    if n.startswith(' + repr(prefixes) + '):\n'
    '                        keeper.execute("DROP TABLE IF EXISTS " + n)\n'
    '            os.remove(os.path.join(data, "tx-open"))\n'
    '        except Exception as e:\n'
    '            print("sdb-sql zoo lossy-recovery: %s" % e,\n'
    '                  file=sys.stderr, flush=True)\n'
)
text = text.replace(a2, drop + a2, 1)
open(p, "w").write(text)
PY
