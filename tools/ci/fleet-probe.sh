#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# A network probe between the nodes of one shards-fleet group (fleet_probe): every node listens on
# ports across the relay's range 47100-47199 for a while; node 0 connects to every node on each of
# them, and every node to node 0. Reports reachability per port, the round-trip time, the address
# used (the fleet's private network or not) and the host firewall's state. Env: INDEX. $1: the
# rendezvous' peers file (node 0 has every node; the others have node 0 and themselves).
set -u
peers=$1
PORTS="47100 47101 47102 47103 47104 47150 47199"
listener=$(mktemp)
printf '%s\n' 'import socket, sys, time' \
  's = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)' \
  's.bind(("0.0.0.0", int(sys.argv[1]))); s.listen(64); s.settimeout(1)' \
  'end = time.time() + float(sys.argv[2])' \
  'while time.time() < end:' \
  '    try:' \
  '        c, a = s.accept(); c.sendall(b"ok\n"); c.close()' \
  '    except socket.timeout:' \
  '        pass' > "$listener"
for t in python3 nc ping; do printf '%s: %s\n' "$t" "$(command -v "$t" || echo none)"; done
ip -4 -o addr show | awk '{print "  " $2 " " $4}'
(sudo -n iptables -S INPUT 2>/dev/null || echo "  iptables: not readable") | head -n 8
(sudo -n ufw status 2>/dev/null || echo "  ufw: not readable") | head -n 3
for p in $PORTS; do python3 "$listener" "$p" 300 & done
sleep 20
while read -r idx ip key; do
  [ "$idx" = "$INDEX" ] && continue
  res=""
  for p in $PORTS; do
    ok=no
    for _ in $(seq 1 12); do
      if timeout 3 bash -c "exec 3<>/dev/tcp/$ip/$p && head -c 2 <&3" 2>/dev/null | grep -q ok; then ok=yes; break; fi
      sleep 5
    done
    res="$res $p:$ok"
  done
  rtt=$(ping -c 5 -q "$ip" 2>/dev/null | tail -n 1)
  echo "node $INDEX -> node $idx ($ip):$res; ping: ${rtt:-no reply}" | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
done < "$peers"
wait || true
