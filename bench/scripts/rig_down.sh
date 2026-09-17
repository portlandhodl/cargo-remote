#!/usr/bin/env bash
# Tear down the benchmark rig.
set -u
BENCH=/tmp/opencode/bench
for pidfile in "$BENCH/sshd/sshd.pid" "$BENCH/proxy-50ms.pid" "$BENCH/proxy-100ms.pid"; do
    if [[ -f $pidfile ]]; then
        kill "$(cat "$pidfile")" 2>/dev/null || true
        rm -f "$pidfile"
    fi
done
echo "rig down"
