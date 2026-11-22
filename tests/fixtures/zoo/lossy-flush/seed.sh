#!/usr/bin/env bash
# R96 Z1 (R95-2): a one-shot SIGTERM handler that drops the last committed
# rows. It fires on the graceful stop after the sentinel write; the store
# itself must fail that ticket, not the next one. The marker lives in the
# store directory, so only the first stop in a career does damage.
set -euo pipefail
work="$1"
python3 - "$work/sql_server.py" <<'PY'
import sys
p = sys.argv[1]
text = open(p).read()
if "import signal\n" not in text:
    a0 = "import socket\n"
    assert a0 in text, "the import anchor moved"
    text = text.replace(a0, "import glob\nimport signal\n" + a0, 1)
a1 = '    keeper.execute("PRAGMA synchronous=FULL")\n'
assert a1 in text, "the keeper anchor moved"
handler = '''    # R96 Z1: a clean stop that flushes away the newest rows, once.
    def _zoo_sigterm(signum, frame):
        marker = os.path.join(data, "zoo-lossy-flush-done")
        try:
            # Only a store whose sentinel the harness has written is in
            # the R95-2 window: an earlier graceful restart is the
            # ticket-4 contract, not the snapshot boundary.
            if not os.path.exists(marker) and glob.glob(
                    os.path.join(data, "sentinel-*.json")):
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
open(p, "w").write(text)
PY
