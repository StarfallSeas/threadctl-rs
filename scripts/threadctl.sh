#!/bin/sh
# threadctl — daemon 启动/停止/重启/状态脚本
#
# 部署要求：本脚本与 threadctl 二进制放在同一目录（可选：threadctl-ebpf
# .bpf.o 也放同目录启用内核事件源；缺失时自动回退 /proc 轮询）。
# 日志输出到同目录：threadctl.log；PID 文件：threadctl.pid。
#
# 用法（需要 root——Android 下用 su 包裹，如 `su -c './threadctl.sh start'`）：
#   ./threadctl.sh start [-c config.kdl]   启动（默认配置 = 同目录 threadctl.kdl）
#   ./threadctl.sh stop                    停止
#   ./threadctl.sh restart [-c config.kdl] 重启
#   ./threadctl.sh status                  查看状态
#   ./threadctl.sh logs                    跟随日志（tail -f）
#
# 兼容：Android（Magisk/root shell）与 Linux（root）。

# 定位脚本所在目录（程序同目录），并切换进去
DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$DIR" || exit 1

BIN="$DIR/threadctl"
EBPF="$DIR/threadctl-ebpf"
LOG="$DIR/threadctl.log"
PID="$DIR/threadctl.pid"
CONF="${TC_CONF:-$DIR/threadctl.kdl}"     # 环境变量 TC_CONF 或默认同目录配置

LOG_MAX_BYTES=5242880   # 日志轮转阈值 5MB

log() {
    echo "[$(date '+%F %T')] $*" | tee -a "$LOG"
}

# ── root 检测（脚本须在 root 下运行；su 由用户在外部调用）──
require_root() {
    if [ "$(id -u 2>/dev/null)" != "0" ]; then
        log "ERROR: 需要 root——请以 root 运行（Android: su -c './threadctl.sh $1'）"
        return 1
    fi
    return 0
}

# ── 日志轮转：超限时保留一份 .old ──
rotate_log() {
    [ -f "$LOG" ] || return 0
    SIZE=$(wc -c < "$LOG" 2>/dev/null || echo 0)
    if [ "$SIZE" -gt "$LOG_MAX_BYTES" ]; then
        mv -f "$LOG" "$LOG.old"
        log "日志超 ${LOG_MAX_BYTES}B，轮转到 $LOG.old"
    fi
}

daemon_pid() {
    [ -f "$PID" ] && cat "$PID" 2>/dev/null || echo ""
}

is_running() {
    P=$(daemon_pid)
    [ -n "$P" ] && kill -0 "$P" 2>/dev/null
}

start() {
    require_root start || return 1
    rotate_log
    log "=== threadctl start ==="

    if is_running; then
        log "已在运行 (pid $(daemon_pid))——先 stop 再 start，或直接 restart"
        return 1
    fi

    if [ ! -x "$BIN" ]; then
        log "ERROR: $BIN 不存在或不可执行（请把脚本与二进制放同目录）"
        return 1
    fi

    if [ -f "$CONF" ]; then
        CONF_OPT="-c $CONF"
        log "配置: $CONF"
    else
        CONF_OPT=""
        log "WARN: 未找到配置 $CONF——daemon 将用内置默认；可用 -c 指定或设 TC_CONF"
    fi

    if [ -f "$EBPF" ]; then
        log "eBPF 程序: 存在（内核事件源，near-real-time 事件发现）"
    else
        log "WARN: 未找到 $EBPF——回退 /proc 轮询（可将 threadctl-ebpf 复制到本目录）"
    fi

    # 后台启动（nohup 脱离 SIGHUP——否则 su/Termux 会话退出会杀死 daemon），
    # 输出重定向到同目录日志；记录 PID
    # shellcheck disable=SC2086  # CONF_OPT 需分字（-c 与路径两个参数）
    nohup "$BIN" $CONF_OPT >>"$LOG" 2>&1 &
    BPID=$!
    echo "$BPID" > "$PID"

    # 短暂等待验证存活（daemon 启动失败会立即退出）
    sleep 1
    if kill -0 "$BPID" 2>/dev/null; then
        log "启动成功 (pid $BPID)——日志: $LOG"
    else
        log "启动失败（进程退出）——日志尾部："
        tail -5 "$LOG" | sed 's/^/    /'
        rm -f "$PID"
        return 1
    fi
}

stop() {
    require_root stop || return 1
    P=$(daemon_pid)
    if [ -z "$P" ] || ! kill -0 "$P" 2>/dev/null; then
        log "未在运行"
        rm -f "$PID"
        return 0
    fi
    log "停止中 (pid $P)..."
    kill "$P" 2>/dev/null
    # 优雅退出等待（daemon SIGTERM 有 cleanup）
    i=0
    while kill -0 "$P" 2>/dev/null && [ "$i" -lt 10 ]; do
        sleep 0.5
        i=$((i + 1))
    done
    if kill -0 "$P" 2>/dev/null; then
        log "未响应，强制 kill"
        kill -9 "$P" 2>/dev/null
    fi
    rm -f "$PID"
    log "已停止"
}

status() {
    P=$(daemon_pid)
    if [ -n "$P" ] && kill -0 "$P" 2>/dev/null; then
        log "运行中 (pid $P)"
        log "二进制: $BIN"
        log "配置:   $CONF"
        log "日志:   $LOG"
        # 尽力读取进程信息（Android ps 选项差异大，失败可忽略）
        ps -p "$P" -o pid,etime,args 2>/dev/null | sed 's/^/    /' || true
    else
        log "未运行"
    fi
}

case "${1:-}" in
    start)
        shift
        while [ $# -gt 0 ]; do
            case "$1" in
                -c) CONF="$2"; shift 2 ;;
                TC_DEBUG=1|--debug) export TC_DEBUG=1; shift ;;
                *) shift ;;
            esac
        done
        start
        ;;
    stop)
        stop
        ;;
    restart)
        shift
        while [ $# -gt 0 ]; do
            case "$1" in
                -c) CONF="$2"; shift 2 ;;
                TC_DEBUG=1|--debug) export TC_DEBUG=1; shift ;;
                *) shift ;;
            esac
        done
        stop
        start
        ;;
    status)
        status
        ;;
    logs)
        exec tail -f "$LOG"
        ;;
    *)
        echo "用法: $0 {start [-c cfg]|stop|restart [-c cfg]|status|logs}"
        exit 1
        ;;
esac
