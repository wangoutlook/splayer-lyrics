#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p runtime

PIDFILE="runtime/pid"
if [[ -f "$PIDFILE" ]]; then
    old_pid="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [[ -n "$old_pid" ]] && kill -0 "$old_pid" 2>/dev/null; then
        # PID 会被复用：开机早期 2187 很可能是别的进程。
        # 必须确认它真的是 splayer-lyrics，否则误判为“已在运行”而拒绝启动。
        old_comm="$(cat "/proc/$old_pid/comm" 2>/dev/null || true)"
        old_cmdline="$(tr '\0' ' ' <"/proc/$old_pid/cmdline" 2>/dev/null || true)"
        if [[ "$old_comm" == "splayer-lyrics" || "$old_cmdline" == *"target/release/splayer-lyrics"* ]]; then
            echo "splayer-lyrics is already running (pid $old_pid)"
            exit 1
        fi
        echo "pid $old_pid is alive but not splayer-lyrics (comm=$old_comm); treating pid file as stale" >&2
    fi
    rm -f "$PIDFILE"
fi

if [[ ! -x target/release/splayer-lyrics ]]; then
    ./build.sh
fi

# Rotate before opening the append redirect below, so stderr and the app's
# own log file always point at the same inode.
LOGFILE="runtime/run.log"
if [[ -f "$LOGFILE" ]]; then
    size="$(stat -c%s "$LOGFILE" 2>/dev/null || echo 0)"
    if (( size > 5 * 1024 * 1024 )); then
        mv -f "$LOGFILE" "$LOGFILE.1"
    fi
fi

export GDK_BACKEND=wayland
export GSK_RENDERER="${SPLAYER_LYRICS_GSK_RENDERER:-cairo}"
export GTK_A11Y=none
export NO_AT_BRIDGE=1
export GTK_THEME=Adwaita
export RUST_BACKTRACE=1

setsid sh -c 'echo $$ > runtime/pid; exec ./target/release/splayer-lyrics "$@"' sh "$@" \
    >>runtime/run.log 2>&1 </dev/null &

sleep 0.6
if [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    echo "started splayer-lyrics (pid $(cat "$PIDFILE"))"
else
    echo "failed to start; see runtime/run.log" >&2
    exit 1
fi
