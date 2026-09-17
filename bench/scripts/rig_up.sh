#!/usr/bin/env bash
# Bring up the localhost benchmark rig:
#   - throwaway userspace sshd on 127.0.0.1:2222 (0ms RTT baseline)
#   - latency proxies on 2223 (25ms/way => ~50ms RTT) and 2224 (50ms/way => ~100ms RTT)
# All state lives under /tmp/opencode/bench. Nothing outside /tmp is touched.
set -euo pipefail

BENCH=/tmp/opencode/bench
SSHD_DIR=$BENCH/sshd
KEYS=$BENCH/keys
REMOTE=$BENCH/remote
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$SSHD_DIR" "$KEYS" "$REMOTE/builds" "$BENCH/logs"
chmod 700 "$SSHD_DIR" "$KEYS"

# --- keys -----------------------------------------------------------------
if [[ ! -f $SSHD_DIR/host_key ]]; then
    ssh-keygen -q -t ed25519 -N '' -f "$SSHD_DIR/host_key"
fi
if [[ ! -f $KEYS/id_ed25519 ]]; then
    ssh-keygen -q -t ed25519 -N '' -f "$KEYS/id_ed25519"
    cp "$KEYS/id_ed25519.pub" "$KEYS/authorized_keys"
    chmod 600 "$KEYS/authorized_keys"
fi

# --- sshd ------------------------------------------------------------------
cat > "$SSHD_DIR/sshd_config" <<EOF
Port 2222
ListenAddress 127.0.0.1
HostKey $SSHD_DIR/host_key
AuthorizedKeysFile $KEYS/authorized_keys
PasswordAuthentication no
PubkeyAuthentication yes
UsePAM no
StrictModes no
PidFile $SSHD_DIR/sshd.pid
LogLevel ERROR
Subsystem sftp internal-sftp
EOF

if [[ -f $SSHD_DIR/sshd.pid ]] && kill -0 "$(cat "$SSHD_DIR/sshd.pid")" 2>/dev/null; then
    echo "sshd already running"
else
    /usr/sbin/sshd -f "$SSHD_DIR/sshd_config" -E "$BENCH/logs/sshd.log"
    echo "sshd up on 127.0.0.1:2222"
fi

# --- latency proxies --------------------------------------------------------
start_proxy() { # port, target, delay_ms, pidfile
    if [[ -f $4 ]] && kill -0 "$(cat "$4")" 2>/dev/null; then return; fi
    nohup python3 "$SCRIPT_DIR/latency_proxy.py" "$1" "$2" "$3" \
        > "$BENCH/logs/proxy-$1.log" 2>&1 &
    echo $! > "$4"
}
start_proxy 2223 2222 25 "$BENCH/proxy-50ms.pid"
start_proxy 2224 2222 50 "$BENCH/proxy-100ms.pid"

# --- client ssh config -------------------------------------------------------
cat > "$BENCH/ssh_config" <<EOF
Host bench-direct bench-50ms bench-100ms
    IdentityFile $KEYS/id_ed25519
    IdentitiesOnly yes
    StrictHostKeyChecking no
    UserKnownHostsFile $BENCH/known_hosts
    LogLevel ERROR

Host bench-direct
    HostName 127.0.0.1
    Port 2222

Host bench-50ms
    HostName 127.0.0.1
    Port 2223

Host bench-100ms
    HostName 127.0.0.1
    Port 2224
EOF

sleep 0.3
ssh -F "$BENCH/ssh_config" bench-direct true && echo "rig OK: bench-direct reachable"
ssh -F "$BENCH/ssh_config" bench-50ms true && echo "rig OK: bench-50ms reachable"
