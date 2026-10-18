#!/usr/bin/env python3
"""Real Agent24 CLI/daemon -> iDoris native Supervisor acceptance, Python 3.9+.

Run --help for the final-artifact launch contract. Missing artifacts exit 3
BLOCKED. --self-test tests this harness only and NEVER emits product PASS.
No build, backend download, signing, service install or release operations.

Agent24 chat cannot set tools/max_tokens/response_format/disable_thinking;
nondefault parameter evidence is explicitly DIRECT iDoris evidence only.
SHA inputs bind bytes; source SHAs are operator build-provenance declarations,
not cryptographic proof that those bytes were compiled from those sources.

Exit codes: 0 listed real criteria passed; 1 failed criterion; 3 BLOCKED;
4 PARTIAL (diagnostic artifacts or skipped native deadline). Diagnostic
builds MUST use --diagnostic-artifacts and --agent24-source-diff-sha256.
Real model quality, final 0.3.1 release provenance, signing and manual product
approval remain outside this harness even when its listed criteria pass.
"""
import argparse
import contextlib
import hashlib
import http.client
import http.server
import io
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import uuid

MODEL_A = 'fixture-native-core'
MODEL_B = 'fixture-native-next'
MARKER = 'native-consumer-fixture-ok'
LIMIT = 1024 * 1024


class Blocked(RuntimeError):
    pass


class Failed(RuntimeError):
    pass


def require(value, reason):
    if not value:
        raise Failed(reason)


def sha256(path):
    checksum = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(LIMIT), b''):
            checksum.update(chunk)
    return checksum.hexdigest()


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def wait_for(predicate, seconds, reason):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(.05)
    raise Failed(reason)


def environment(home):
    home.mkdir(parents=True, exist_ok=True)
    # Allowlist, never inherit provider credentials, proxy vars or caller config.
    return {'HOME': str(home), 'PATH': '/usr/bin:/bin:/usr/sbin:/sbin',
            'TMPDIR': str(home), 'XDG_CONFIG_HOME': str(home / 'config'),
            'XDG_DATA_HOME': str(home / 'data'), 'XDG_CACHE_HOME': str(home / 'cache'),
            'NO_PROXY': '*', 'no_proxy': '*', 'LANG': 'en_US.UTF-8'}


class Process:
    """Own one session/group; bounded TERM/KILL, reap leader, verify group gone."""
    def __init__(self, argv, env, cwd, stdin=None, separate_stderr=False):
        self.output = tempfile.TemporaryFile()
        self.error = tempfile.TemporaryFile() if separate_stderr else None
        self.proc = None
        self.closed = False
        try:
            self.proc = subprocess.Popen(argv, env=env, cwd=cwd, stdin=subprocess.PIPE if stdin else subprocess.DEVNULL,
                                         stdout=self.output, stderr=self.error or self.output, start_new_session=True)
            if stdin:
                self.proc.stdin.write(stdin)
                self.proc.stdin.close()
        except BaseException:
            self.close()
            raise

    def output_bytes(self):
        self.output.seek(0)
        data = self.output.read(LIMIT + 1)
        require(len(data) <= LIMIT, 'subprocess output limit exceeded')
        return data

    def text(self):
        return self.output_bytes().decode('utf-8', errors='replace')

    def group_alive(self):
        try:
            os.killpg(self.proc.pid, 0)
            return True
        except ProcessLookupError:
            return False

    def close(self):
        if self.closed:
            return
        self.closed = True
        try:
            if self.proc is not None:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(self.proc.pid, signal.SIGTERM)
                try:
                    self.proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    pass
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait(timeout=5)
                wait_for(lambda: not self.group_alive(), 5, 'owned process group remains after cleanup')
        finally:
            self.output.close()
            if self.error:
                self.error.close()


def command(argv, env, cwd, timeout=20, stdin=None, stdout_only=False):
    child = Process(argv, env, cwd, stdin, separate_stderr=stdout_only)
    try:
        try:
            code = child.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            raise Failed('owned command deadline exceeded') from None
        return code, child.output_bytes() if stdout_only else child.text()
    finally:
        child.close()


def request_http(port_number, path, method='GET', body=None, headers=None, timeout=5):
    conn = http.client.HTTPConnection('127.0.0.1', port_number, timeout=timeout)
    try:
        data = json.dumps(body).encode() if body is not None else None
        conn.request(method, path, body=data, headers={'Content-Type': 'application/json', **(headers or {})})
        response = conn.getresponse()
        payload = response.read(LIMIT + 1)
        require(len(payload) <= LIMIT, 'HTTP response limit exceeded')
        return response.status, dict((k.lower(), v) for k, v in response.getheaders()), json.loads(payload)
    finally:
        conn.close()


class Server:
    def start(self, handler):
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), handler)
        self.server.daemon_threads = True
        self.port = self.server.server_port
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(3)
        require(not self.thread.is_alive(), 'owned HTTP listener did not stop')


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, value, headers=None):
        raw = json.dumps(value).encode()
        self.send_response(status)
        for key, val in (headers or {}).items():
            if key.lower() not in ('content-length', 'connection', 'transfer-encoding', 'server', 'date'):
                self.send_header(key, val)
        self.send_header('Content-Length', str(len(raw)))
        self.send_header('Connection', 'close')
        self.end_headers()
        with contextlib.suppress(BrokenPipeError, ConnectionResetError):
            self.wfile.write(raw)

    def body(self):
        size = int(self.headers.get('Content-Length', 0))
        require(0 <= size <= LIMIT, 'fixture request limit exceeded')
        return json.loads(self.rfile.read(size)) if size else {}


