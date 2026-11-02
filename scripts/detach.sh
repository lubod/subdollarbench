#!/usr/bin/env bash
# R61 F5: launch a long job so a restart of the agent's service (opencode or
# Claude) cannot kill it.
#
# Usage: scripts/detach.sh <name> -- <command...>
#
# The job runs as the systemd user unit sdb-<name>, in this directory. It
# survives the launching shell, its process group and a service restart, and
# it is logged by the user journal. Linger must be on (`loginctl
# enable-linger <user>`, a one-time host change the operator makes);
# otherwise the job would die at logout, so this script refuses to start.
#
# Follow: journalctl --user -u sdb-<name> -f
# Stop:   systemctl --user stop sdb-<name>, or kill the unit's MainPID.
#         Never pkill -f.
# Status: the job's exit status is written to
#         <git-common-dir>/sdb-gates/detach/<name>.status
#         (SDB_DETACH_DIR overrides the directory).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
NAME_RE='^[a-z0-9][a-z0-9-]*$'

detach_dir() {
    if [[ -n "${SDB_DETACH_DIR:-}" ]]; then
        printf '%s\n' "$SDB_DETACH_DIR"
        return 0
    fi
    printf '%s/sdb-gates/detach\n' \
        "$(cd "$ROOT" && git rev-parse --path-format=absolute --git-common-dir)"
}

linger_is_on() {
    local linger
    linger="$(loginctl show-user "$USER" -p Linger 2>/dev/null \
        | sed -n 's/^Linger=//p')"
    [[ "$linger" == "yes" ]]
}

unit_state() {
    systemctl --user show "sdb-$1" -p ActiveState --value 2>/dev/null || true
}

unit_active() {
    case "$(unit_state "$1")" in
        active|activating|reloading) return 0 ;;
        *) return 1 ;;
    esac
}

start() {
    local name="$1"
    shift
    local dir status unit pid
    dir="$(detach_dir)"
    mkdir -p "$dir"
    status="$dir/$name.status"
    unit="sdb-$name"

    if ! linger_is_on; then
        echo "[detach] FATAL: linger is off for $USER; the unit would die at logout" >&2
        echo "[detach] Fix it once (host change): loginctl enable-linger $USER" >&2
        return 1
    fi
    if unit_active "$name"; then
        echo "[detach] FATAL: sdb-$name is already running; stop it first:" >&2
        echo "[detach]   systemctl --user stop $unit" >&2
        return 1
    fi
    systemctl --user reset-failed "$unit" >/dev/null 2>&1 || true
    rm -f "$status"

    # The user manager's PATH lacks ~/.cargo/bin and friends: pass the
    # caller's PATH (and the toolchain homes, when set) to the unit. The
    # first bar through this script failed 1,120 runs on `nice: cargo: No
    # such file or directory` (docs/rounds/61.md, "After the merge").
    local envs=(--setenv="SDB_DETACH_STATUS=$status" --setenv="PATH=$PATH")
    local v
    for v in CARGO_HOME RUSTUP_HOME; do
        [[ -n "${!v:-}" ]] && envs+=(--setenv="$v=${!v}")
    done
    if ! systemd-run --user --unit "$unit" --same-dir --collect \
        "${envs[@]}" \
        -- bash -c 'rc=0; "$@" || rc=$?; printf "%s\n" "$rc" > "$SDB_DETACH_STATUS.tmp"; mv "$SDB_DETACH_STATUS.tmp" "$SDB_DETACH_STATUS"; exit "$rc"' \
        _ "$@"; then
        echo "[detach] FATAL: systemd-run could not start $unit" >&2
        return 1
    fi
    pid=""
    for _ in $(seq 1 20); do
        pid="$(systemctl --user show "$unit" -p MainPID --value 2>/dev/null || true)"
        [[ "$pid" =~ ^[0-9]+$ ]] && [[ "$pid" -gt 0 ]] && break
        sleep 0.1
    done
    echo "[detach] started $unit"
    echo "[detach] follow: journalctl --user -u $unit -f"
    echo "[detach] stop:   systemctl --user stop $unit"
    echo "[detach] pid:    ${pid:-unknown} (kill that PID only, never pkill -f)"
    echo "[detach] status: $status"
}

