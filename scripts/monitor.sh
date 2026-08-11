#!/bin/sh
# threadctl monitor — 实时监测 daemon CPU 开销 / 内存占用（Android root 版）
#
# 用法（Android 端）:
#   su -c ./scripts/monitor.sh              # root 直接跑
#   ./scripts/monitor.sh                    # 非 root 自动 su 提权（Termux 场景）
#   ./scripts/monitor.sh 1                  # 每 1s 输出
#   ./scripts/monitor.sh 2 60               # 每 2s 输出，60s 后停止
#   TC_PIDFILE=/path/threadctl.pid ./scripts/monitor.sh   # 指定 pid 文件
#   ./scripts/monitor.sh > /sdcard/cpu.log  # 输出到文件
#
# 输出列: TIME PID CPU% MEM% RSS(kB) VSZ(kB)
# 说明:
#   - CPU% 用 /proc/<pid>/stat utime+stime 差分计算（比 toybox ps %cpu 准）
#   - 必须 root 才能读 daemon 进程的 /proc（SELinux：u0_aXXX 读 root 进程被拒）
#   - daemon pid 发现：pid 文件优先（TC_PIDFILE/常见位置），回退 ps 扫描

INTERVAL=${1:-2}
DURATION=${2:-0}
ELAPSED=0

# ── root 检测 + 自动提权（非 root 自动 su 重启自己）──
if [ "$(id -u)" -ne 0 ]; then
    echo "非 root——尝试 su 提权（Android daemon 的 /proc 需 root 读取）..."
    exec su -c "sh '$0' $INTERVAL $DURATION"
fi

# ── daemon pid 发现：pid 文件优先 → pidof → ps 扫描 ──
find_pid() {
    # 1) 显式 pid 文件（TC_PIDFILE 或常见位置）
    for f in "$TC_PIDFILE" ./threadctl.pid /data/threadctl测试/threadctl.pid; do
        if [ -n "$f" ] && [ -f "$f" ]; then
            PID=$(cat "$f" 2>/dev/null | tr -d '[:space:]')
            [ -n "$PID" ] && [ -d "/proc/$PID" ] && echo "$PID" && return
        fi
    done
    # 2) pidof（root 下可用）
    PID=$(pidof threadctl 2>/dev/null | awk '{print $1}')
    [ -n "$PID" ] && echo "$PID" && return
    # 3) ps 扫描——toybox 格式 "USER PID PPID ..."，PID 在 $2
    ps -A 2>/dev/null | grep -E "[t]hreadctl" | awk '{print $2}' | head -1
}

# ── HZ 探测（toybox getconf 可能缺失；Android 内核通常 100/250）──
HZ=$(getconf CLK_TCK 2>/dev/null || echo 100)
[ "$HZ" -lt 50 ] && HZ=100

# ── CPU ticks（stat 字段 14+15 = utime+stime，`(` 后）──
cpu_ticks() {
    awk '{print $14+$15}' "/proc/$1/stat" 2>/dev/null
}

# ── 内存（status 字段）──
mem_kb() {
    grep -E "^VmRSS:" "/proc/$1/status" 2>/dev/null | awk '{print $2}'
}

# ── 单次采样 ──
sample() {
    PID=$(find_pid)
    if [ -z "$PID" ] || [ ! -d "/proc/$PID" ]; then
        printf "%-9s %-7s %5s %5s %8s %8s\n" "$(date +%H:%M:%S)" "-" "-" "-" "-" "-"
        return
    fi
    T1=$(cpu_ticks "$PID")
    RSS=$(mem_kb "$PID")
    sleep "$INTERVAL"
    T2=$(cpu_ticks "$PID")
    # CPU% = 差分 ticks / HZ / 间隔 * 100（单核百分比）
    if [ -n "$T1" ] && [ -n "$T2" ] && [ "$T2" -ge "$T1" ] 2>/dev/null; then
        CPU=$(( (T2 - T1) * 100 / HZ / INTERVAL ))
    else
        CPU="-"
    fi
    [ -z "$RSS" ] && RSS="-"
    # MEM%（总内存——Android 常见 8/12/16GB，从 /proc/meminfo 读）
    MTOTAL=$(grep -E "^MemTotal:" /proc/meminfo 2>/dev/null | awk '{print $2}')
    if [ -n "$MTOTAL" ] && [ "$RSS" != "-" ]; then
        MEMP=$(( RSS * 100 / MTOTAL ))
    else
        MEMP="-"
    fi
    VSZ=$(grep -E "^VmSize:" "/proc/$PID/status" 2>/dev/null | awk '{print $2}')
    [ -z "$VSZ" ] && VSZ="-"
    printf "%-9s %-7s %5s %5s %8s %8s\n" "$(date +%H:%M:%S)" "$PID" "$CPU" "$MEMP" "$RSS" "$VSZ"
}

echo "=== threadctl monitor (interval=${INTERVAL}s, duration=${DURATION:-无限}s, hz=$HZ, root) ==="
printf "%-9s %-7s %5s %5s %8s %8s\n" "TIME" "PID" "CPU%" "MEM%" "RSS" "VSZ"

if [ "$DURATION" -gt 0 ]; then
    while [ "$ELAPSED" -lt "$DURATION" ]; do
        sample
        ELAPSED=$((ELAPSED + INTERVAL))
    done
else
    while true; do
        sample
    done
fi