class Fixture(Server):
    """Owned oMLX 0.6.4 lifecycle + OAI HTTP fixture, NEVER resident bypass."""
    def __init__(self, footprint):
        self.footprint = footprint
        self.loaded = set()
        self.events = []
        self.virtual_key_leaked = False
        self.lock = threading.Lock()
        self.hold = False
        self.post_seen = threading.Event()
        self.disconnected = threading.Event()
        self.stopping = threading.Event()
        owner = self

        class Backend(Handler):
            def do_GET(self):
                with owner.lock:
                    owner.events.append(('GET', self.path, None))
                    if self.path == '/v1/models/status':
                        value = {'models': [{'id': model, 'loaded': model in owner.loaded, 'pinned': False,
                                             'estimated_size': owner.footprint} for model in (MODEL_A, MODEL_B)]}
                    elif self.path == '/api/status':
                        value = {'loaded_models': sorted(owner.loaded), 'model_memory_used': len(owner.loaded) * owner.footprint,
                                 'model_memory_max': 0, 'pressure': 'ok'}
                    elif self.path == '/v1/models':
                        value = {'object': 'list', 'data': [{'id': m, 'object': 'model'} for m in (MODEL_A, MODEL_B)]}
                    else:
                        self.reply(404, {'error': 'unknown fixture route'})
                        return
                self.reply(200, value)

            def do_POST(self):
                body = self.body()
                with owner.lock:
                    owner.virtual_key_leaked |= self.headers.get('Authorization', '').startswith('Bearer idk_')
                    owner.events.append(('POST', self.path, body))
                    match = re.fullmatch(r'/v1/models/([^/]+)/(load|unload)', self.path)
                    if match and match[1] in (MODEL_A, MODEL_B):
                        if match[2] == 'load':
                            owner.loaded.add(match[1])
                        else:
                            owner.loaded.discard(match[1])
                        self.reply(200, {'ok': True})
                        return
                    if self.path != '/v1/chat/completions':
                        self.reply(404, {'error': 'unknown fixture route'})
                        return
                    if body.get('model') not in owner.loaded:
                        self.reply(409, {'error': 'model not loaded'})
                        return
                    hold = owner.hold
                owner.post_seen.set()
                if hold:
                    end = time.monotonic() + 145
                    while not owner.stopping.is_set() and time.monotonic() < end:
                        readable, _, _ = select.select([self.connection], [], [], .1)
                        if readable:
                            try:
                                closed = not self.connection.recv(1, socket.MSG_PEEK)
                            except (ConnectionResetError, OSError):
                                closed = True
                            if closed:
                                owner.disconnected.set()
                                return
                    return
                self.reply(200, {'id': 'fixture-native-chat', 'object': 'chat.completion', 'model': body['model'],
                                 'choices': [{'index': 0, 'finish_reason': 'stop',
                                              'message': {'role': 'assistant', 'content': MARKER}}],
                                 'usage': {'prompt_tokens': 1, 'completion_tokens': 1, 'total_tokens': 2}})
        self.start(Backend)

    def posts(self, suffix):
        with self.lock:
            return [event for event in self.events if event[0] == 'POST' and event[1].endswith(suffix)]

    def close(self):
        self.stopping.set()
        super().close()


class Tap(Server):
    """Observes real consumer payload/headers without modifying them."""
    def __init__(self, upstream, key):
        self.events = []
        owner = self
        class Proxy(Handler):
            def forward(self):
                body = self.body() if self.command == 'POST' else None
                headers = {k: v for k, v in self.headers.items() if k.lower() not in ('host', 'content-length', 'connection')}
                entry = {'method': self.command, 'path': self.path, 'body': body,
                         'real_key': self.headers.get('Authorization') == 'Bearer ' + key,
                         'privacy': self.headers.get('X-iDoris-Privacy')}
                owner.events.append(entry)
                try:
                    status, response_headers, result = request_http(upstream, self.path, self.command, body, headers, timeout=20)
                    entry.update(status=status, locality=response_headers.get('x-idoris-served-locality'),
                                 record_id=response_headers.get('x-idoris-record-id'), actual_model=result.get('model'))
                    error = result.get('error')
                    if isinstance(error, dict):
                        entry['error_reason'] = error.get('reason_code')
                    if self.path == '/health':
                        entry['identity'] = result
                    self.reply(status, result, response_headers)
                except (OSError, ValueError, http.client.HTTPException):
                    entry['transport_failed'] = True
                    self.reply(502, {'error': 'owned tap upstream unavailable'})
            do_GET = do_POST = forward
        self.start(Proxy)


