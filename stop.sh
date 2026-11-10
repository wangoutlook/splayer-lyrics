#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

PIDFILE="runtime/pid"
if [[ ! -f "$PIDFILE" ]]; then
    echo "not running (no $PIDFILE)"
    exit 0
fi

pid="$(cat "$PIDFILE" 2>/dev/null || true)"
if [[ -z "$pid" ]] || ! kill -0 "$pid" 2>/dev/null; then
    echo "not running (stale pid ${pid:-?})"
    rm -f "$PIDFILE"
    exit 0
fi

comm="$(cat "/proc/$pid/comm" 2>/dev/null || true)"
cmdline="$(tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)"
if [[ "$comm" != "splayer-lyrics" && "$cmdline" != *"target/release/splayer-lyrics"* ]]; then
    echo "pid $pid does not look like splayer-lyrics (comm=$comm); refusing to kill" >&2
    exit 1
fi

kill -TERM "$pid"
for _ in $(seq 1 50); do
    if ! kill -0 "$pid" 2>/dev/null; then
        rm -f "$PIDFILE"
        echo "stopped (pid $pid)"
        exit 0
    fi
    sleep 0.1
done

echo "pid $pid is still alive after SIGTERM" >&2
exit 1
