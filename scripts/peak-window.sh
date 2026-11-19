#!/usr/bin/env bash
# R90 M2: the peak windows of the off-peak-only models, one copy for
# model-check.sh (which refuses a check at peak) and grid.sh (which waits
# for off-peak before a phase). Source this file; do not execute it.
#
# R83 F3: minimax and deepseek are priced and run off-peak only (R75-1,
# DECISIONS #30). minimax's window is the one PRICE_OVERRIDES names
# (`src/cost/calculator.rs`: "2x 01:00-15:00 UTC"). deepseek's is its
# published pricing page: peak 01:00-04:00 and 06:00-10:00 UTC, Monday
# through Friday; weekends and Chinese public holidays are off-peak
# (the check does not know the holiday calendar, so a holiday check may
# wait a day).
#
# SDB_MODEL_CHECK_NOW is the one fake clock (the model check's self-test
# uses it; the grid's self-test uses it too).
peak_state() {
    # R93 G0: the grid passes the model as it appears in a careers file
    # (`openrouter/deepseek/...`); the window matches the model family,
    # so drop the provider prefix the grid's launcher adds.
    local id="${1#openrouter/}" now dow hhmm
    now="${SDB_MODEL_CHECK_NOW:-$(date -u +%Y-%m-%dT%H:%M:%SZ)}"
    dow="$(date -u -d "$now" +%u 2>/dev/null || echo 0)"
    hhmm="$(date -u -d "$now" +%H:%M 2>/dev/null || echo 00:00)"
    case "$id" in
        minimax/*)
            if [[ "$hhmm" > "00:59" && "$hhmm" < "15:00" ]]; then
                echo "peak 01:00-15:00 UTC (off-peak 15:00-01:00 UTC)"
            else
                echo "off-peak"
            fi
            ;;
        deepseek/*)
            # Peak 01:00-04:00 and 06:00-10:00 UTC, Monday-Friday.
            if [[ "$dow" -le 5 ]] && {
                { [[ "$hhmm" > "00:59" && "$hhmm" < "04:00" ]] || \
                  [[ "$hhmm" > "05:59" && "$hhmm" < "10:00" ]]; }; }; then
                echo "peak 01:00-04:00 and 06:00-10:00 UTC on weekdays (off-peak all other hours, weekends and Chinese public holidays)"
            else
                echo "off-peak"
            fi
            ;;
        *) echo "off-peak" ;;
    esac
}