class Canaries:
    """Fail BLOCKED if ANY default-backend listener cannot be owned."""
    def __init__(self, ports=(8000, 11434)):
        self.hits = 0
        self.sockets = []
        self.threads = []
        self.stopping = threading.Event()
        try:
            for family, address in ((socket.AF_INET, '127.0.0.1'), (socket.AF_INET6, '::1')):
                for number in ports:
                    listener = socket.socket(family)
                    self.sockets.append(listener)
                    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                    if family == socket.AF_INET6:
                        listener.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
                    listener.bind((address, number))
                    listener.listen(8)
                    listener.settimeout(.1)
                    thread = threading.Thread(target=self.serve, args=(listener,), daemon=True)
                    self.threads.append(thread)
                    thread.start()
        except OSError:
            self.close()
            raise Blocked('default backend canary unavailable; fallback absence cannot be accepted') from None

    def serve(self, listener):
        while not self.stopping.is_set():
            try:
                conn, _ = listener.accept()
                self.hits += 1
                conn.close()
            except (socket.timeout, OSError):
                continue

    def close(self):
        self.stopping.set()
        for listener in self.sockets:
            listener.close()
        for thread in self.threads:
            thread.join(2)


def artifacts(args, root):
    evidence, binaries = {}, {}
    bindir = root / 'bin'
    bindir.mkdir()
    for name in ('idoris', 'agent24', 'agent24d'):
        value, expected = getattr(args, name + '_bin'), getattr(args, name + '_sha256')
        if not value or not expected:
            raise Blocked('all three final executable paths and SHA256 inputs are required')
        source = Path(value).resolve(strict=True)
        if not source.is_file() or not os.access(source, os.X_OK):
            raise Blocked('supplied artifact is not executable')
        require(re.fullmatch('[0-9a-f]{64}', expected), 'invalid expected executable SHA256')
        checksum = sha256(source)
        require(checksum == expected, 'supplied executable SHA256 mismatch')
        dest = bindir / name
        shutil.copyfile(source, dest)
        dest.chmod(0o700)
        require(sha256(dest) == checksum, 'executable changed during isolated copy')
        binaries[name] = str(dest)
        evidence[name] = {'sha256': checksum, 'verified_isolated_copy': True}
    for name in ('idoris', 'agent24'):
        declared = getattr(args, name + '_source_sha')
        require(declared and re.fullmatch('[0-9a-f]{40}', declared), 'full source provenance declarations required')
        evidence[name]['declared_source_sha'] = declared
    evidence['agent24d']['declared_source_sha'] = args.agent24_source_sha
    return binaries, evidence


class BudgetHandoff:
    """One exact LF-terminated sender frame; retain the value bytes for env."""
    def __init__(self, raw):
        match = re.fullmatch(rb'IDORIS_ALLOCATED_BUDGET_BYTES=([0-9]+)\n', raw)
        require(match, 'hardware handoff stdout was not one exact LF-terminated allocation assignment')
        self.raw = raw
        self.value = match[1].decode('ascii')
        self.amount = int(self.value)
        require(self.amount < 2 ** 64, 'hardware handoff exceeds unsigned byte-count range')

    def apply_to(self, env):
        name, value = self.raw[:-1].split(b'=', 1)
        env[name.decode('ascii')] = value.decode('ascii')


def budget(binary, env, root, reservation):
    code, raw = command([binary, 'hw', 'idoris-env', '--core-reserved-bytes', str(reservation)], env, root, stdout_only=True)
    require(code == 0, 'real Agent24 hardware handoff failed')
    return BudgetHandoff(raw)


def consumer(binary, env, root):
    return command([binary, 'chat', 'fixed native acceptance prompt', '--model', 'idoris/daily'], env, root, timeout=25)


def payload(model=MODEL_A):
    return {'model': model, 'messages': [{'role': 'user', 'content': 'fixed native acceptance prompt'}],
            'stream': False, 'tools': [{'type': 'function', 'function': {'name': 'fixture_tool', 'description': 'fixture',
                      'parameters': {'type': 'object', 'properties': {}, 'additionalProperties': False}}}],
            'max_tokens': 37, 'response_format': {'type': 'json_object'},
            'chat_template_kwargs': {'enable_thinking': False}, 'disable_thinking': True}


