#!/usr/bin/env bash
# R80 F1: check the operator's recording and install it as the voice
# reference. The recording and everything derived from it stay under
# ~/sdb-video/; only this script and the reading text are in the repo.
#
# Usage: video/voice-ref.sh <recording>
#        video/voice-ref.sh --self-test
#
# The reference is ~/sdb-video/voice/ref.wav (mono 24 kHz) and ref.txt
# (the spoken part of video/voice/reading.txt). SDB_VIDEO_DIR and
# SDB_VOICE_DIR override the locations for the self-test.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VIDEO_DIR="${SDB_VIDEO_DIR:-$HOME/sdb-video}"
VOICE_DIR="${SDB_VOICE_DIR:-$VIDEO_DIR/voice}"
READING="$ROOT/video/voice/reading.txt"

usage() {
    sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

refuse() {
    echo "[voice-ref] REFUSED: $*" >&2
    exit 1
}

# A path under any checkout of the repo is refused (portal/ is the
# samples' exception); the operator's recording must never be tracked.
repo_path() {
    local path="$1" repo common main
    path="$(realpath -m "$path")"
    repo="$(git -C "$ROOT" rev-parse --show-toplevel 2>/dev/null || true)"
    common="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
    main=""
    [[ -n "$common" ]] && main="$(dirname "$common")"
    for repo in "$repo" "$main"; do
        [[ -n "$repo" ]] || continue
        case "$path" in
            "$repo"/portal | "$repo"/portal/*) continue ;;
            "$repo" | "$repo"/*) return 0 ;;
        esac
    done
    return 1
}

# The spoken part is everything before the first `---` line.
spoken_text() {
    awk '/^---[[:space:]]*$/{exit} {print}' "$READING" \
        | sed -e 's/[[:space:]]*$//' | sed -e '/^[[:space:]]*$/d'
}

measure() {
    local rec="$1" probe
    probe="$(ffprobe -v error -select_streams a:0 \
        -show_entries format=duration:stream=sample_rate,channels \
        -of json "$rec" 2>/dev/null)" || refuse "ffprobe cannot read $rec"
    DURATION="$(python3 -c 'import json,sys; print(float(json.loads(sys.argv[1])["format"]["duration"]))' "$probe" 2>/dev/null)" \
        || refuse "cannot measure the duration of $rec"
    RATE="$(python3 -c 'import json,sys; print(int(json.loads(sys.argv[1])["streams"][0]["sample_rate"]))' "$probe" 2>/dev/null)" \
        || refuse "cannot measure the sample rate of $rec"
    local vol mean max
    vol="$(ffmpeg -hide_banner -nostats -i "$rec" -af volumedetect -f null - 2>&1 || true)"
    mean="$(printf '%s\n' "$vol" | sed -n 's/.*mean_volume: \(.*\) dB/\1/p' | tail -1)"
    max="$(printf '%s\n' "$vol" | sed -n 's/.*max_volume: \(.*\) dB/\1/p' | tail -1)"
    [[ -n "$mean" && -n "$max" ]] || refuse "cannot measure the level of $rec"
    MEAN="$mean"
    MAX="$max"
}

check() {
    local rec="$1"
    repo_path "$rec" && refuse "the recording is inside the repo's tree ($rec); keep it under $VIDEO_DIR/voice/incoming/"
    [[ -f "$rec" ]] || refuse "no such recording: $rec"
    repo_path "$VOICE_DIR/ref.wav" && refuse "the reference output is inside the repo's tree ($VOICE_DIR/ref.wav)"
    measure "$rec"
    awk -v d="$DURATION" 'BEGIN{exit !(d < 10)}' && refuse "$(printf '%.1f s is shorter than 10 s' "$DURATION")"
    awk -v d="$DURATION" 'BEGIN{exit !(d > 60)}' && refuse "$(printf '%.1f s is longer than 60 s' "$DURATION")"
    awk -v r="$RATE" 'BEGIN{exit !(r < 16000)}' && refuse "$RATE Hz is below 16 kHz"
    awk -v m="$MEAN" 'BEGIN{exit !(m < -60)}' && refuse "$(printf 'silent (mean %s dB, peak %s dB)' "$MEAN" "$MAX")"
    awk -v m="$MAX" 'BEGIN{exit !(m >= -0.1)}' && refuse "$(printf 'clipped (peak %s dB)' "$MAX")"
    return 0
}

install_ref() {
    local rec="$1" tmp
    mkdir -p "$VOICE_DIR"
    tmp="$VOICE_DIR/.ref.$$.wav"
    if ! ffmpeg -y -v error -i "$rec" -ac 1 -ar 24000 -c:a pcm_s16le "$tmp"; then
        rm -f "$tmp"
        refuse "ffmpeg cannot convert $rec"
    fi
    mv "$tmp" "$VOICE_DIR/ref.wav"
    spoken_text > "$VOICE_DIR/ref.txt"
    printf '[voice-ref] installed %s/ref.wav (%.1f s, mono 24 kHz) and ref.txt\n' \
        "$VOICE_DIR" "$DURATION"
}

self_test() {
    local tmp out rc clip
    tmp="$(mktemp -d /tmp/sdb-voice-ref.XXXXXX)"
    trap 'rm -rf "${tmp:-}"' EXIT
    mkdir -p "$tmp/clips" "$tmp/video" "$tmp/voice"

    ffmpeg -y -v error -f lavfi -i "sine=frequency=440:duration=5:sample_rate=44100" "$tmp/clips/short.wav"
    ffmpeg -y -v error -f lavfi -i "sine=frequency=440:duration=65:sample_rate=44100" "$tmp/clips/long.wav"
    ffmpeg -y -v error -f lavfi -i "anullsrc=r=44100:cl=mono:d=12" "$tmp/clips/silent.wav"
    ffmpeg -y -v error -f lavfi -i "sine=frequency=440:duration=12:sample_rate=44100" -af "volume=20" "$tmp/clips/clipped.wav"
    ffmpeg -y -v error -f lavfi -i "sine=frequency=440:duration=12:sample_rate=8000" "$tmp/clips/lowrate.wav"
    ffmpeg -y -v error -f lavfi -i "sine=frequency=220:duration=12:sample_rate=44100" -af "volume=0.1" "$tmp/clips/good.wav"

    expect_refused() {
        local clip="$1" want="$2" out rc=0
        out="$(SDB_VIDEO_DIR="$tmp/video" SDB_VOICE_DIR="$tmp/voice" "$0" "$clip" 2>&1)" || rc=$?
        [[ "$rc" -ne 0 ]] || {
            echo "[voice-ref self-test] $clip was accepted" >&2; exit 1; }
        grep -q "$want" <<<"$out" || {
            echo "[voice-ref self-test] $clip was refused for the wrong reason: $out" >&2
            exit 1; }
    }

    expect_refused "$tmp/clips/short.wav" "shorter than 10 s"
    expect_refused "$tmp/clips/long.wav" "longer than 60 s"
    expect_refused "$tmp/clips/silent.wav" "silent"
    expect_refused "$tmp/clips/clipped.wav" "clipped"
    expect_refused "$tmp/clips/lowrate.wav" "below 16 kHz"
    expect_refused "$ROOT/README.md" "inside the repo"
    local main
    main="$(dirname "$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir)")"
    if [[ "$main" != "$ROOT" && -f "$main/README.md" ]]; then
        expect_refused "$main/README.md" "inside the repo"
    fi
    rc=0
    out="$(SDB_VIDEO_DIR="$tmp/video" SDB_VOICE_DIR="$tmp/voice-empty" "$0" 2>&1)" || rc=$?
    [[ "$rc" -ne 0 ]] && grep -q "no recording" <<<"$out" || {
        echo "[voice-ref self-test] an empty incoming/ was not refused: $out" >&2
        exit 1; }
    mkdir -p "$tmp/voice/incoming"
    cp "$tmp/clips/good.wav" "$tmp/voice/incoming/newest.wav"
    SDB_VIDEO_DIR="$tmp/video" SDB_VOICE_DIR="$tmp/voice" "$0" >/dev/null
    [[ -f "$tmp/voice/ref.wav" ]] || {
        echo "[voice-ref self-test] the newest incoming file was not installed" >&2
        exit 1; }

    # A good clip installs ref.wav and ref.txt.
    SDB_VIDEO_DIR="$tmp/video" SDB_VOICE_DIR="$tmp/voice" "$0" "$tmp/clips/good.wav" >/dev/null
    [[ -f "$tmp/voice/ref.wav" ]] || {
        echo "[voice-ref self-test] the good clip wrote no ref.wav" >&2; exit 1; }
    local rate channels
    rate="$(ffprobe -v error -select_streams a:0 -show_entries stream=sample_rate -of csv=p=0 "$tmp/voice/ref.wav")"
    channels="$(ffprobe -v error -select_streams a:0 -show_entries stream=channels -of csv=p=0 "$tmp/voice/ref.wav")"
    [[ "$rate" == "24000" && "$channels" == "1" ]] || {
        echo "[voice-ref self-test] ref.wav is $rate Hz / $channels ch, want 24000/1" >&2
        exit 1; }
    local expected
    expected="$(awk '/^---[[:space:]]*$/{exit} {print}' "$READING" \
        | sed -e 's/[[:space:]]*$//' | sed -e '/^[[:space:]]*$/d')"
    diff -u <(printf '%s\n' "$expected") "$tmp/voice/ref.txt" >/dev/null || {
        echo "[voice-ref self-test] ref.txt is not the spoken part of reading.txt" >&2
        exit 1; }

    # The reference output inside the repo is refused too.
    out="$(SDB_VIDEO_DIR="$tmp/video" SDB_VOICE_DIR="$ROOT/video/voice" "$0" "$tmp/clips/good.wav" 2>&1)" || rc=$?
    [[ "$rc" -ne 0 ]] && grep -q "reference output is inside the repo" <<<"$out" || {
        echo "[voice-ref self-test] the repo output path was not refused: $out" >&2
        exit 1; }

    echo "[voice-ref self-test] short, long, silent, clipped and low-rate clips are refused; the good clip installs 24 kHz mono and the spoken text"
}

if [[ "${1:-}" == "--self-test" ]]; then
    self_test
    exit 0
fi
if [[ $# -eq 0 ]]; then
    mkdir -p "$VOICE_DIR/incoming"
    rec="$(ls -1t "$VOICE_DIR/incoming"/* 2>/dev/null | head -1 || true)"
    [[ -n "$rec" ]] || refuse "no recording in $VOICE_DIR/incoming/"
else
    [[ $# -eq 1 ]] || usage
    rec="$1"
fi
check "$rec"
install_ref "$rec"
