#!/usr/bin/env python3
"""Agent24 -> iDoris -> loopback fixture wire acceptance (v0.3.1 candidate).

Starts the real iDoris binary (isolated DB/state, loopback fixture as its only
backend) and drives a REAL Agent24 executable that you supply. A recording
loopback tap sits between the two processes, so the evidence is HTTP captured
on the wire: Agent24 -> tap -> iDoris -> fixture.

Nothing here downloads models, calls paid APIs, or contacts non-loopback hosts.
Not an Agent24 implementation: when Agent24 has no IDORIS_URL provider the tool
reports RED (exit 10), which is a product finding, never a PASS.

Exit codes: 0 PASS | 10 RED (Agent24 not implemented / non-conformant)
            1 unexpected failure | 2 tool/usage/environment error
            3 BLOCKED (no Agent24 supplied; nothing to accept)

Interface assumptions come from docs/interfaces/iDoris-Agent24-边界与接口规范.md
(IDORIS_URL / IDORIS_API_KEY slots, Bearer key, X-iDoris-Privacy, model
idoris/<role>, X-iDoris-Served-Locality, X-iDoris-Record-Id). Agent24's own CLI
is NOT assumed: you pass the exact argv via --agent24-call-cmd.

Example (replace the Agent24 argv with whatever its real CLI takes):
  python3 scripts/test_agent24_idoris_v031.py --idoris-bin target/release/idoris \\
    --agent24-source ../Agent24 --agent24-bin ../Agent24/<exe> \\
    --agent24-call-cmd '["<exe>", "<subcommand>", "--model", "{model}", "{prompt}"]'
Self-proof of this tool (stub consumers, NOT Agent24):
  python3 scripts/test_agent24_idoris_v031.py --self-test --idoris-bin target/release/idoris
"""
import argparse
import contextlib
import hashlib
import http.client
import http.server
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.error
import urllib.request
import uuid
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
MARKER = 'fixture-ok'
ROLE_MODELS = {'fast': 'fixture-fast-model', 'daily': 'fixture-daily-model',
               'deep': 'fixture-deep-model', 'auto': 'fixture-daily-model'}
CONCRETE = 'fixture-concrete-model'
ALL_ROLES = ['fast', 'daily', 'deep', 'vision', 'embed', 'rerank', 'decide', 'auto']
CANARY_PORTS = {'omlx-default-8000': 8000, 'ollama-default-11434': 11434}
EXIT_PASS, EXIT_FAIL, EXIT_TOOL, EXIT_BLOCKED, EXIT_RED = 0, 1, 2, 3, 10

REQUIRED_INTERFACE = [
    'ModelRouter/provider registry accepts an optional IDORIS_URL (+ IDORIS_API_KEY) slot; unset => behaviour unchanged.',
    'Two logical providers: idoris-local (fixed X-iDoris-Privacy: local_only) and idoris-any (fixed any).',
    'Request: POST <IDORIS_URL>/v1/chat/completions, Authorization: Bearer <virtual key>, model=idoris/<role> (or a concrete id), X-iDoris-Privacy, optional Complexity/Intent/Request-Id.',
    'Attach-time GET /health identity check; every response must carry X-iDoris-Served-Locality; idoris-local must reject anything but loopback (missing/lan/remote) and return no content to the module.',
    'X-iDoris-Record-Id from the response recorded in Agent24 run/event log (observable in CLI output or log).',
    'Missing key / 401 / 403 / 5xx / connection failure => request fails; never fall back to direct oMLX/Ollama/cloud.',
]


# --------------------------------------------------------------------------- utils
def free_port():
    with socket.socket() as s:
        s.bind(('127.0.0.1', 0))
        return s.getsockname()[1]


def http_get(url, timeout=3):
    with OPENER.open(url, timeout=timeout) as r:
        return r.status, r.read()


def sha(s):
    return hashlib.sha256(s.encode()).hexdigest()


class Scrubber:
    """Removes secrets and local absolute paths from anything that enters the report."""

    def __init__(self):
        self.secrets, self.paths = [], []

    def add_secret(self, s):
        if s:
            self.secrets.append(s)

    def add_path(self, p, label):
        self.paths.append((str(p), label))

    def __call__(self, text):
        if not isinstance(text, str):
            return text
        for s in self.secrets:
            text = text.replace(s, '<redacted>')
        for p, label in sorted(self.paths, key=lambda x: -len(x[0])):
            text = text.replace(p, label)
        text = text.replace(str(Path.home()), '<home>').replace(str(REPO), '<repo>')
        return re.sub(r'(idk_[A-Za-z0-9_\-]{8,})', '<redacted>', text)