def scenario(args, binaries, root, config, name, handoff, footprint, reservation, result):
    allocation = handoff.amount
    work = root / name
    work.mkdir()
    env = environment(work / 'idoris-home')
    fixture = Fixture(footprint)
    tap = None
    gateway = daemon = None
    result.update(name=name, allocated_budget_bytes=allocation, core_reserved_bytes=reservation,
                  fixture_estimated_size_bytes=footprint, checks={})
    try:
        cards = work / 'components'
        cards.mkdir()
        original = (config / 'components/omlx.yaml').read_text()
        require('mode: on_demand' in original and 'id: omlx' in original and 'http://127.0.0.1:8000' in original,
                'input omlx card does not match inspected on_demand contract')
        card = original.replace('http://127.0.0.1:8000', 'http://127.0.0.1:%d' % fixture.port)
        require('idoris_role_models' not in card, 'source card already binds roles; explicit fixture binding required')
        card += '\nextensions:\n  _degradation: "owned lifecycle fixture only"\n  idoris_role_models:\n    daily: %s\n    deep: %s\n' % (MODEL_A, MODEL_B)
        (cards / 'omlx.yaml').write_text(card)
        gateway_port = port()
        env.update(IDORIS_COMPONENTS_DIR=str(cards), IDORIS_ROUTING_POLICY=str(config / 'routing-policy.yaml'),
                   IDORIS_CATALOG=str(config / 'catalog.yaml'), IDORIS_PORT=str(gateway_port),
                   IDORIS_ADMIN_PORT=str(port()), IDORIS_BIND_HOST='127.0.0.1', IDORIS_DEPLOY_MODE='personal',
                   IDORIS_DB_PATH=str(work / 'idoris.sqlite'), IDORIS_STATE_DIR=str(work / 'idoris-state'))
        handoff.apply_to(env)
        key_spec = {'owner': 'native-consumer-fixture', 'allowed_privacy': ['local_only'], 'allowed_roles': ['daily', 'deep']}
        code, issued = command([binaries['idoris'], 'key', 'issue', '--spec-stdin'], env, work,
                               stdin=json.dumps(key_spec).encode())
        require(code == 0, 'real iDoris key issuance failed')
        key = json.loads(issued)['secret']
        require(isinstance(key, str) and key.startswith('idk_'), 'key issuance did not return a real data key')
        gateway = Process([binaries['idoris'], 'serve', '--admin-token-stdin'], env, work,
                          stdin=(uuid.uuid4().hex + uuid.uuid4().hex + '\n').encode())
        health = {}
        def ready():
            require(gateway.proc.poll() is None, 'iDoris exited before readiness')
            try:
                code, _, identity = request_http(gateway_port, '/health', timeout=1)
                if code == 200:
                    health.update(identity)
                    return True
            except (OSError, ValueError, http.client.HTTPException):
                pass
            return False
        wait_for(ready, 20, 'iDoris readiness deadline')
        checks = result['checks']
        checks['exact_sender_assignment_handed_to_idoris_env'] = (b'IDORIS_ALLOCATED_BUDGET_BYTES='
                                                                 + env['IDORIS_ALLOCATED_BUDGET_BYTES'].encode('ascii') + b'\n' == handoff.raw)
        result['health'] = {k: health.get(k) for k in ('service', 'status', 'version', 'contract_version', 'dev_no_key_enabled')}
        checks['formal_health_1_0_1'] = (health.get('status') == 'ok' and health.get('service') == 'idoris' and health.get('contract_version') == '1.0.1'
                                       and health.get('version') == args.expected_version and bool(health.get('instance_id'))
                                       and health.get('dev_no_key_enabled') is False)
        require(checks['formal_health_1_0_1'], 'actual iDoris formal identity mismatch')
        status, _, capabilities = request_http(gateway_port, '/capabilities', headers={'Authorization': 'Bearer ' + key})
        result['recommendation_http_status'] = status
        require(status == 200 and isinstance(capabilities, list) and capabilities, 'live recommendation response unavailable')
        result['recommendations'] = [{'id': row['id'], 'admission_status': row['admission_status'],
                                      'estimated_memory_gb': row['estimated_memory_gb']} for row in capabilities]
        if name == 'zero':
            checks['zero_recommendation_budget_blocks_all'] = all(row['admission_status'] == 'blocked' for row in capabilities)
            require(checks['zero_recommendation_budget_blocks_all'], 'zero budget recommendation admitted a model')
        tap = None if args.daemon_cancellation_only else Tap(gateway_port, key)
        agent_home = work / 'agent-home'
        agent_env = environment(agent_home)
        state = agent_home / '.agent24'
        state.mkdir()
        (state / 'os.json').write_text('{"default":"disabled","domainOs":{}}\n')
        agent_env.update(IDORIS_URL='http://127.0.0.1:%d' % (gateway_port if args.daemon_cancellation_only else tap.port), IDORIS_API_KEY=key,
                         AGENT24D_BIN=str(work / 'never-spawn-another-daemon'))
        daemon = Process([binaries['agent24d'], 'serve', '--port', '0'], agent_env, work)
        def daemon_ready():
            require(daemon.proc.poll() is None, 'actual agent24d exited before readiness')
            try:
                discovery = json.loads((state / 'daemon.json').read_text())
                return discovery.get('pid') == daemon.proc.pid and bool(discovery.get('port'))
            except (OSError, ValueError):
                return False
        wait_for(daemon_ready, 20, 'real Agent24 daemon discovery deadline')
        if args.daemon_cancellation_only:
            result['control'] = 'daemon_origin_shutdown_cancellation'
            checks['consumer_connected_directly_without_tap'] = tap is None
            fixture.hold = True
            fixture.post_seen.clear()
            cli = Process([binaries['agent24'], 'chat', 'owned-cancel-' + uuid.uuid4().hex,
                           '--model', 'idoris/daily'], agent_env, work)
            try:
                require(fixture.post_seen.wait(8), 'real CLI cancellation request did not reach completion POST')
                checks['actual_cli_and_daemon_alive_after_post'] = cli.proc.poll() is None and daemon.proc.poll() is None
                started = time.monotonic()
                os.killpg(daemon.proc.pid, signal.SIGTERM)
                disconnected = fixture.disconnected.wait(5)
                elapsed = time.monotonic() - started
                result['daemon_term_to_upstream_disconnect_seconds'] = round(elapsed, 3)
                checks['daemon_origin_upstream_disconnect_within_5s'] = disconnected and elapsed <= 5
                try:
                    exit_code = cli.proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    raise Failed('actual CLI did not settle after owned daemon TERM') from None
                checks['cancelled_cli_failed_without_answer'] = exit_code != 0 and MARKER not in cli.text()
            finally:
                cli.close()
            daemon.close()
            fixture.hold = False
            require(all(checks.values()), 'daemon-origin cancellation criteria failed')
            auth = {'Authorization': 'Bearer ' + key, 'X-iDoris-Privacy': 'local_only'}
            status, _, response = request_http(gateway_port, '/v1/chat/completions', 'POST', payload('idoris/deep'), auth)
            checks['supervisor_unloaded_cancelled_A_and_loaded_B'] = (status == 200 and response.get('model') == MODEL_B
                                                                     and bool(fixture.posts('/%s/unload' % MODEL_A))
                                                                     and bool(fixture.posts('/%s/load' % MODEL_B)))
            require(checks['supervisor_unloaded_cancelled_A_and_loaded_B'], 'Supervisor failed eviction after daemon cancellation')
            daemon = Process([binaries['agent24d'], 'serve', '--port', '0'], agent_env, work)
            wait_for(daemon_ready, 20, 'new owned Agent24 daemon discovery deadline')
            code, output = command([binaries['agent24'], 'chat', 'owned-next-' + uuid.uuid4().hex,
                                    '--model', 'idoris/daily'], agent_env, work, timeout=25)
            checks['new_daemon_next_cli_succeeded_after_eviction'] = (code == 0 and MARKER in output
                                                                      and 'iDoris record ' in output
                                                                      and len(fixture.posts('/%s/load' % MODEL_A)) >= 2
                                                                      and bool(fixture.posts('/%s/unload' % MODEL_B))
                                                                      and fixture.posts('/chat/completions')[-1][2].get('model') == MODEL_A)
            require(checks['new_daemon_next_cli_succeeded_after_eviction'], 'new daemon CLI did not recover after cancellation/eviction')
            return result
        code, cli_output = consumer(binaries['agent24'], agent_env, work)
        require(daemon.proc.poll() is None, 'owned daemon exited during consumer request')
        wire = [item for item in tap.events if item['path'] == '/v1/chat/completions']
        result['consumer_wire_posts'] = len(wire)
        result['consumer_exit_code'] = code
        checks['real_key_and_localonly_wire'] = bool(wire) and all(item['real_key'] and item['privacy'] == 'local_only' for item in wire)
        checks['real_consumer_health_probe'] = any(item['path'] == '/health' and item.get('status') == 200 for item in tap.events)
        require(checks['real_key_and_localonly_wire'] and checks['real_consumer_health_probe'], 'consumer did not use authenticated formal gateway')
        if name != 'positive':
            checks['consumer_rejected'] = code != 0 and MARKER not in cli_output
            checks['consumer_capacity_denial'] = all(item.get('status') == 503 and item.get('error_reason') == 'eviction_impossible' for item in wire)
            checks['zero_lifecycle_load_posts'] = not fixture.posts('/load')
            checks['zero_completion_posts'] = not fixture.posts('/chat/completions')
            result['consumer_gateway_statuses'] = [item.get('status') for item in wire]
            require(all(checks.values()), 'insufficient allocation reached lifecycle or returned success')
            return result
        completions = fixture.posts('/chat/completions')
        checks['manual_daily_binding_loaded_actual_model'] = bool(fixture.posts('/%s/load' % MODEL_A)) and bool(completions) and completions[-1][2].get('model') == MODEL_A
        checks['consumer_success_content'] = code == 0 and MARKER in cli_output
        checks['record_id_visible_cli'] = bool(wire[-1].get('record_id')) and wire[-1]['record_id'] in cli_output
        checks['consumer_loopback_attestation'] = wire[-1].get('locality') == 'loopback'
        checks['consumer_response_actual_model'] = wire[-1].get('actual_model') == MODEL_A
        result['consumer_evidence'] = {'requested_model': wire[-1]['body'].get('model'),
                                      'actual_model': wire[-1].get('actual_model'),
                                      'record_id_visible_in_cli': checks['record_id_visible_cli'],
                                      'served_locality': wire[-1].get('locality')}
        fields = ('messages', 'tools', 'max_tokens', 'response_format', 'chat_template_kwargs', 'disable_thinking')
        checks['consumer_payload_unchanged_native'] = ({k: completions[-1][2][k] for k in fields if k in completions[-1][2]}
                                                      == {k: wire[-1]['body'][k] for k in fields if k in wire[-1]['body']})
        require(all(checks.values()), 'actual consumer lifecycle/record/payload acceptance failed')
        auth = {'Authorization': 'Bearer ' + key, 'X-iDoris-Privacy': 'local_only'}
        rich = payload('idoris/daily')
        status, _, _ = request_http(gateway_port, '/v1/chat/completions', 'POST', rich, auth)
        got = fixture.posts('/chat/completions')[-1][2]
        result['DIRECT_parameters_http_status'] = status
        checks['DIRECT_native_nondefault_parameters_preserved'] = (status == 200 and got.get('model') == MODEL_A
                                                                  and all(got.get(k) == v for k, v in rich.items() if k != 'model'))
        require(checks['DIRECT_native_nondefault_parameters_preserved'], 'direct native parameter preservation failed')
        # Real POST observed before closing the owned TCP client. This is a
        # client cancellation test, not proof of a 120-second server timeout.
        fixture.hold = True
        fixture.post_seen.clear()
        conn = http.client.HTTPConnection('127.0.0.1', gateway_port, timeout=5)
        try:
            conn.request('POST', '/v1/chat/completions', json.dumps(payload('idoris/daily')), {**auth, 'Content-Type': 'application/json'})
            require(fixture.post_seen.wait(5), 'cancellation did not reach actual fixture POST')
        finally:
            if conn.sock:
                with contextlib.suppress(OSError):
                    conn.sock.shutdown(socket.SHUT_RDWR)
            conn.close()
        checks['DIRECT_post_post_cancellation_observed'] = fixture.disconnected.wait(5)
        require(checks['DIRECT_post_post_cancellation_observed'], 'native upstream did not observe post-POST cancellation')
        if not args.skip_server_timeout:
            fixture.post_seen.clear()
            fixture.disconnected.clear()
            started = time.monotonic()
            # Actual production OmlxAdapter/Supervisor chat deadline is 120s.
            # Client timeout is deliberately longer; no fake timeout config.
            status, _, value = request_http(gateway_port, '/v1/chat/completions', 'POST', payload('idoris/daily'), auth, timeout=135)
            elapsed = time.monotonic() - started
            result['DIRECT_server_timeout_seconds'] = round(elapsed, 2)
            checks['DIRECT_real_server_timeout_after_post'] = (fixture.post_seen.is_set() and status >= 400
                                                              and 90 <= elapsed <= 132 and MARKER not in json.dumps(value))
            checks['DIRECT_timeout_upstream_disconnected'] = fixture.disconnected.wait(5)
            require(checks['DIRECT_real_server_timeout_after_post'] and checks['DIRECT_timeout_upstream_disconnected'],
                    'actual native server deadline did not settle and close upstream')
        fixture.hold = False
        # Admission of B forces eviction of A: each is 70% of the allocated
        # total, so both cannot coexist. Never invoke fixture unload directly.
        status, _, _ = request_http(gateway_port, '/v1/chat/completions', 'POST', payload('idoris/deep'), auth)
        checks['DIRECT_next_model_completed_after_unload'] = status == 200 and bool(fixture.posts('/%s/unload' % MODEL_A)) and bool(fixture.posts('/%s/load' % MODEL_B))
        require(checks['DIRECT_next_model_completed_after_unload'], 'Supervisor did not unload cancelled model and serve next model')
        code, text = consumer(binaries['agent24'], agent_env, work)
        checks['consumer_next_request_after_eviction'] = code == 0 and MARKER in text and bool(fixture.posts('/%s/unload' % MODEL_B))
        checks['consumer_next_request_after_eviction'] &= (len(fixture.posts('/%s/load' % MODEL_A)) >= 2
                                                          and fixture.posts('/chat/completions')[-1][2].get('model') == MODEL_A)
        require(checks['consumer_next_request_after_eviction'], 'actual consumer next request after unload failed')
        return result
    finally:
        errors = []
        for owned in (daemon, gateway, tap, fixture):
            if owned:
                try:
                    owned.close()
                except Exception:
                    errors.append('owned resource cleanup failed')
        result['lifecycle_load_posts'] = len(fixture.posts('/load'))
        result['lifecycle_unload_posts'] = len(fixture.posts('/unload'))
        result['completion_posts'] = len(fixture.posts('/chat/completions'))
        result['owned_process_groups_absent'] = not errors
        result['checks']['virtual_key_not_forwarded_to_backend'] = not fixture.virtual_key_leaked
        require(not fixture.virtual_key_leaked, 'real virtual key leaked to backend fixture')
        require(not errors, 'owned process or listener cleanup failed')


