#!/bin/sh
# threadctl monitor — 实时监测 daemon 进程 CPU 开销 / 内存占用
#
# 用法:
#   ./scripts/monitor.sh                  # 每 2s 输出，无限
#   ./scripts/monitor.sh 1                # 每 1s 输出
#   ./scripts/monitor.sh 2 60             # 每 2s 输出，60s 后停止
#   ./scripts/monitor.sh 2 > cpu.log      # 输出到文件
#
# 输出列: 时间 pid CPU% 内存% RSS(kB) VSZ(kB)

INTERVAL=${1:-2}
DURATION=${2:-0}
ELAPSED=0

echo "=== threadctl monitor (interval=${INTERVAL}s, duration=${DURATION:-无限}s) ==="
printf "%-9s %-7s %5s %5s %8s %8s\n" "TIME" "PID" "CPU%" "MEM%" "RSS" "VSZ"

while true; do
    PID=$(pidof threadctl 2>/dev/null | awk '{print $1}')
    if [ -z "$PID" ]; then
        printf "%-9s %-7s %5s %5s %8s %8s\n" "$(date +%H:%M:%S)" "-" "-" "-" "-" "-"
    else
        STAT=$(ps -p "$PID" -o %cpu,%mem,rss,vsz --no-headers 2>/dev/null | tr -s ' ' | sed 's/^ //')
        if [ -n "$STAT" ]; then
            printf "%-9s %-7s %s\n" "$(date +%H:%M:%S)" "$PID" "$STAT" | awk '{printf "%-9s %-7s %5s %5s %8s %8s\n", $1, $2, $3, $4, $5, $6}'
        fi
    fi
    if [ "$DURATION" -gt 0 ]; then
        ELAPSED=$((ELAPSED + INTERVAL))
        [ "$ELAPSED" -ge "$DURATION" ] && break
    fi
    sleep "$INTERVAL"
done
