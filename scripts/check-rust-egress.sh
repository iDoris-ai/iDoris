#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

for tool in strace setsid python3 curl; do
  command -v "$tool" >/dev/null 2>&1 || { echo "[egress] missing required tool: $tool" >&2; exit 1; }
done

tmp="$(mktemp -d)"
pids=()
groups=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  for group in "${groups[@]}"; do kill -- "-$group" 2>/dev/null || true; done
  rm -rf "$tmp"
}
trap cleanup EXIT

bin="${IDORIS_EGRESS_BIN:-${CARGO_TARGET_DIR:-$root/target}/release/idoris}"
if [ ! -x "$bin" ]; then
  cargo build --release --locked -p idoris-router
fi

free_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(('127.0.0.1', 0))
print(s.getsockname()[1])
s.close()
PY
}

nonloopback_ip() {
  python3 - <<'PY'
import socket
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.connect(('192.0.2.1', 9))
print(s.getsockname()[0])
s.close()
PY
}

free_port_for() {
  python3 - "$1" <<'PY'
import socket, sys
s = socket.socket()
s.bind((sys.argv[1], 0))
print(s.getsockname()[1])
s.close()
PY
}

write_policy() {
  cat >"$1" <<'YAML'
routing_policy:
  version: 1
  rules: []
  default: { tiers: [local, remote], fail_closed: true }
YAML
}

write_local_card() {
  local path="$1" port="$2"
  cat >"$path" <<YAML
provider:
  id: local-http
  family: local
  tier: local
  capabilities: [chat]
  privacy_class: local_only
  cost: { input_per_m: 0, output_per_m: 0 }
  locality: loopback
form: http_service
endpoint: "http://127.0.0.1:${port}"
version_pin: "fixture@1"
privacy_class: local_only
allowed_egress: [loopback]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }
YAML
}

write_remote_card() {
  local path="$1" host="$2" port="$3"
  cat >"$path" <<YAML
provider:
  id: remote-http
  family: other
  tier: remote
  capabilities: [chat]
  privacy_class: any
  cost: { input_per_m: 0, output_per_m: 0 }
  locality: remote
form: http_service
endpoint: "http://${host}:${port}"
version_pin: "fixture@1"
privacy_class: any
allowed_egress: [internet]
fallback_policy: fail_closed
fail_closed: true
load_policy: { mode: resident, keepalive: { pinned: true }, admission: coexist }
YAML
}

start_fake_upstream() {
  local port_file="$1"
  python3 -u - <<'PY' >"$port_file" 2>"$tmp/upstream.log" &
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == '/v1/models':
            body = json.dumps({'object':'list','data':[{'id':'fixture-model'}]}).encode()
            self.send_response(200); self.send_header('content-type','application/json')
            self.send_header('content-length', str(len(body))); self.end_headers(); self.wfile.write(body)
        else:
            self.send_response(404); self.end_headers()
    def log_message(self, *args):
        pass
server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(server.server_address[1], flush=True)
server.serve_forever()
PY
  pids+=("$!")
  for _ in $(seq 1 100); do [ -s "$port_file" ] && return 0; sleep 0.05; done
  echo "[egress] fake upstream failed to start" >&2; cat "$tmp/upstream.log" >&2; exit 1
}

wait_health() {
  local port="$1" log="$2"
  for _ in $(seq 1 120); do
    if curl -fsS "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then return 0; fi
    sleep 0.05
  done
  echo "[egress] idoris failed to become healthy" >&2
  cat "$log" >&2
  exit 1
}

start_traced_router() {
  local components="$1" policy="$2" trace="$3" log="$4" port="$5" db="$6"
  IDORIS_COMPONENTS_DIR="$components" \
  IDORIS_ROUTING_POLICY="$policy" \
  IDORIS_DB_PATH="$db" \
  IDORIS_PORT="$port" \
  setsid strace -f -s 256 -e trace=connect -o "$trace" "$bin" serve >"$log" 2>&1 &
  groups+=("$!")
  wait_health "$port" "$log"
}

stop_group() {
  local group="$1"
  kill -- "-$group" 2>/dev/null || true
  wait "$group" 2>/dev/null || true
}

check_trace() {
  local trace="$1" mode="$2"
  python3 - "$trace" "$mode" <<'PY'
import ipaddress, re, sys
path, mode = sys.argv[1:]
text = open(path, encoding='utf-8', errors='replace').read()
ips = []
for pattern in (r'inet_addr\("([^"]+)"\)', r'inet_pton\(AF_INET6, "([^"]+)"'):
    ips.extend(re.findall(pattern, text))
def loopback(value):
    ip = ipaddress.ip_address(value)
    mapped = getattr(ip, 'ipv4_mapped', None)
    return ip.is_loopback or (mapped is not None and mapped.is_loopback)
nonlocal_ips = [ip for ip in ips if not loopback(ip)]
if mode == 'local':
    if not ips:
        raise SystemExit('no IP connect() observed; probe is not load-bearing')
    if nonlocal_ips:
        raise SystemExit('unexpected non-loopback connect(s): ' + ', '.join(nonlocal_ips))
elif mode == 'positive':
    if not nonlocal_ips:
        raise SystemExit('positive control failed: no non-loopback connect() captured')
else:
    raise SystemExit('bad trace mode: ' + mode)
print('[egress] observed IP connects:', ', '.join(ips))
PY
}

mkdir -p "$tmp/local/components" "$tmp/remote/components"
write_policy "$tmp/local/policy.yaml"
write_policy "$tmp/remote/policy.yaml"
start_fake_upstream "$tmp/upstream-port"
upstream_port="$(cat "$tmp/upstream-port")"
write_local_card "$tmp/local/components/local.yaml" "$upstream_port"
remote_host="$(nonloopback_ip)"
remote_target_port="$(free_port_for "$remote_host")"
write_remote_card "$tmp/remote/components/remote.yaml" "$remote_host" "$remote_target_port"

local_port="$(free_port)"
start_traced_router "$tmp/local/components" "$tmp/local/policy.yaml" "$tmp/local.trace" "$tmp/local.log" "$local_port" "$tmp/local.sqlite3"
curl -fsS "http://127.0.0.1:${local_port}/health" >/dev/null
curl -fsS "http://127.0.0.1:${local_port}/v1/models" >/dev/null
stop_group "${groups[0]}"
check_trace "$tmp/local.trace" local

remote_port="$(free_port)"
start_traced_router "$tmp/remote/components" "$tmp/remote/policy.yaml" "$tmp/remote.trace" "$tmp/remote.log" "$remote_port" "$tmp/remote.sqlite3"
curl -sS "http://127.0.0.1:${remote_port}/v1/models" >/dev/null || true
stop_group "${groups[1]}"
check_trace "$tmp/remote.trace" positive

echo "[egress] PASS: local path stayed loopback and positive control caught remote connect"
