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
extensions:
  _degradation: "Removing this fixture binding disables daily aliases."
  idoris_role_models: { daily: fixture-model }
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
  IDORIS_DEV_NO_KEY=1 \
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

# 云端档（计算服务 S7）。三件事，同一份配置、同一个「武装好的」云端档：
#   1. 启动、/health、/v1/models 之后：零出网（云端档只在请求路径上、本机失败之后才会被问到，从不在启动时拨号探测）；
#   2. 让本机失败的请求，只要不满足「隐私 any + 调用方显式同意」——`local_only`（显式的、没声明的）、`any` 但没有同意——
#      trace 里仍然只有回环连接：本机的失败不会悄悄变成一次出网；
#   3. 正控：同一份配置、同一个失败的本机，`any` + 同意的请求**会**去拨云端——trace 里必须抓到一个非回环的 connect()。
#      没有它，上面两条是「没拨号」还是「根本拨不出去」分不清：云端档的目标是一个本机的非回环 IP + 一个没人听的端口，
#      拨一次就会在 trace 里留下一条非回环的 connect()，所以第 2 条是真的可能变红的。
#
# 「武装好」= 云端档要拨出去，所有前置条件都满足，只剩闸门本身在拦：设备凭证文件在（0600、origin 对得上）、路由策略允许
# any 请求用 remote 层、请求带了一把允许 any 的本地数据密钥（免密钥开发请求永远不出网，所以不能用 IDORIS_DEV_NO_KEY）、
# 本机的 oMLX 指向一个关着的回环端口（本机服务不了：回落的前提）。
tier_target_port="$(free_port_for "$remote_host")"
tier_origin="https://${remote_host}:${tier_target_port}"
mkdir -p "$tmp/tier/components"
cat >"$tmp/tier/policy.yaml" <<'YAML'
routing_policy:
  version: 1
  rules:
    - if: { privacy: local_only }
      then: { tiers: [local, lora], fail_closed: true }
    - if: { privacy: any }
      then: { tiers: [local, remote] }
  default: { tiers: [local], fail_closed: true }
YAML
tier_closed_port="$(free_port)"
sed "s/:8000/:${tier_closed_port}/" "$root/config/components/omlx.yaml" >"$tmp/tier/components/omlx.yaml"
grep -q "127.0.0.1:${tier_closed_port}" "$tmp/tier/components/omlx.yaml" || {
  echo "[egress] could not point the local omlx card at a closed port" >&2
  exit 1
}
cat >"$tmp/tier/cloud-tier.yaml" <<YAML
schema_version: 1
base_url: ${tier_origin}/api
model: idoris/agent
state_dir: $tmp/tier/state
YAML
mkdir -p "$tmp/tier/state"
chmod 700 "$tmp/tier/state"
python3 - "$tmp/tier/state/device.json" "$tier_origin" <<'PY'
import json, os, sys
path, origin = sys.argv[1:]
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as out:
    json.dump({
        'version': 1, 'origin': origin, 'credential_id': 'vk_egress_check_0001',
        'membership': 'm-egress-check', 'pairing_id': 'pc_egress_check_0001',
        'paired_at_ms': 1, 'secret': 'idk_' + 'ab' * 32,
    }, out)
PY

# 一把允许 any 的本地数据密钥（和 launcher 一样：idoris key issue --spec-stdin，写进这个实例自己的库）。
issue_data_key() {
  local db="$1"
  printf '%s' '{"owner":"egress-check","tenant_id":null,"allowed_privacy":["local_only","any"],"allowed_roles":["fast","daily","deep","vision","embed","rerank","decide","auto"]}' \
    | IDORIS_DB_PATH="$db" "$bin" key issue --spec-stdin \
    | python3 -c 'import json, sys; print(json.load(sys.stdin)["secret"])'
}

# 起一个带云端档的 traced router（不设 IDORIS_DEV_NO_KEY：请求要带密钥，免密钥的开发请求永远不出网）。
# 进程组号写进全局变量 tier_group，调用方自己 stop_group。
start_traced_tier_router() {
  local trace="$1" log="$2" port="$3" db="$4"
  IDORIS_COMPONENTS_DIR="$tmp/tier/components" \
  IDORIS_ROUTING_POLICY="$tmp/tier/policy.yaml" \
  IDORIS_DB_PATH="$db" \
  IDORIS_PORT="$port" \
  IDORIS_CLOUD_TIER_CONFIG="$tmp/tier/cloud-tier.yaml" \
  setsid strace -f -s 256 -e trace=connect -o "$trace" "$bin" serve >"$log" 2>&1 &
  tier_group="$!"
  groups+=("$tier_group")
  wait_health "$port" "$log"
  grep -q "云端档已启用" "$log" || {
    echo "[egress] the cloud tier was not enabled in the traced router" >&2
    cat "$log" >&2
    exit 1
  }
}