# --------------------------------------------------------------------------- fixture backend
class Fixture:
    """OpenAI-shaped loopback backend standing in for oMLX/Ollama (resident http_service)."""

    def __init__(self):
        self.seen = []
        fx = self

        class H(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def _reply(self, body):
                raw = json.dumps(body).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_GET(self):
                data = [{'id': m, 'object': 'model'} for m in sorted(set(ROLE_MODELS.values()) | {CONCRETE})]
                self._reply({'object': 'list', 'data': data})

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get('Content-Length', 0))) or b'{}')
                auth = self.headers.get('Authorization', '')
                fx.seen.append({'path': self.path, 'model': body.get('model'),
                                'authorization_forwarded': bool(auth),
                                'virtual_key_forwarded': 'idk_' in auth})
                self._reply({'id': 'fixture-chat', 'object': 'chat.completion', 'model': body.get('model'),
                             'choices': [{'index': 0, 'finish_reason': 'stop',
                                          'message': {'role': 'assistant', 'content': MARKER}}],
                             'usage': {'prompt_tokens': 1, 'completion_tokens': 1, 'total_tokens': 2}})

        self.srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), H)
        self.port = self.srv.server_port
        self.thread = threading.Thread(target=self.srv.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.srv.shutdown()
        self.srv.server_close()
        self.thread.join(5)


# --------------------------------------------------------------------------- canaries
class Canaries:
    """Listeners on well-known direct-backend loopback ports; any connection is a bypass."""

    def __init__(self):
        self.hits = {k: [] for k in CANARY_PORTS}
        self.unavailable = {}
        self.socks, self.threads = {}, []
        self.stop = False
        for name, port in CANARY_PORTS.items():
            s = socket.socket()
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            try:
                s.bind(('127.0.0.1', port))
                s.listen(8)
                s.settimeout(0.2)
            except OSError as e:
                s.close()
                self.unavailable[name] = str(e)
                continue
            self.socks[name] = s
            t = threading.Thread(target=self._serve, args=(name, s), daemon=True)
            t.start()
            self.threads.append(t)

    def _serve(self, name, s):
        while not self.stop:
            try:
                c, _ = s.accept()
            except (socket.timeout, OSError):
                continue
            try:
                c.settimeout(0.5)
                first = c.recv(256).split(b'\r\n')[0].decode('latin1')
            except OSError:
                first = ''
            self.hits[name].append(first[:80])
            c.close()

    def total(self):
        return sum(len(v) for v in self.hits.values())

    def close(self):
        self.stop = True
        for t in self.threads:
            t.join(3)
        for s in self.socks.values():
            s.close()


# --------------------------------------------------------------------------- recording tap
class Tap:
    """Loopback reverse proxy Agent24 talks to (its IDORIS_URL). Records, optionally tampers."""

    def __init__(self, upstream_port, expected_key_sha=None):
        self.upstream_port = upstream_port
        self.expected_key_sha = expected_key_sha
        self.mode = 'none'  # none | locality_remote | locality_lan | locality_missing
        self.log = []
        tap = self

        class H(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *_):
                pass

            def _fwd(self):
                n = int(self.headers.get('Content-Length', 0) or 0)
                raw = self.rfile.read(n) if n else b''
                try:
                    body = json.loads(raw) if raw else {}
                except ValueError:
                    body = {}
                auth = self.headers.get('Authorization', '')
                entry = {'method': self.command, 'path': self.path,
                         'request_headers': sorted(k.lower() for k in self.headers.keys()),
                         'authorization_present': bool(auth),
                         'authorization_bearer_matches_key': (
                             auth.startswith('Bearer ') and tap.expected_key_sha is not None
                             and sha(auth[7:]) == tap.expected_key_sha),
                         'privacy': self.headers.get('X-iDoris-Privacy'),
                         'complexity': self.headers.get('X-iDoris-Complexity'),
                         'request_id': self.headers.get('X-iDoris-Request-Id'),
                         'model': body.get('model') if isinstance(body, dict) else None,
                         'stream': body.get('stream') if isinstance(body, dict) else None}
                tap.log.append(entry)
                hdrs = {k: v for k, v in self.headers.items()
                        if k.lower() not in ('host', 'connection', 'keep-alive', 'transfer-encoding')}
                try:
                    c = http.client.HTTPConnection('127.0.0.1', tap.upstream_port, timeout=15)
                    c.request(self.command, self.path, body=raw or None, headers=hdrs)
                    r = c.getresponse()
                    data = r.read()
                    rheaders = [(k, v) for k, v in r.getheaders()
                                if k.lower() not in ('transfer-encoding', 'connection', 'content-length')]
                    status = r.status
                    c.close()
                except OSError as e:
                    entry['upstream_error'] = type(e).__name__
                    self.close_connection = True
                    with contextlib.suppress(OSError):
                        self.connection.shutdown(socket.SHUT_RDWR)
                    return
                loc = next((v for k, v in rheaders if k.lower() == 'x-idoris-served-locality'), None)
                if tap.mode != 'none':
                    rheaders = [(k, v) for k, v in rheaders if k.lower() != 'x-idoris-served-locality']
                    forced = {'locality_remote': 'remote', 'locality_lan': 'lan'}.get(tap.mode)
                    if forced:
                        rheaders.append(('X-iDoris-Served-Locality', forced))
                    entry['tampered'] = tap.mode
                entry.update(status=status, served_locality_from_idoris=loc,
                             record_id=next((v for k, v in rheaders if k.lower() == 'x-idoris-record-id'), None))
                self.send_response(status)
                for k, v in rheaders:
                    self.send_header(k, v)
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = do_PUT = do_DELETE = _fwd

        self.srv = http.server.ThreadingHTTPServer(('127.0.0.1', 0), H)
        self.port = self.srv.server_port
        self.thread = threading.Thread(target=self.srv.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.srv.shutdown()
        self.srv.server_close()
        self.thread.join(5)


# --------------------------------------------------------------------------- iDoris process
class Gateway:
    def __init__(self, binary, config_dir, fixture_port, root):
        self.binary, self.root = Path(binary).resolve(strict=True), Path(root)
        self.port, self.admin_port = free_port(), free_port()
        self.proc, self.log_path = None, self.root / 'idoris.log'
        comps = self.root / 'components'
        comps.mkdir()
        card = (Path(config_dir) / 'components' / 'omlx.yaml').read_text()
        card = (card.replace('http://127.0.0.1:8000', f'http://127.0.0.1:{fixture_port}')
                .replace('id: omlx', 'id: fixture-local').replace('omlx@0.6.4', 'fixture@1')
                .replace('mode: on_demand', 'mode: resident')
                .replace('admission: requires_eviction', 'admission: coexist')
                .replace('idle_ttl_s: 300', 'pinned: true'))
        card += '\nextensions:\n  _degradation: "fixture binding removed => role aliases unavailable"\n  idoris_role_models:\n'
        card += ''.join(f'    {r}: {m}\n' for r, m in ROLE_MODELS.items())
        (comps / 'local.yaml').write_text(card)
        self.env = {k: v for k, v in os.environ.items() if not k.startswith('IDORIS_')}
        for k in ('HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY', 'http_proxy', 'https_proxy', 'all_proxy'):
            self.env.pop(k, None)
        self.env.update(NO_PROXY='127.0.0.1,localhost', no_proxy='127.0.0.1,localhost',
                        IDORIS_DEPLOY_MODE='personal',
                        IDORIS_DB_PATH=str(self.root / 'state.sqlite3'),
                        IDORIS_STATE_DIR=str(self.root / 'state'),
                        IDORIS_PORT=str(self.port), IDORIS_ADMIN_PORT=str(self.admin_port),
                        IDORIS_COMPONENTS_DIR=str(comps),
                        IDORIS_ROUTING_POLICY=str(Path(config_dir) / 'routing-policy.yaml'),
                        IDORIS_CATALOG=str(Path(config_dir) / 'catalog.yaml'))

    def issue_key(self, owner, roles, privacy=('local_only',)):
        spec = {'owner': owner, 'allowed_privacy': list(privacy), 'allowed_roles': roles}
        r = subprocess.run([str(self.binary), 'key', 'issue', '--spec-stdin'], env=self.env, input=json.dumps(spec),
                           text=True, capture_output=True, timeout=30, cwd=self.root)
        if r.returncode != 0:
            raise ToolError('key issue failed rc=%d' % r.returncode)
        return json.loads(r.stdout)['secret']

    def start(self):
        log = self.log_path.open('w+')
        self.proc = subprocess.Popen([str(self.binary), 'serve', '--admin-token-stdin'], cwd=self.root, env=self.env,
                                     stdin=subprocess.PIPE, stdout=log, stderr=log, text=True)
        self.proc.stdin.write(uuid.uuid4().hex + uuid.uuid4().hex + '\n')
        self.proc.stdin.close()
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise ToolError('idoris exited early: ' + self.log_path.read_text()[-400:])
            with contextlib.suppress(OSError, urllib.error.URLError):
                if http_get(f'http://127.0.0.1:{self.port}/health')[0] == 200:
                    return
            time.sleep(0.1)
        raise ToolError('idoris readiness timeout')

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()

    def alive(self):
        return self.proc is not None and self.proc.poll() is None


class ToolError(Exception):
    pass


# --------------------------------------------------------------------------- run the consumer
def run_consumer(argv, env, cwd, timeout):
    t0 = time.monotonic()
    p = subprocess.Popen(argv, env=env, cwd=cwd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                         stderr=subprocess.PIPE, text=True, errors='replace', start_new_session=True)
    try:
        out, err = p.communicate(timeout=timeout)
        timed_out = False
    except subprocess.TimeoutExpired:
        timed_out = True
        with contextlib.suppress(ProcessLookupError):
            os.killpg(p.pid, signal.SIGKILL)
        out, err = p.communicate()
    with contextlib.suppress(ProcessLookupError):  # reap stragglers in the group
        os.killpg(p.pid, signal.SIGKILL)
    return {'exit_code': None if timed_out else p.returncode, 'timed_out': timed_out,
            'seconds': round(time.monotonic() - t0, 2), 'stdout': out, 'stderr': err}


def child_env(home, tap_url, key, args):
    env = {'PATH': os.environ.get('PATH', '/usr/bin:/bin'), 'HOME': str(home), 'XDG_CONFIG_HOME': str(home / 'cfg'),
           'XDG_DATA_HOME': str(home / 'data'), 'XDG_CACHE_HOME': str(home / 'cache'), 'LANG': 'C.UTF-8',
           'NO_PROXY': '127.0.0.1,localhost', 'no_proxy': '127.0.0.1,localhost'}
    env[args.url_env] = tap_url
    if key is not None:
        env[args.key_env] = key
    for kv in args.agent24_env:
        k, _, v = kv.partition('=')
        env[k] = v
    return env


def render(template, model, prompt):
    role = model.split('/', 1)[1] if model.startswith('idoris/') else ''
    return [a.replace('{model}', model).replace('{role}', role).replace('{prompt}', prompt) for a in template]


# --------------------------------------------------------------------------- scenarios
def acceptance(args, scrub):
    """Returns the report dict. Raises ToolError for environment/tool problems."""
    template = json.loads(args.agent24_call_cmd) if args.agent24_call_cmd else None
    if template is not None and (not isinstance(template, list) or not all(isinstance(x, str) for x in template)):
        raise ToolError('--agent24-call-cmd must be a JSON array of strings')
    report = {'schema': 'agent24-idoris-wire-acceptance/1', 'platform': sys.platform, 'static': None,
              'scenarios': [], 'coverage_gaps': [], 'required_interface_for_green': REQUIRED_INTERFACE}
    if args.agent24_source:
        report['static'] = static_scan(Path(args.agent24_source))
    report['coverage_gaps'] += [
        'Linux-only run; macOS/Windows not exercised.',
        'Cloud-direct bypass is not observable: only loopback canaries (:8000 oMLX, :11434 Ollama) are counted.',
        'Fixture stands in for oMLX/Ollama; real model quality/lifecycle not tested.',
        'Tap buffers responses: SSE streaming path is not covered.',
        'Agent24 behaviour is judged from process exit, captured output and wire traffic only; its internal tripwire code is not inspected by this tool.']
    if not (args.agent24_bin and template):
        report['verdict'] = 'BLOCKED'
        report['reason'] = ('no Agent24 executable/call command supplied; static scan only'
                            if report['static'] else 'no Agent24 supplied')
        if report['static'] and not report['static']['idoris_url_provider_found']:
            report['verdict'] = 'RED'
            report['reason'] = 'not-implemented (static only): no IDORIS_URL provider in supplied Agent24 source'
            report['coverage_gaps'].append('No dynamic two-process run: --agent24-bin/--agent24-call-cmd not supplied.')
        return report
    agent_bin = shutil.which(args.agent24_bin) or str(Path(args.agent24_bin).resolve())
    if not os.access(agent_bin, os.X_OK):
        raise ToolError('--agent24-bin is not executable')

    fixture, canaries, gw, tap = Fixture(), Canaries(), None, None
    for name, why in canaries.unavailable.items():
        report['coverage_gaps'].append(f'canary {name} not bound ({why}); bypass to that port unobservable.')
    with tempfile.TemporaryDirectory(prefix='a24-idoris-') as tmp:
        root = Path(tmp)
        scrub.add_path(root, '<tmp>')
        home = root / 'agent24-home'
        home.mkdir()
        cwd = Path(args.agent24_cwd).resolve() if args.agent24_cwd else root
        try:
            config_dir = Path(args.idoris_config_dir or REPO / 'config')
            gw = Gateway(args.idoris_bin, config_dir, fixture.port, root)
            full = gw.issue_key('a24-acceptance-full', ALL_ROLES)
            daily_only = gw.issue_key('a24-acceptance-daily', ['daily'])
            for k in (full, daily_only):
                scrub.add_secret(k)
            gw.start()
            tap = Tap(gw.port, sha(full))
            tap_url = f'http://127.0.0.1:{tap.port}'
            health = json.loads(http_get(f'http://127.0.0.1:{gw.port}/health')[1])
            report['idoris'] = {'health_status': health.get('status'), 'health_version': health.get('version'),
                                'binary_sha256': hashlib.sha256(Path(gw.binary).read_bytes()).hexdigest()}
            ctx = dict(args=args, tap=tap, fixture=fixture, canaries=canaries, gw=gw, root=root, home=home,
                       cwd=cwd, agent_bin=agent_bin, template=template, tap_url=tap_url, full=full,
                       daily_only=daily_only)
            plan = [
                ('happy_role_daily', 'full', 'idoris/daily', 'none', 'ok'),
                ('happy_concrete_model', 'full', CONCRETE, 'none', 'ok'),
                ('no_key', None, 'idoris/daily', 'none', 'reject'),
                ('wrong_key', 'idk_' + 'x' * 40, 'idoris/daily', 'none', 'reject'),
                ('role_not_permitted', 'daily_only', 'idoris/deep', 'none', 'reject'),
                ('locality_remote', 'full', 'idoris/daily', 'locality_remote', 'reject'),
                ('locality_lan', 'full', 'idoris/daily', 'locality_lan', 'reject'),
                ('locality_missing', 'full', 'idoris/daily', 'locality_missing', 'reject'),
                ('gateway_down', 'full', 'idoris/daily', 'kill', 'reject'),
            ]
            for name, keyref, model, tapmode, expect in plan:
                report['scenarios'].append(scenario(ctx, name, keyref, model, tapmode, expect))
            report['process_cleanup'] = {'idoris_alive_after_run': gw.alive()}
        finally:
            if tap:
                tap.close()
            if gw:
                gw.stop()
            canaries.close()
            fixture.close()
    # verdict
    sc = report['scenarios']
    happy = sc[0]
    if happy['tap_requests'] == 0:
        report['verdict'] = 'RED'
        report['reason'] = ('not-implemented: Agent24 made no request to IDORIS_URL in the happy path '
                            f"(exit={happy['agent24']['exit_code']}; see scenarios[0].agent24 tails - a launch error "
                            'such as a relative path also lands here)'
                            + ('; it contacted direct-backend canaries instead (BYPASS)'
                               if happy['canary_hits'] else ''))
    elif all(s['pass'] for s in sc):
        report['verdict'] = 'PASS'
        report['reason'] = 'all scenarios satisfied'
    else:
        report['verdict'] = 'RED'
        report['reason'] = 'non-conformant: ' + ', '.join(s['name'] for s in sc if not s['pass'])
    if report['static'] and not report['static']['idoris_url_provider_found'] and report['verdict'] == 'PASS':
        report['coverage_gaps'].append('Static scan found no IDORIS_URL literal although dynamic run passed '
                                       '(provider may use another config name; review).')
    return report


def scenario(ctx, name, keyref, model, tapmode, expect):
    args, tap, fixture, canaries, gw = ctx['args'], ctx['tap'], ctx['fixture'], ctx['canaries'], ctx['gw']
    key = {None: None, 'full': ctx['full'], 'daily_only': ctx['daily_only']}.get(keyref, keyref)
    if tapmode == 'kill':
        gw.stop()
        tap.mode = 'none'
    else:
        tap.mode = tapmode
    t0, f0, c0 = len(tap.log), len(fixture.seen), canaries.total()
    prompt = 'wire-acceptance-' + uuid.uuid4().hex[:8]
    argv = render(ctx['template'], model, prompt)
    argv[0] = ctx['agent_bin'] if argv[0] == args.agent24_bin else argv[0]
    if not (os.path.isabs(argv[0]) or shutil.which(argv[0])):
        raise ToolError('call command argv[0] must be absolute or on PATH (Agent24 runs in an isolated cwd)')
    res = run_consumer(argv, child_env(ctx['home'], ctx['tap_url'], key, args), ctx['cwd'], args.call_timeout)
    time.sleep(0.2)
    reqs = [e for e in tap.log[t0:] if e['path'].startswith('/v1/chat/completions')]
    fx = fixture.seen[f0:]
    hits = canaries.total() - c0
    text = res['stdout'] + res['stderr']
    checks = {}
    if expect == 'ok':
        r = reqs[-1] if reqs else {}
        want = ROLE_MODELS.get(model.split('/', 1)[1]) if model.startswith('idoris/') else model
        checks = {
            'exit_zero': res['exit_code'] == 0,
            'request_reached_idoris': bool(reqs),
            'virtual_key_bearer': bool(r.get('authorization_bearer_matches_key')),
            'privacy_header_' + args.expect_privacy: r.get('privacy') == args.expect_privacy,
            'model_is_requested': r.get('model') == model,
            'idoris_status_200': r.get('status') == 200,
            'served_locality_loopback': r.get('served_locality_from_idoris') == 'loopback',
            'record_id_issued': bool(r.get('record_id')),
            'fixture_got_resolved_model': bool(fx) and all(x['model'] == want for x in fx),
            'virtual_key_not_forwarded_upstream': bool(fx) and not any(x['virtual_key_forwarded'] for x in fx),
            'content_returned_to_module': MARKER in text,
            'no_direct_backend_canary_hits': hits == 0}
        if args.record_id_in_output:
            checks['record_id_visible_in_agent24_output'] = bool(r.get('record_id')) and r['record_id'] in text
    else:
        checks = {
            'consumer_failed_closed': res['exit_code'] not in (0, None) and not res['timed_out'],
            'no_content_returned_to_module': MARKER not in text,
            'no_direct_backend_canary_hits': hits == 0}
        if name in ('no_key', 'wrong_key', 'role_not_permitted'):
            checks['no_fixture_inference'] = len(fx) == 0
            checks['idoris_denied_or_not_sent'] = (not reqs) or all(x.get('status') in (401, 403) for x in reqs)
        if name == 'gateway_down':
            checks['no_fixture_inference'] = len(fx) == 0
            checks['no_successful_response_via_tap'] = not any(x.get('status') == 200 for x in reqs)
        if name.startswith('locality_'):
            checks['tap_tampered_response'] = bool(reqs) and all(x.get('tampered') for x in reqs)
    return {'name': name, 'expect': expect, 'model': model, 'tap_mode': tapmode, 'pass': all(checks.values()),
            'checks': checks, 'tap_requests': len(reqs), 'fixture_requests': len(fx), 'canary_hits': hits,
            'wire': [{k: v for k, v in e.items() if k not in ('request_headers',)} for e in reqs][:3],
            'fixture_seen': fx[:3], 'agent24': {'exit_code': res['exit_code'], 'timed_out': res['timed_out'],
                                                'seconds': res['seconds'], 'stdout_tail': res['stdout'][-400:],
                                                'stderr_tail': res['stderr'][-400:]}}


def static_scan(src):
    out = {'path_label': src.name, 'git_head': None, 'git_dirty': None, 'idoris_url_provider_found': False,
           'matches': []}
    if not src.is_dir():
        raise ToolError('--agent24-source is not a directory')
    for key, cmd in (('git_head', ['rev-parse', 'HEAD']), ('git_dirty', ['status', '--porcelain'])):
        r = subprocess.run(['git', '-C', str(src)] + cmd, capture_output=True, text=True)
        if r.returncode == 0:
            out[key] = r.stdout.strip() if key == 'git_head' else bool(r.stdout.strip())
    skip = {'.git', 'node_modules', 'target', 'dist', 'build', '.venv', '__pycache__'}
    for dp, dns, fns in os.walk(src):
        dns[:] = [d for d in dns if d not in skip]
        for fn in fns:
            p = Path(dp) / fn
            try:
                if p.stat().st_size > 2_000_000:
                    continue
                if 'IDORIS_URL' in p.read_text(errors='ignore'):
                    out['matches'].append(str(p.relative_to(src)))
            except OSError:
                pass
    out['matches'] = sorted(out['matches'])[:10]
    out['idoris_url_provider_found'] = bool(out['matches'])
    return out


# --------------------------------------------------------------------------- stub consumers (self-test only)
def stub_consumer(mode, argv):
    """NOT Agent24. Exercises the tool: conformant | ignore_locality | bypass | noop."""
    p = argparse.ArgumentParser()
    p.add_argument('--model', required=True)
    p.add_argument('--prompt', required=True)
    a = p.parse_args(argv)
    url, key = os.environ.get('IDORIS_URL'), os.environ.get('IDORIS_API_KEY')
    if mode == 'noop':
        print('no provider configured')
        return 4
    if mode == 'bypass':
        try:
            c = http.client.HTTPConnection('127.0.0.1', CANARY_PORTS['ollama-default-11434'], timeout=2)
            c.request('POST', '/v1/chat/completions', body='{}')
            c.getresponse()
        except OSError:
            pass
        print('direct backend attempted')
        return 5
    if not url or not key:
        print('refused: IDORIS_URL/IDORIS_API_KEY missing')
        return 3
    body = json.dumps({'model': a.model, 'messages': [{'role': 'user', 'content': a.prompt}], 'stream': False}).encode()
    req = urllib.request.Request(url + '/v1/chat/completions', data=body, headers={
        'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json', 'X-iDoris-Privacy': 'local_only',
        'X-iDoris-Complexity': 'simple', 'X-iDoris-Request-Id': 'stub-' + uuid.uuid4().hex})
    try:
        with OPENER.open(req, timeout=10) as r:
            loc, rid, data = r.headers.get('X-iDoris-Served-Locality'), r.headers.get('X-iDoris-Record-Id'), r.read()
    except (urllib.error.URLError, OSError, http.client.HTTPException) as e:
        print('request failed:', type(e).__name__)
        return 3
    if mode == 'conformant' and loc != 'loopback':
        print('locality tripwire: refusing, got', loc)
        return 3
    print('record_id=' + str(rid))
    print(json.loads(data)['choices'][0]['message']['content'])
    return 0


# --------------------------------------------------------------------------- self-test
STUB_ARGS = ['--model', '{model}', '--prompt', '{prompt}']


def stub_cmd(mode):
    return json.dumps([sys.executable, '-I', str(Path(__file__).resolve()), '--stub-consumer', mode] + STUB_ARGS)


def mk_args(**kw):
    ns = argparse.Namespace(idoris_bin=None, idoris_config_dir=None, agent24_bin=sys.executable,
                            agent24_source=None, agent24_call_cmd=None, agent24_cwd=None, agent24_env=[],
                            url_env='IDORIS_URL', key_env='IDORIS_API_KEY', call_timeout=20,
                            expect_privacy='local_only', record_id_in_output=True, report_json=None, self_test=False,
                            stub_consumer=None)
    ns.__dict__.update(kw)
    return ns


class SelfTest(unittest.TestCase):
    """Proves the tool itself (plumbing, negative detection, timeouts, cleanup) with stubs, not Agent24."""

    idoris_bin = None

    def run_mode(self, mode):
        a = mk_args(idoris_bin=self.idoris_bin, agent24_call_cmd=stub_cmd(mode))
        return acceptance(a, Scrubber())

    def need_bin(self):
        if not self.idoris_bin:
            self.skipTest('--idoris-bin not supplied')

    def test_template_validation(self):
        with self.assertRaises(ToolError):
            acceptance(mk_args(agent24_call_cmd='{"not":"a list"}'), Scrubber())

    def test_blocked_without_agent24(self):
        r = acceptance(mk_args(agent24_bin=None), Scrubber())
        self.assertEqual(r['verdict'], 'BLOCKED')

    def test_static_scan_detects_absence_and_presence(self):
        with tempfile.TemporaryDirectory() as d:
            (Path(d) / 'a.ts').write_text('const x = 1')
            r = acceptance(mk_args(agent24_bin=None, agent24_source=d), Scrubber())
            self.assertEqual(r['verdict'], 'RED')
            self.assertFalse(r['static']['idoris_url_provider_found'])
            (Path(d) / 'b.ts').write_text('process.env.IDORIS_URL')
            self.assertTrue(static_scan(Path(d))['idoris_url_provider_found'])

    def test_timeout_kills_process_group(self):
        marker = tempfile.mktemp()
        script = f'import subprocess,time;subprocess.Popen(["sleep","30"]);open({marker!r},"w").write("x");time.sleep(30)'
        t0 = time.monotonic()
        res = run_consumer([sys.executable, '-I', '-c', script], {'PATH': os.environ['PATH']}, '/', 1.5)
        self.assertTrue(res['timed_out'])
        self.assertLess(time.monotonic() - t0, 6)
        os.unlink(marker)

    def test_scrubber(self):
        s = Scrubber()
        s.add_secret('SECRETVALUE')
        s.add_path('/tmp/zzz', '<tmp>')
        self.assertEqual(s('a SECRETVALUE /tmp/zzz/x idk_abcdefgh1234'), 'a <redacted> <tmp>/x <redacted>')

    def test_conformant_stub_passes_and_cleans_up(self):
        self.need_bin()
        r = self.run_mode('conformant')
        bad = [(s['name'], [k for k, v in s['checks'].items() if not v]) for s in r['scenarios'] if not s['pass']]
        self.assertEqual(r['verdict'], 'PASS', bad)
        self.assertFalse(r['process_cleanup']['idoris_alive_after_run'])
        for port in CANARY_PORTS.values():  # canaries released
            with socket.socket() as s:
                s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                s.bind(('127.0.0.1', port))

    def test_stub_ignoring_locality_is_red(self):
        self.need_bin()
        r = self.run_mode('ignore_locality')
        self.assertEqual(r['verdict'], 'RED')
        self.assertIn('locality_remote', r['reason'])

    def test_bypass_stub_is_red_not_implemented_with_canary_evidence(self):
        self.need_bin()
        r = self.run_mode('bypass')
        self.assertEqual(r['verdict'], 'RED')
        self.assertIn('BYPASS', r['reason'])

    def test_noop_stub_is_red_not_implemented(self):
        self.need_bin()
        r = self.run_mode('noop')
        self.assertEqual(r['verdict'], 'RED')
        self.assertTrue(r['reason'].startswith('not-implemented'))


# --------------------------------------------------------------------------- main
def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('--idoris-bin', help='path to the real iDoris binary (required except for static-only)')
    p.add_argument('--idoris-config-dir', help='dir with components/omlx.yaml, routing-policy.yaml, catalog.yaml (default: repo config/)')
    p.add_argument('--agent24-bin', help='real Agent24 executable (argv[0] of the call command)')
    p.add_argument('--agent24-source', help='read-only Agent24 checkout: SHA + static IDORIS_URL scan')
    p.add_argument('--agent24-call-cmd', help='JSON argv array for ONE model call; {model} {role} {prompt} are substituted')
    p.add_argument('--agent24-cwd', help='working dir for Agent24 (default: isolated temp dir)')
    p.add_argument('--agent24-env', action='append', default=[], metavar='K=V', help='extra env for Agent24 (repeatable)')
    p.add_argument('--url-env', default='IDORIS_URL', help='env var carrying the iDoris base URL (spec R1 default)')
    p.add_argument('--key-env', default='IDORIS_API_KEY', help='env var carrying the virtual key (spec R1 default)')
    p.add_argument('--expect-privacy', default='local_only', choices=['local_only', 'any'],
                   help='X-iDoris-Privacy value expected from the provider under test (idoris-local => local_only)')
    p.add_argument('--no-record-id-in-output', dest='record_id_in_output', action='store_false',
                   help='skip the check that Agent24 output shows the Record-Id')
    p.add_argument('--call-timeout', type=float, default=60, help='seconds per Agent24 call before kill')
    p.add_argument('--report-json', help='write the sanitized report here (default: stdout)')
    p.add_argument('--self-test', action='store_true', help='prove this tool with stub consumers (not Agent24)')
    p.add_argument('--stub-consumer', choices=['conformant', 'ignore_locality', 'bypass', 'noop'], help=argparse.SUPPRESS)
    args, rest = p.parse_known_args(argv)
    if args.stub_consumer:
        return stub_consumer(args.stub_consumer, rest)
    if rest:
        p.error('unrecognized arguments: ' + ' '.join(rest))
    if args.self_test:
        SelfTest.idoris_bin = args.idoris_bin
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(SelfTest)
        res = unittest.TextTestRunner(verbosity=2).run(suite)
        print(f'SELF-TEST ran={res.testsRun} failures={len(res.failures)} errors={len(res.errors)} skipped={len(res.skipped)}')
        return EXIT_PASS if res.wasSuccessful() else EXIT_FAIL
    scrub = Scrubber()
    needs_bin = bool(args.agent24_bin and args.agent24_call_cmd)
    if needs_bin and not args.idoris_bin:
        p.error('--idoris-bin is required for a two-process run')
    try:
        report = acceptance(args, scrub)
    except (ToolError, ValueError) as e:
        print('TOOL ERROR:', scrub(str(e)), file=sys.stderr)
        return EXIT_TOOL
    text = scrub(json.dumps(report, indent=2, ensure_ascii=False))
    if args.report_json:
        Path(args.report_json).write_text(text + '\n')
    else:
        print(text)
    v = report['verdict']
    print(f'VERDICT: {v} - {scrub(report["reason"])}', file=sys.stderr)
    return {'PASS': EXIT_PASS, 'RED': EXIT_RED, 'BLOCKED': EXIT_BLOCKED}[v]


if __name__ == '__main__':
    sys.exit(main())
