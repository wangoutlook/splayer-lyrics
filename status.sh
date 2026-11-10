#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

PIDFILE="runtime/pid"
if [[ -f "$PIDFILE" ]]; then
    pid="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        echo "pid: $pid (running)"
        echo "comm: $(cat "/proc/$pid/comm" 2>/dev/null || echo unknown)"
        ps -o pid,rss,vsz,pcpu,etime,comm -p "$pid" || true
        if [[ -r "/proc/$pid/smaps_rollup" ]]; then
            grep -E '^(Rss|Pss|Private_Clean|Private_Dirty):' "/proc/$pid/smaps_rollup" || true
        fi
    else
        echo "pid: ${pid:-?} (not running; stale pid file)"
    fi
else
    echo "pid: none (not running)"
fi

port="$(python3 -c 'import json;print(json.load(open("config.json"))["api"]["port"])' 2>/dev/null || echo 14558)"
if curl -fsS --max-time 2 "http://127.0.0.1:${port}/api/info" >/dev/null 2>&1; then
    echo "api: reachable on 127.0.0.1:${port}"
else
    echo "api: unreachable on 127.0.0.1:${port}"
fi

if command -v niri >/dev/null 2>&1; then
    layers="$(niri msg layers 2>/dev/null | grep -E 'splayer-lyrics' || true)"
    if [[ -n "$layers" ]]; then
        echo "niri layers:"
        echo "$layers"
    else
        echo "niri layers: splayer-lyrics namespaces not found"
    fi
fi