def acceptance(args, report):
    if not all(getattr(args, name + '_bin') for name in ('idoris', 'agent24', 'agent24d')):
        raise Blocked('three final artifacts not supplied; no product execution')
    if args.core_reserved_bytes is None or args.expected_version is None or args.config_dir is None:
        raise Blocked('explicit core reservation, expected version and config directory are required')
    require(0 <= args.core_reserved_bytes < 2 ** 64, 'invalid core reservation')
    with tempfile.TemporaryDirectory(prefix='native-consumer-') as temporary:
        root = Path(temporary).resolve()
        binaries, report['artifacts'] = artifacts(args, root)
        config = Path(args.config_dir).resolve(strict=True)
        report['config_sha256'] = {rel: sha256(config / rel) for rel in ('components/omlx.yaml', 'routing-policy.yaml', 'catalog.yaml')}
        hardware_env = environment(root / 'hardware-home')
        code, raw = command([binaries['agent24'], 'hw', 'probe'], hardware_env, root)
        require(code == 0, 'real hardware probe failed')
        usable = json.loads(raw)['usable_bytes']
        require(isinstance(usable, int) and usable > 0, 'hardware probe reports no usable allocation')
        positive_handoff = budget(binaries['agent24'], hardware_env, root, args.core_reserved_bytes)
        positive = positive_handoff.amount
        require(positive == max(0, usable - args.core_reserved_bytes), 'handoff differs from real hardware minus explicit core reservation')
        if positive < 16 * 1024 * 1024:
            raise Blocked('positive hardware allocation too small for controlled fixture')
        footprint = positive * 7 // 10
        small_reservation = max(0, usable - footprint // 2)
        zero_handoff = budget(binaries['agent24'], hardware_env, root, 2 ** 64 - 1)
        small_handoff = budget(binaries['agent24'], hardware_env, root, small_reservation)
        zero, small = zero_handoff.amount, small_handoff.amount
        require(zero == 0 and 0 < small < footprint, 'real hardware handoffs did not produce zero and insufficient allocations')
        report['hardware_handoff'] = {'usable_bytes': usable, 'positive_bytes': positive, 'zero_bytes': zero, 'insufficient_bytes': small}
        canaries = Canaries()
        try:
            failures = []
            cases = [('positive', positive_handoff, args.core_reserved_bytes)]
            if not args.daemon_cancellation_only:
                cases.extend([('zero', zero_handoff, 2 ** 64 - 1), ('insufficient', small_handoff, small_reservation)])
            for name, handoff, reservation in cases:
                item = {}
                # Keep partial evidence if a product assertion fails.
                report['scenarios'].append(item)
                try:
                    scenario(args, binaries, root, config, name, handoff, footprint, reservation, item)
                except Failed as error:
                    item['failure'] = str(error)
                    failures.append(name)
            report['canary_connections'] = canaries.hits
            require(canaries.hits == 0, 'direct default backend canary was contacted')
            require(not failures, 'scenario criteria failed: ' + ', '.join(failures))
            if not args.daemon_cancellation_only:
                positive_rows = report['scenarios'][0]['recommendations']
                zero_rows = report['scenarios'][1]['recommendations']
                require(any(row['admission_status'] != 'blocked' for row in positive_rows)
                        and positive_rows != zero_rows, 'positive-to-zero recommendation allocation change not observable')
                report['recommendation_allocation_change_observed'] = True
        finally:
            report['canary_connections'] = canaries.hits
            canaries.close()


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    for name in ('idoris', 'agent24', 'agent24d'):
        result.add_argument('--' + name + '-bin', type=Path)
        result.add_argument('--' + name + '-sha256')
    result.add_argument('--idoris-source-sha')
    result.add_argument('--agent24-source-sha')
    result.add_argument('--agent24-source-diff-sha256', help='declared diff provenance for a diagnostic build')
    result.add_argument('--diagnostic-artifacts', action='store_true', help='force PARTIAL: supplied artifacts have not passed final product gates')
    result.add_argument('--config-dir', type=Path)
    result.add_argument('--core-reserved-bytes', type=int)
    result.add_argument('--expected-version')
    result.add_argument('--report-json', type=Path)
    result.add_argument('--self-test', action='store_true')
    result.add_argument('--daemon-cancellation-only', action='store_true',
                        help='run only actual CLI/daemon TERM cancellation and recovery; PARTIAL, no unchanged deadline rerun')
    result.add_argument('--skip-server-timeout', action='store_true',
                        help='omit actual 120s server deadline test; forces PARTIAL/nonzero verdict')
    return result


def main(argv=None):
    args = parser().parse_args(argv)
    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(HarnessTests)
        ok = unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful()
        print('HARNESS_SELFTEST_ONLY: no product acceptance executed')
        return 0 if ok else 1
    report = {'verdict': 'BLOCKED', 'purpose': 'v0.3.1 behavior candidate; release version/provenance must be checked separately',
              'scenarios': [], 'coverage_gaps': [
        'No nondefault Agent24 CLI tools/max_tokens/response_format/disable_thinking interface; rich native checks are DIRECT iDoris only.',
        'Inspected CLI prints content/tokens/Record-Id only; actual model and locality evidence are gateway wire facts, not separate CLI display fields.',
        ('This narrowed run omits unchanged Tap, full allocation matrix and native server deadline controls.' if args.daemon_cancellation_only
         else 'Standard cancellation/deadline checks are DIRECT; daemon-origin proof requires the separate daemon-cancellation-only control.'),
        'Owned loopback lifecycle/OAI fixture is not a real model engine or model-quality proof.',
        'No OS network sandbox: isolated environment/config and default-backend canaries cover configured paths, not all possible outbound traffic.',
        'Supplied SHA256 verifies executable bytes; source SHAs remain operator build provenance declarations.']}
    code = 3
    try:
        if args.diagnostic_artifacts:
            require(args.agent24_source_diff_sha256 and re.fullmatch('[0-9a-f]{64}', args.agent24_source_diff_sha256),
                    'diagnostic artifacts require source diff SHA256 declaration')
            report['diagnostic_source_provenance'] = {'agent24_declared_diff_sha256': args.agent24_source_diff_sha256,
                                                     'clean_source_claimed': False, 'final_product_gates_passed': False}
        acceptance(args, report)
        partial = args.skip_server_timeout or args.diagnostic_artifacts or args.daemon_cancellation_only
        report['verdict'] = 'PARTIAL' if partial else 'PASS'
        report['reason'] = ('only daemon-origin cancellation control executed' if args.daemon_cancellation_only
                            else 'diagnostic artifacts have not passed final source/product gates' if args.diagnostic_artifacts
                            else 'actual native deadline check omitted' if args.skip_server_timeout
                            else 'listed real consumer, native, lifecycle and allocated-budget criteria completed')
        code = 4 if partial else 0
    except Blocked as error:
        report['reason'] = str(error)
    except Failed as error:
        report.update(verdict='FAIL', reason=str(error))
        code = 1
    except (OSError, ValueError, KeyError, TypeError, http.client.HTTPException):
        report.update(verdict='BLOCKED', reason='artifact/config/process or protocol input unavailable; inspect locally without exporting raw logs')
    text = json.dumps(report, indent=2) + '\n'
    if args.report_json:
        args.report_json.write_text(text)
    else:
        print(text, end='')
    return code


class HarnessTests(unittest.TestCase):
    def sender(self, root, stdout, stderr=b''):
        sender = root / 'fixture-sender'
        sender.write_text('#!/usr/bin/python3\nimport sys\nsys.stdout.buffer.write(%r)\nsys.stderr.buffer.write(%r)\n' % (stdout, stderr))
        sender.chmod(0o700)
        return str(sender)

    def test_budget_rejects_malformed_sender_stdout(self):
        name = b'IDORIS_ALLOCATED_BUDGET_BYTES='
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            env = environment(root / 'home')
            for stdout in (name + b'123 \n', name + b'123\n\n', name + b'123\nextra\n',
                           name + b'123\r\n', name + b'123\t\n', b' ' + name + b'123\n',
                           name + b'123', name + b' 123\n'):
                with self.subTest(stdout=stdout), self.assertRaises(Failed):
                    budget(self.sender(root, stdout), env, root, 0)

    def test_budget_uses_stdout_not_stderr(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            with self.assertRaises(Failed):
                budget(self.sender(root, b'', b'IDORIS_ALLOCATED_BUDGET_BYTES=123\n'),
                       environment(root / 'home'), root, 0)

    def test_budget_retains_sender_value_in_independent_process_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            env = environment(root / 'home')
            raw = b'IDORIS_ALLOCATED_BUDGET_BYTES=000123\n'
            handoff = budget(self.sender(root, raw, b'ignored sender diagnostic\n'), env, root, 0)
            self.assertEqual(handoff.amount, 123)
            self.assertEqual(handoff.raw, raw)
            handoff.apply_to(env)
            code, observed = command([sys.executable, '-c',
                                      'import os,sys; sys.stdout.buffer.write(("IDORIS_ALLOCATED_BUDGET_BYTES="+os.environ["IDORIS_ALLOCATED_BUDGET_BYTES"]+"\\n").encode("ascii"))'],
                                     env, root, stdout_only=True)
            self.assertEqual(code, 0)
            self.assertEqual(observed, raw)

    def test_no_artifacts_blocks(self):
        report = {}
        with self.assertRaises(Blocked):
            acceptance(parser().parse_args([]), report)

    def test_no_artifacts_cli_is_nonzero_blocked(self):
        for argv in ([], ['--daemon-cancellation-only']):
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                code = main(argv)
            report = json.loads(output.getvalue())
            self.assertEqual(code, 3)
            self.assertEqual(report['verdict'], 'BLOCKED')
            self.assertEqual(report['scenarios'], [])

    def test_hash_compatible_with_system_python(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'bytes'
            path.write_bytes(b'abc')
            self.assertEqual(sha256(path), hashlib.sha256(b'abc').hexdigest())

    def test_busy_canary_is_blocked(self):
        with socket.socket() as held:
            held.bind(('127.0.0.1', 0))
            held.listen(1)
            with self.assertRaises(Blocked):
                Canaries(ports=(held.getsockname()[1],))

    def test_child_environment_does_not_inherit_provider_credentials(self):
        with tempfile.TemporaryDirectory() as temp:
            env = environment(Path(temp).resolve())
            self.assertEqual(set(env), {'HOME', 'PATH', 'TMPDIR', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME',
                                        'XDG_CACHE_HOME', 'NO_PROXY', 'no_proxy', 'LANG'})
            self.assertNotIn('IDORIS_API_KEY', env)

    def test_lifecycle_fixture_requires_load_and_confirms_unload(self):
        fixture = Fixture(12345)
        try:
            self.assertEqual(request_http(fixture.port, '/v1/chat/completions', 'POST', payload())[0], 409)
            self.assertEqual(request_http(fixture.port, '/v1/models/%s/load' % MODEL_A, 'POST', {})[0], 200)
            self.assertEqual(request_http(fixture.port, '/v1/chat/completions', 'POST', payload())[0], 200)
            self.assertEqual(request_http(fixture.port, '/v1/models/%s/unload' % MODEL_A, 'POST', {})[0], 200)
            self.assertFalse(request_http(fixture.port, '/v1/models/status')[2]['models'][0]['loaded'])
        finally:
            fixture.close()

    def test_held_post_fixture_observes_actual_peer_disconnect(self):
        fixture = Fixture(12345)
        conn = http.client.HTTPConnection('127.0.0.1', fixture.port, timeout=2)
        try:
            request_http(fixture.port, '/v1/models/%s/load' % MODEL_A, 'POST', {})
            fixture.hold = True
            self.assertFalse(fixture.disconnected.is_set())
            conn.request('POST', '/v1/chat/completions', json.dumps(payload()), {'Content-Type': 'application/json'})
            self.assertTrue(fixture.post_seen.wait(2))
            self.assertFalse(fixture.disconnected.is_set())
            conn.sock.shutdown(socket.SHUT_RDWR)
            conn.close()
            self.assertTrue(fixture.disconnected.wait(2))
        finally:
            conn.close()
            fixture.close()

    def test_owned_group_timeout_cleanup_including_child(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp).resolve()
            child = Process([sys.executable, '-c', 'import subprocess,time; subprocess.Popen(["/bin/sleep","30"]); time.sleep(30)'], environment(root / 'home'), root)
            time.sleep(.15)
            child.close()
            self.assertIsNotNone(child.proc.returncode)
            self.assertFalse(child.group_alive())


if __name__ == '__main__':
    sys.exit(main())
