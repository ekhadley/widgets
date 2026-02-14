#!/bin/bash
# Benchmark widget startup time.
# Measures wall time from launch until the process enters sleep state
# (i.e. hits the event loop's epoll_wait, meaning init + first frame is done).
#
# Usage: ./bench.sh <widget-binary> [args...]
# Examples:
#   ./bench.sh grimoire --drun
#   ./bench.sh wallrun --dir ~/walls --ext png
#   ./bench.sh panel

set -euo pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <widget> [args...]"
    exit 1
fi

RUNS=1000
times=()

for i in $(seq 1 "$RUNS"); do
    start=$(date +%s%N)
    "$@" &
    pid=$!

    # Poll until the process enters S (sleeping) state — means it's idle
    # in the event loop after initialization and first render.
    while [ -d "/proc/$pid" ]; do
        state=$(cut -d' ' -f3 /proc/$pid/stat 2>/dev/null || echo "")
        [ "$state" = "S" ] && break
    done

    end=$(date +%s%N)
    kill "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null || true

    ms=$(( (end - start) / 1000000 ))
    times+=("$ms")
    echo "  run $i: ${ms}ms"
done

# Stats
sorted=($(printf '%s\n' "${times[@]}" | sort -n))
sum=0
for t in "${times[@]}"; do sum=$((sum + t)); done
avg=$((sum / RUNS))
median=${sorted[$((RUNS / 2))]}
echo ""
echo "  avg: ${avg}ms  median: ${median}ms  min: ${sorted[0]}ms  max: ${sorted[-1]}ms"
