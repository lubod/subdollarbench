#!/usr/bin/env bash
# R96 Z1: the mimo `.51` shape. The reference marks an open transaction in
# its store and a recovery that finds the mark after a kill drops every
# older table. Round 95 runs the kill on a scratch copy and reads the
# older sentinels there, so the loss costs the ticket whose check did it.
set -euo pipefail
work="$1"
python3 - "$work/sql_server.py" <<'PY'
import sys
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
    '        # R96 Z1: recovery after a kill with an open transaction\n'
    '        # discards what earlier tickets committed (mimo .51).\n'
    '        try:\n'
    '            names = [r[0] for r in keeper.execute(\n'
    '                "SELECT name FROM sqlite_master WHERE type=\'table\'").fetchall()]\n'
    '            for n in names:\n'
    '                keeper.execute("DROP TABLE IF EXISTS " + n)\n'
    '            os.remove(os.path.join(data, "tx-open"))\n'
    '        except Exception as e:\n'
    '            print("sdb-sql zoo lossy-recovery: %s" % e,\n'
    '                  file=sys.stderr, flush=True)\n'
)
text = text.replace(a2, drop + a2, 1)
open(p, "w").write(text)
PY