self_test() {
    local dir marker out rc unit i launcher
    dir="$(mktemp -d /tmp/sdb-detach-self-test.XXXXXX)"
    trap "systemctl --user stop sdb-detach-survive sdb-detach-busy sdb-detach-path >/dev/null 2>&1 || true; systemctl --user reset-failed sdb-detach-survive sdb-detach-busy sdb-detach-path >/dev/null 2>&1 || true; rm -rf '$dir'" EXIT
    export SDB_DETACH_DIR="$dir"
    marker="$dir/marker"

    if ! linger_is_on; then
        echo "[self-test] SKIP: linger is off for $USER; enable it and rerun"
        return 0
    fi

    # 1) A job launched from a subshell survives that subshell's process
    # group being killed.
    setsid "$ROOT/scripts/detach.sh" detach-survive -- \
        bash -c "sleep 2; echo alive > '$marker'" >"$dir/launch.out" 2>&1 &
    launcher=$!
    for i in $(seq 1 50); do
        [[ "$(unit_state detach-survive)" == "active" ]] && break
        sleep 0.1
    done
    kill -TERM -- "-$launcher" 2>/dev/null || true
    wait "$launcher" 2>/dev/null || true
    for i in $(seq 1 100); do
        [[ -f "$marker" ]] && break
        sleep 0.1
    done
    [[ -f "$marker" ]] || {
        echo "[self-test] the job died with its launcher's process group" >&2
        cat "$dir/launch.out" >&2
        return 1
    }
    echo "[self-test] survived the launcher's process group"

    # 2) The exit status is readable afterwards.
    for i in $(seq 1 100); do
        [[ -f "$dir/detach-survive.status" ]] && break
        sleep 0.1
    done
    rc="$(cat "$dir/detach-survive.status" 2>/dev/null || true)"
    [[ "$rc" == "0" ]] || {
        echo "[self-test] exit status was '$rc', want 0" >&2
        return 1
    }
    echo "[self-test] exit status readable"

    # 3) The same name is refused while it runs.
    "$ROOT/scripts/detach.sh" detach-busy -- sleep 5 >"$dir/busy.out" 2>&1 || {
        echo "[self-test] the busy fixture did not start" >&2
        cat "$dir/busy.out" >&2
        return 1
    }
    if "$ROOT/scripts/detach.sh" detach-busy -- true >"$dir/second.out" 2>&1; then
        echo "[self-test] a second launch with the same name was accepted" >&2
        return 1
    fi
    grep -q 'already running' "$dir/second.out" || {
        echo "[self-test] the refusal did not name the running unit" >&2
        cat "$dir/second.out" >&2
        return 1
    }
    systemctl --user stop sdb-detach-busy >/dev/null 2>&1 || true
    echo "[self-test] duplicate name refused"

    # 4) The unit sees the caller's PATH: a command only on that PATH runs.
    mkdir -p "$dir/pathbin"
    printf '#!/usr/bin/env bash\necho on-path > "$1"\n' >"$dir/pathbin/sdb-detach-probe"
    chmod +x "$dir/pathbin/sdb-detach-probe"
    PATH="$dir/pathbin:$PATH" "$ROOT/scripts/detach.sh" detach-path -- \
        sdb-detach-probe "$dir/path-marker" >"$dir/path.out" 2>&1 || {
        echo "[self-test] the PATH fixture did not start" >&2
        cat "$dir/path.out" >&2
        return 1
    }
    for i in $(seq 1 100); do
        [[ -f "$dir/detach-path.status" ]] && break
        sleep 0.1
    done
    [[ "$(cat "$dir/detach-path.status" 2>/dev/null)" == "0" && -f "$dir/path-marker" ]] || {
        echo "[self-test] the unit did not see the caller's PATH" >&2
        journalctl --user -u sdb-detach-path --no-pager -n 5 >&2 || true
        return 1
    }
    echo "[self-test] the caller's PATH reaches the unit"

    # 5) Linger off is refused with the fix.
    mkdir -p "$dir/bin"
    cat >"$dir/bin/loginctl" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "show-user" ]]; then
    echo "Linger=no"
    exit 0
fi
exec /usr/bin/loginctl "$@"
EOF
    chmod +x "$dir/bin/loginctl"
    if PATH="$dir/bin:$PATH" "$ROOT/scripts/detach.sh" detach-nolinger -- true \
        >"$dir/nolinger.out" 2>&1; then
        echo "[self-test] linger off was accepted" >&2
        return 1
    fi
    grep -q 'enable-linger' "$dir/nolinger.out" || {
        echo "[self-test] the refusal did not name the fix" >&2
        cat "$dir/nolinger.out" >&2
        return 1
    }
    echo "[self-test] linger off refused"

    echo "RESULT: PASS (detach self-test)"
}

case "${1:-}" in
    --self-test)
        self_test
        exit $? ;;
    "")
        echo "usage: scripts/detach.sh <name> -- <command...>" >&2
        exit 2 ;;
esac

name="$1"
shift
[[ "$name" =~ $NAME_RE ]] || {
    echo "[detach] FATAL: name must match $NAME_RE (got '$name')" >&2
    exit 2
}
[[ "${1:-}" == "--" ]] || {
    echo "usage: scripts/detach.sh <name> -- <command...>" >&2
    exit 2
}
shift
[[ $# -gt 0 ]] || {
    echo "[detach] FATAL: no command after --" >&2
    exit 2
}
start "$name" "$@"