# 发一个聊天请求，状态码打到 stdout，响应头和响应体留在 $tmp/chat.headers / $tmp/chat.body。
chat_once() {
  local port="$1" key="$2"
  shift 2
  rm -f "$tmp/chat.headers" "$tmp/chat.body"
  curl -sS -o "$tmp/chat.body" -D "$tmp/chat.headers" -w '%{http_code}' \
    -X POST "http://127.0.0.1:${port}/v1/chat/completions" \
    -H "authorization: Bearer ${key}" -H 'content-type: application/json' "$@" \
    -d '{"model":"small","messages":[{"role":"user","content":"egress check"}]}'
}

# 本机必须真的失败了（503 local_only_unavailable），否则「没出网」什么都证明不了；没出网的拒绝连 attempt 头都不该有。
expect_local_failure_and_no_cloud_attempt() {
  local label="$1" status
  shift
  status="$(chat_once "$@")"
  if [ "$status" != 503 ] || ! grep -q 'local_only_unavailable' "$tmp/chat.body"; then
    echo "[egress] $label: expected the local 503 local_only_unavailable failure, got $status: $(cat "$tmp/chat.body")" >&2
    exit 1
  fi
  if grep -qi '^x-idoris-cloud-attempt:' "$tmp/chat.headers"; then
    echo "[egress] $label: the cloud tier was attempted: $(grep -i '^x-idoris-cloud-attempt:' "$tmp/chat.headers")" >&2
    exit 1
  fi
}

# 实例 N（负控）：启动 + health + models，再发三个本机必定失败、但不满足出网条件的请求。trace 里只许有回环连接。
tier_neg_db="$tmp/tier-neg.sqlite3"
tier_neg_key="$(issue_data_key "$tier_neg_db")"
tier_neg_port="$(free_port)"
start_traced_tier_router "$tmp/tier-neg.trace" "$tmp/tier-neg.log" "$tier_neg_port" "$tier_neg_db"
curl -fsS "http://127.0.0.1:${tier_neg_port}/health" >/dev/null
curl -fsS -H "authorization: Bearer ${tier_neg_key}" "http://127.0.0.1:${tier_neg_port}/v1/models" >/dev/null
expect_local_failure_and_no_cloud_attempt "local_only (explicit) + consent" "$tier_neg_port" "$tier_neg_key" \
  -H 'x-idoris-privacy: local_only' -H 'x-idoris-cloud-fallback: allow'
expect_local_failure_and_no_cloud_attempt "no privacy declared (defaults to local_only) + consent" "$tier_neg_port" "$tier_neg_key" \
  -H 'x-idoris-cloud-fallback: allow'
expect_local_failure_and_no_cloud_attempt "any without consent" "$tier_neg_port" "$tier_neg_key" \
  -H 'x-idoris-privacy: any'
stop_group "$tier_group"
check_trace "$tmp/tier-neg.trace" local

# 实例 P（正控）：同一份配置、同一个失败的本机；再发 any + 同意：云端档真的会去拨。
# 响应里要有 X-iDoris-Cloud-Attempt（云端档确实尝试了），trace 里要抓到非回环的 connect()。
tier_pos_db="$tmp/tier-pos.sqlite3"
tier_pos_key="$(issue_data_key "$tier_pos_db")"
tier_pos_port="$(free_port)"
start_traced_tier_router "$tmp/tier-pos.trace" "$tmp/tier-pos.log" "$tier_pos_port" "$tier_pos_db"
pos_status="$(chat_once "$tier_pos_port" "$tier_pos_key" -H 'x-idoris-privacy: any' -H 'x-idoris-cloud-fallback: allow')"
if [ "$pos_status" != 503 ] || ! grep -qi '^x-idoris-cloud-attempt:' "$tmp/chat.headers"; then
  echo "[egress] positive control: an any + consent request that the local node cannot serve did not reach the cloud tier (status $pos_status): $(cat "$tmp/chat.body")" >&2
  exit 1
fi
stop_group "$tier_group"
check_trace "$tmp/tier-pos.trace" positive

echo "[egress] PASS: local path stayed loopback, positive control caught remote connect, and an armed cloud tier dialled nothing at start-up nor for local_only / unconsented requests (and did dial for any + consent)"
