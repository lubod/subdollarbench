"""Backfill tripped-trial stats from archived console.log last-live TELEMETRY.
Usage: python3 backfill_billing.py [--yes]
Dry run by default; --yes patches subrun docs in DB (parents + trials
rows with full column shape) + results/*.json, then re-merges each touched
parent via the existing `backfill --yes` CLI (fair-cost/efficiency
recompute). Pass rates untouched (genuine verifier output).
"""
import json, os, re, sqlite3, sys

YES = "--yes" in sys.argv
TELE = re.compile(r"TELEMETRY\] turn=(\d+) max_turns=\d+ spent=([\d.]+) tokens=(\d+)")

def trial_status(pass_rate):
    return "passed" if (pass_rate or 0) >= 100.0 else "failed"

con = sqlite3.connect("bench.db")
rows = dict((i, json.loads(d)) for i, d in con.execute("SELECT id,doc FROM parents").fetchall())

patched_parents = []
patched_trials = 0
for pid, j in sorted(rows.items()):
    subs = j.get("subruns") or []
    if not subs:
        continue
    dirty = False
    for idx, s in enumerate(subs):
        if (s.get("total_cost_usd") or 0) != 0 or (s.get("total_tokens") or 0) != 0:
            continue
        tid = s.get("id", "")
        f = "runs/%s/console.log" % tid
        if not os.path.exists(f):
            continue
        txt = open(f, errors="ignore").read()
        if "[BILLING]" not in txt:
            continue
        teles = TELE.findall(txt)
        nz = [t for t in teles if t[0] != "0"]
        if not nz:
            continue
        turn, spent, tokens = nz[-1]
        tokens = int(tokens)
        s["turns"] = int(turn)
        s["prompt_tokens"] = tokens
        s["total_tokens"] = tokens
        s["total_cost_usd"] = round(float(spent), 6)
        dirty = True
        patched_trials += 1
        if YES:
            sub_doc = json.dumps(s)
            con.execute(
                "INSERT OR REPLACE INTO trials (id, parent_id, trial_no, pass_rate, status, cost_usd, duration_s, turns, doc)"
                " VALUES (?,?,?,?,?,?,?,?,?)",
                (s.get("id", "%s#%d" % (pid, idx)), pid, idx + 1, s.get("pass_rate", 0.0),
                 trial_status(s.get("pass_rate")), s.get("total_cost_usd", 0.0),
                 s.get("duration_seconds", 0.0), s.get("turns"),
                 sub_doc),
            )
    if dirty:
        patched_parents.append(pid)
        if YES:
            con.execute("UPDATE parents SET doc=? WHERE id=?", (json.dumps(j), pid))
            fp = "results/%s.json" % pid
            if os.path.exists(fp):
                open(fp, "w").write(json.dumps(j, indent=2))
if YES:
    con.commit()
print("tripped trials patched: %d across %d parents %s" % (patched_trials, len(patched_parents), "(WRITTEN)" if YES else "(dry run)"))
if patched_parents and not YES:
    print("parents:", ", ".join(p[:45] for p in patched_parents[:16]))
if YES and patched_parents:
    print("NEXT: ./target/release/subdollarbench backfill --yes  # re-merge fair costs")
