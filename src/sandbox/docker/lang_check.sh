#!/bin/bash
# R81 F3: the Rust rule on the running server, run inside the candidate
# container as root (`docker exec ... sudo -n bash -c <this> sdb-lang
# <port> <harness-pid>...`). Prints SDB-LANG-OK or SDB-LANG-FAIL <which>.
#
# The rule (round 81's F3 answer):
#   1. The launch tree (the container's PID 1 and every descendant) may
#      not hold a python3 or node process. The interpreter is identified
#      by the ELF of /proc/<pid>/exe, not by its name, so a renamed copy
#      is caught too.
#   2. A python3/node process outside the launch tree whose working
#      directory or command line points into /workspace fails, unless it
#      is part of a harness tree the runner started (the PIDs passed in,
#      with their descendants).
#   3. The process listening on the candidate port is an ELF under
#      /workspace/target/release/ and its .comment section names rustc.
set -u

PORT="$1"
shift
WS=/workspace

fail() {
    echo "SDB-LANG-FAIL $*"
    exit 0
}

# ppid map: /proc/<pid>/stat field 4 is the ppid, after the comm field
# (which may contain spaces and parentheses).
declare -A PPID_OF
for d in /proc/[0-9]*; do
    pid="${d#/proc/}"
    [ -r "$d/stat" ] || continue
    rest=$(sed 's/^[^)]*)//' "$d/stat" 2>/dev/null) || continue
    read -r _state ppid _rest <<<"$rest"
    [ -n "${ppid:-}" ] || continue
    PPID_OF[$pid]="$ppid"
done

# DESC = root plus every descendant, by ppid.
declare -A DESC
descendants() {
    local root="$1" changed=1 pid p
    DESC=()
    [ -n "${PPID_OF[$root]:-}" ] || [ "$root" = "1" ] || return 1
    DESC[$root]=1
    while [ "$changed" = 1 ]; do
        changed=0
        for pid in "${!PPID_OF[@]}"; do
            [ -n "${DESC[$pid]:-}" ] && continue
            p="${PPID_OF[$pid]}"
            if [ -n "${DESC[$p]:-}" ]; then
                DESC[$pid]=1
                changed=1
            fi
        done
    done
}

# An interpreter is a python3/node ELF: the dynamically linked library
# or the interpreter's own symbols/strings. A Rust binary links neither.
is_interp() {
    local exe
    exe=$(readlink -f "/proc/$1/exe" 2>/dev/null) || return 1
    [ -f "$exe" ] || return 1
    if grep -aqE 'Py_Initialize|Python 3\.|libpython3\.' "$exe" 2>/dev/null; then
        return 0
    fi
    if grep -aqE 'libnode\.so|node::' "$exe" 2>/dev/null; then
        return 0
    fi
    return 1
}

interp_name() {
    local exe
    exe=$(readlink -f "/proc/$1/exe" 2>/dev/null) || exe="?"
    echo "${exe##*/}"
}

# The harness trees the runner started: excluded from both rules.
declare -A HARNESS
for root in "$@"; do
    [ -n "$root" ] || continue
    if descendants "$root"; then
        for pid in "${!DESC[@]}"; do
            HARNESS[$pid]=1
        done
    fi
done

# Rule 1: no interpreter in the server's launch tree.
descendants 1 || fail "the container has no PID 1 process table"
for pid in "${!DESC[@]}"; do
    [ -n "${HARNESS[$pid]:-}" ] && continue
    if is_interp "$pid"; then
        fail "a $(interp_name "$pid") process is in the server's launch tree (pid $pid)"
    fi
done

# The listener: the port's process must be the Rust binary. Read the
# listening socket from /proc/net/tcp{,6} and its inode from /proc/*/fd:
# `ss -p` under sudo does not name the owning process here.
port_hex=$(printf '%04X' "$PORT")
inodes=$(awk -v want=":$port_hex" 'NR > 1 && $4 == "0A" {
    split($2, a, ":");
    if (":" a[2] == want) print $10
}' /proc/net/tcp /proc/net/tcp6 2>/dev/null | sort -u)
[ -n "$inodes" ] || fail "no process is listening on port $PORT"
pid=""
for ino in $inodes; do
    for fd in /proc/[0-9]*/fd/*; do
        [ "$(readlink "$fd" 2>/dev/null)" = "socket:[$ino]" ] || continue
        pid="${fd#/proc/}"
        pid="${pid%%/*}"
        break 2
    done
done
[ -n "$pid" ] || fail "the listener socket has no owning process (port $PORT)"
exe=$(readlink -f "/proc/$pid/exe" 2>/dev/null) || fail "cannot read the listener's executable (pid $pid)"
case "$exe" in
    "$WS"/target/release/*) ;;
    *) fail "the listener is not under target/release/ ($exe)" ;;
esac
if ! readelf -p .comment "$exe" 2>/dev/null | grep -q 'rustc'; then
    fail "the listener's .comment section does not name rustc ($exe)"
fi
descendants 1 >/dev/null 2>&1 || true
[ -n "${DESC[$pid]:-}" ] || fail "the listener is not in the server's launch tree (pid $pid)"

# Rule 2: an interpreter that escaped the launch tree but points into
# the workspace, unless it belongs to a harness tree.
for d in /proc/[0-9]*; do
    pid="${d#/proc/}"
    [ -n "${DESC[$pid]:-}" ] && continue
    [ -n "${HARNESS[$pid]:-}" ] && continue
    is_interp "$pid" || continue
    cwd=$(readlink -f "/proc/$pid/cwd" 2>/dev/null) || cwd=""
    cmd=$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null) || cmd=""
    case "$cwd" in
        "$WS" | "$WS"/*)
            fail "a $(interp_name "$pid") process points into the workspace (pid $pid, cwd $cwd)"
            ;;
    esac
    case "$cmd" in
        *"$WS"*)
            fail "a $(interp_name "$pid") process points into the workspace (pid $pid, command line)"
            ;;
    esac
done

echo "SDB-LANG-OK"
