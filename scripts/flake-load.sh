#!/usr/bin/env bash
# R48 F3: background load for `scripts/flake-bar.sh --load <L>`.
#
# Each load container is capped like a candidate (`--cpus=2`, 2 GiB) and
# does what a model's session does next to a graded cell: a compile loop
# and a write-and-fsync loop. The containers carry `sdb.flake-load=<key>`
# and `sdb.role=load`, and `stop` removes every one for that key. The bar
# calls `stop` from an EXIT/INT/TERM trap, so a Ctrl-C does not leak them.
#
# The self-test uses a fake `docker` (no daemon needed): it checks the run
# arguments, the label, the removal, and that a child interrupted with
# SIGINT runs `stop`.
set -euo pipefail

LABEL="sdb.flake-load"
IMAGE="${SDB_LOAD_IMAGE:-subdollar-sandbox}"

load_cmd() {
    cat <<'CMD'
while :; do
  printf 'int main(void){return 0;}\n' > /tmp/flake-load.c
  (cc -o /tmp/flake-load.bin /tmp/flake-load.c) >/dev/null 2>&1 || true
  dd if=/dev/zero of=/tmp/flake-load.dat bs=1M count=4 conv=fsync >/dev/null 2>&1 || true
done
CMD
}

start() {
    local n="$1" key="$2" i
    [[ "$n" =~ ^[1-9][0-9]*$ ]] || {
        echo "[flake-load] count must be a positive integer" >&2
        return 2
    }
    [[ -n "$key" ]] || {
        echo "[flake-load] a key is required" >&2
        return 2
    }
    for i in $(seq 1 "$n"); do
        if ! docker run -d --rm \
            --label "$LABEL=$key" \
            --label "sdb.role=load" \
            --name "sdb-flake-load-$key-$i" \
            --memory=2g \
            --cpus=2.0 \
            --pids-limit=256 \
            "$IMAGE" /bin/bash -c "$(load_cmd)" >/dev/null; then
            echo "[flake-load] could not start load container $i" >&2
            stop "$key"
            return 1
        fi
    done
    echo "[flake-load] $n load container(s) for $key" >&2
}

stop() {
    local key="$1" ids
    [[ -n "$key" ]] || return 0
    ids="$(docker ps -aq --filter "label=$LABEL=$key" 2>/dev/null || true)"
    if [[ -n "$ids" ]]; then
        # shellcheck disable=SC2086
        docker rm -f $ids >/dev/null 2>&1 || true
    fi
    echo "[flake-load] removed $key" >&2
}

self_test() {
    local dir bin log rc
    dir="$(mktemp -d)"
    trap 'rm -rf "${dir:-}"' EXIT
    bin="$dir/bin"
    log="$dir/docker.log"
    mkdir -p "$bin"
    cat > "$bin/docker" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$DOCKER_LOG"
case "$1" in
    run) echo "fake-container-id" ;;
    ps)  echo "fake-container-id" ;;
    *)   ;;
esac
EOF
    chmod +x "$bin/docker"
    export PATH="$bin:$PATH"
    export DOCKER_LOG="$log"

    start 2 testkey
    grep -q -- "--label sdb.flake-load=testkey" "$log" || {
        echo "the load container is not labelled"; exit 1; }
    [[ "$(grep -c '^run ' "$log")" -eq 2 ]] || {
        echo "start did not run two containers"; exit 1; }
    grep -q -- "--cpus=2.0" "$log" || { echo "no 2-CPU cap"; exit 1; }
    grep -q -- "--memory=2g" "$log" || { echo "no 2 GiB cap"; exit 1; }

    : > "$log"
    stop testkey
    grep -q '^rm -f ' "$log" || { echo "stop did not remove the containers"; exit 1; }

    # Ctrl-C: a child with the bar's trap starts a load container, gets
    # SIGINT, and must remove it before exiting.
    : > "$log"
    rc=0
    bash -c '
        source "$1"
        trap "stop testkey" INT
        start 1 testkey
        kill -INT $$
        sleep 0.2
    ' _ "$0" || rc=$?
    [[ "$rc" -eq 0 || "$rc" -eq 130 ]] || {
        echo "the interrupted child exited $rc"; exit 1; }
    grep -q '^rm -f ' "$log" || {
        echo "SIGINT did not remove the load containers"; exit 1; }
    echo "RESULT: PASS (flake-load self-test)"
}

# Sourced by the self-test: define the functions, do not run a command.
if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
    return 0
fi

case "${1:-}" in
    start) shift; start "$@" ;;
    stop) shift; stop "$@" ;;
    self-test) self_test ;;
    *)
        echo "usage: flake-load.sh start <n> <key> | stop <key> | self-test" >&2
        exit 2
        ;;
esac
