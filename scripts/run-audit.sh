#!/usr/bin/env bash
# R74 F3: the run audit before publishing.
#
# Reads the runs the export would publish and reports, per model:
#   - phases over F2's health thresholds (tool errors > 2 % of turns and
#     at least 5; empty responses > 5 % and at least 5; billed/list > 1.25);
#   - every phase 1 of a model at 0 points;
#   - a rerun much worse than an earlier attempt of the same model
#     (median career points under half of the earlier attempt's);
# and checks `docs/audit-waivers.txt` for a line closing each finding.
#
# Usage: scripts/run-audit.sh [--runs-dir DIR] [--export DIR]
#                             [--waivers FILE] [--check] [--summary] [--json]
#        scripts/run-audit.sh --self-test
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
exec python3 - "$ROOT" "$@" <<'PY'
import json, os, re, statistics, subprocess, sys, tempfile

ROOT = sys.argv[1]
args = sys.argv[2:]
runs_dir = os.path.join(ROOT, "runs")
export = ""
newer = ""
waivers = os.path.join(ROOT, "docs", "audit-waivers.txt")
check = False
summary = False
json_out = False
self_test = False
i = 0
while i < len(args):
    a = args[i]
    if a == "--runs-dir":
        runs_dir = args[i + 1]; i += 2
    elif a == "--export":
        export = args[i + 1]; i += 2
    elif a == "--waivers":
        waivers = args[i + 1]; i += 2
    elif a == "--newer":
        newer = args[i + 1]; i += 2
    elif a == "--check":
        check = True; i += 1
    elif a == "--summary":
        summary = True; i += 1
    elif a == "--json":
        json_out = True; i += 1
    elif a == "--self-test":
        self_test = True; i += 1
    else:
        print(f"[audit] unknown argument: {a}", file=sys.stderr)
        raise SystemExit(2)
runs_dir = os.environ.get("SDB_AUDIT_RUNS_DIR", runs_dir)
waivers = os.environ.get("SDB_AUDIT_WAIVERS", waivers)
BIN = os.environ.get("SDB_BIN", os.path.join(ROOT, "target", "release", "subdollarbench"))

def load(path):
    try:
        return json.load(open(path))
    except Exception:
        return None

def health_of(cell_dir, rollup):
    h = rollup.get("health") if rollup else None
    if isinstance(h, dict):
        return h
    try:
        out = subprocess.check_output(
            [BIN, "health", "--run-dir", cell_dir, "--json"],
            text=True, stderr=subprocess.DEVNULL,
        )
        return json.loads(out)
    except Exception:
        return {}

def date_of(cid):
    m = re.search(r"_(\d{8})_", cid)
    return m.group(1) if m else "?"

def career_ids_from_export(exp):
    index = os.path.join(exp, "index.html")
    if not os.path.isfile(index):
        return []
    text = open(index, errors="replace").read()
    ids = []
    for m in re.finditer(r"results\.html#(campaign_[A-Za-z0-9_.-]+)", text):
        if m.group(1) not in ids:
            ids.append(m.group(1))
    return ids

def career_cells(cid):
    rollup = load(os.path.join(runs_dir, cid, "campaign.json"))
    if not rollup:
        return []
    cells = (rollup.get("career") or {}).get("cells") or [cid]
    return cells

def add(findings, model, career, phase, kind, detail):
    findings.append({
        "model": model, "career": career, "phase": phase,
        "kind": kind, "detail": detail,
    })

def audit():
    findings = []
    if not os.path.isdir(runs_dir):
        return [], [], []
    published = career_ids_from_export(export) if export else []
    if not published:
        # No export: every non-hidden, non-rehearsal, non-smoke career.
        cutoff = os.path.getmtime(newer) if newer else None
        for name in sorted(os.listdir(runs_dir)):
            d = os.path.join(runs_dir, name)
            if not name.startswith("campaign_") or not os.path.isdir(d):
                continue
            if cutoff is not None and os.path.getmtime(d) < cutoff:
                continue
            r = load(os.path.join(d, "campaign.json"))
            if not r or r.get("rehearsal") or r.get("smoke"):
                continue
            if os.path.exists(os.path.join(d, ".portal_hidden")):
                continue
            published.append(name)

    for cid in published:
        rollup = load(os.path.join(runs_dir, cid, "campaign.json"))
        if not rollup:
            continue
        model = rollup.get("model", "?")
        cells = (rollup.get("career") or {}).get("cells") or [cid]
        for cell in cells:
            d = os.path.join(runs_dir, cell)
            if not os.path.isdir(d):
                continue
            r = load(os.path.join(d, "campaign.json")) or {}
            h = health_of(d, r)
            turns = int(h.get("turns") or 0)
            errors = int(h.get("tool_errors") or 0)
            empty = int(h.get("empty_responses") or 0)
            ratio = h.get("billed_ratio")
            if turns > 0 and errors >= 5 and errors / turns > 0.02:
                add(findings, model, cid, r.get("phase"), "tool-errors",
                    f"{errors}/{turns} turns")
            if turns > 0 and empty >= 5 and empty / turns > 0.05:
                add(findings, model, cid, r.get("phase"), "empty-responses",
                    f"{empty}/{turns} turns")
            if ratio is not None and float(ratio) > 1.25:
                add(findings, model, cid, r.get("phase"), "billed-ratio",
                    f"{ratio:.3f}")
            if r.get("phase") == 1 and float(r.get("points") or 0.0) == 0.0:
                add(findings, model, cid, 1, "phase1-zero", "0 points")

    # A rerun much worse than an earlier attempt: group the careers by
    # model + start date; the latest group's median career points under
    # half of an earlier group's. Hidden runs count.
    groups = {}
    for name in sorted(os.listdir(runs_dir)):
        d = os.path.join(runs_dir, name)
        if not name.startswith("campaign_") or not os.path.isdir(d):
            continue
        r = load(os.path.join(d, "campaign.json"))
        if not r or r.get("rehearsal") or r.get("smoke"):
            continue
        points = (r.get("career") or {}).get("points")
        if points is None:
            continue
        key = (r.get("model", "?"), date_of(name))
        groups.setdefault(key, []).append(float(points))
    by_model = {}
    for (model, date), points in groups.items():
        by_model.setdefault(model, {})[date] = statistics.median(points)
    for model, dates in by_model.items():
        if len(dates) < 2:
            continue
        latest = max(dates)
        earlier = [d for d in dates if d < latest]
        if not earlier:
            continue
        best_earlier = max(dates[d] for d in earlier)
        if dates[latest] < best_earlier / 2.0:
            add(findings, model, "", None, "worse-rerun",
                f"{latest} median {dates[latest]:.2f} vs {best_earlier:.2f}")

    waivers_lines = []
    if os.path.isfile(waivers):
        for line in open(waivers, errors="replace"):
            line = line.strip()
            if line and not line.startswith("#"):
                waivers_lines.append(line)
    open_findings = []
    waived = []
    for f in findings:
        close = None
        for line in waivers_lines:
            parts = [p.strip() for p in line.split("|")]
            if len(parts) >= 2 and parts[0] == f["model"] and parts[1] == f["kind"]:
                close = line
                break
        if close:
            waived.append((f, close))
        else:
            open_findings.append(f)
    return findings, open_findings, waived

def print_report(findings, open_findings, waived):
    if json_out:
        print(json.dumps({"findings": findings, "open": open_findings,
                          "waived": [{"finding": f, "line": l} for f, l in waived]},
                         indent=1))
        return
    if not findings:
        print("[audit] no findings")
        return
    for f in findings:
        where = f["model"]
        if f["career"]:
            where += f" {f['career']}"
        if f["phase"] is not None:
            where += f" phase {f['phase']}"
        state = "WAIVED" if any(f is w[0] for w in waived) else "OPEN"
        print(f"[audit] {state}: {f['kind']} {where}: {f['detail']}")
    for f, line in waived:
        print(f"[audit] waiver: {line}")
    print(f"[audit] {len(findings)} finding(s), {len(open_findings)} open, {len(waived)} waived")

def run_self_test():
    root = tempfile.mkdtemp(prefix="sdb-audit-self-")
    runs = os.path.join(root, "runs")
    exp = os.path.join(root, "export")
    os.makedirs(runs)
    os.makedirs(exp)
    def write(cid, model, phase, points, health, career_points, hidden=False):
        d = os.path.join(runs, cid)
        os.makedirs(d, exist_ok=True)
        r = {"campaign_id": cid, "model": model, "phase": phase,
             "points": points, "total_cost_usd": 1.0, "billed_usd": 1.0,
             "health": health,
             "career": {"cells": [cid], "points": career_points}}
        json.dump(r, open(os.path.join(d, "campaign.json"), "w"))
        if hidden:
            open(os.path.join(d, ".portal_hidden"), "w").write("hidden\n")
    clean = {"turns": 100, "tool_errors": 0, "ref_queries": 10,
             "ref_refused": 0, "empty_responses": 0, "billed_ratio": 1.0}
    bad = {"turns": 73, "tool_errors": 25, "ref_queries": 0,
           "ref_refused": 0, "empty_responses": 0, "billed_ratio": 1.0}
    mini_old = "campaign_sdb-sql_minimax_20261107_100000_w1"
    mini_new = "campaign_sdb-sql_minimax_20261108_100000_w1"
    write(mini_old, "minimax", 1, 7.0, clean, 7.0, hidden=True)
    write(mini_new, "minimax", 1, 0.0, bad, 0.0)
    ids = [mini_new]
    for n, model in enumerate(["anthropic", "deepseek", "google", "openai",
                               "qwen", "xiaomi"]):
        cid = f"campaign_sdb-sql_{model}_20261108_10000{n}_w1"
        write(cid, model, 1, 8.0, clean, 8.0)
        ids.append(cid)
    open(os.path.join(exp, "index.html"), "w").write(
        "".join(f'<a href="results.html#{cid}">x</a>' for cid in ids))
    waiver = os.path.join(root, "waivers.txt")
    open(waiver, "w").write(
        "minimax | tool-errors | operator | provider fault\n"
        "minimax | phase1-zero | operator | provider fault\n"
        "minimax | worse-rerun | operator | provider fault\n")
    global export, runs_dir, waivers
    export, runs_dir, waivers = exp, runs, os.path.join(root, "none.txt")
    findings, open_f, _ = audit()
    kinds = {(f["model"], f["kind"]) for f in open_f}
    want = {("minimax", "tool-errors"), ("minimax", "phase1-zero"),
            ("minimax", "worse-rerun")}
    assert want <= kinds, f"missing minimax findings: {kinds}"
    assert not any(m != "minimax" for m, _ in kinds), f"clean model flagged: {kinds}"
    waivers = waiver
    findings, open_f, waived = audit()
    assert not open_f, f"waivers did not close: {open_f}"
    assert len(waived) >= 3, f"waivers not printed: {waived}"
    print("RESULT: PASS (run-audit self-test)")

if self_test:
    run_self_test()
    raise SystemExit(0)

findings, open_findings, waived = audit()
print_report(findings, open_findings, waived)
if check and open_findings:
    raise SystemExit(1)
PY
