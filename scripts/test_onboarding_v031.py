#!/usr/bin/env python3
"""v0.3.1-candidate onboarding integration against a release-shaped idoris binary.

Exercises loopback OpenAI-compatible / oMLX fixtures only. No model downloads,
subscription commands, paid calls, or real remote backends.

Usage:
  python3 scripts/test_onboarding_v031.py /absolute/path/to/extracted/idoris
  python3 scripts/test_onboarding_v031.py /path/to/idoris --case success|missing-binding|unavailable|late-backend|all
"""
from __future__ import annotations

import argparse
import contextlib
import http.server
import json
import os
from pathlib import Path
import secrets
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request

OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
SEEN: list = []
LOADED: list = []

ROLE_EXTENSION = """
extensions:
  _degradation: "Removing these bindings disables role aliases on this backend."
  idoris_role_models:
    daily: fixture-model
    auto: fixture-model
"""


class Fixture(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, body):
        raw = json.dumps(body).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_GET(self):
        if self.path == '/api/status':
            return self.reply({
                'loaded_models': LOADED.copy(),
                'model_memory_used': len(LOADED) * 1024**3,
                'model_memory_max': 8 * 1024**3,
                'pressure': 'ok',
            })
        if self.path == '/v1/models/status':
            return self.reply({
                'models': [{
                    'id': 'fixture-model',
                    'estimated_size': 1024**3,
                    'loaded': bool(LOADED),
                    'pinned': False,
                }]
            })
        assert self.path == '/v1/models', self.path
        self.reply({'object': 'list', 'data': [{'id': 'fixture-model', 'object': 'model'}]})

    def do_POST(self):
        if self.path == '/v1/models/fixture-model/load':
            LOADED[:] = ['fixture-model']
            return self.reply({})
        if self.path == '/v1/models/fixture-model/unload':
            LOADED.clear()
            return self.reply({})
        assert self.path == '/v1/chat/completions', self.path
        SEEN.append(json.loads(self.rfile.read(int(self.headers['Content-Length']))))
        self.reply({
            'id': 'fixture-chat',
            'object': 'chat.completion',
            'model': 'fixture-model',
            'choices': [{
                'index': 0,
                'message': {'role': 'assistant', 'content': 'fixture-ok'},
                'finish_reason': 'stop',
            }],
            'usage': {'prompt_tokens': 1, 'completion_tokens': 1, 'total_tokens': 2},
        })


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def request(url, token=None, body=None):
    headers = {'Content-Type': 'application/json', 'X-iDoris-Privacy': 'local_only'}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    req = urllib.request.Request(
        url,
        headers=headers,
        data=None if body is None else json.dumps(body).encode(),
    )
    try:
        response = OPENER.open(req, timeout=8)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        raw = response.read()
        return response.status, response.headers, raw


def packaged_omlx_card(binary: Path) -> str:
    path = binary.parent / 'config' / 'components' / 'omlx.yaml'
    if not path.is_file():
        raise SystemExit(
            f'missing packaged card next to binary: {path} '
            '(expect release-shaped layout: <prefix>/idoris + <prefix>/config/components/omlx.yaml)'
        )
    return path.read_text()


def write_card(components: Path, card: str, *, bind_roles: bool, endpoint: str) -> None:
    text = card.replace('http://127.0.0.1:8000', endpoint)
    if bind_roles:
        if 'idoris_role_models:' in text:
            raise AssertionError('packaged omlx card unexpectedly already binds roles')
        text = text.rstrip() + '\n' + ROLE_EXTENSION.lstrip('\n')
    (components / 'local.yaml').write_text(text)


def isolated_env(root: Path) -> dict:
    env = {k: v for k, v in os.environ.items() if not k.startswith('IDORIS_')}
    home = root / 'home'
    xdg = root / 'xdg'
    localapp = root / 'localapp'
    for path in (home, xdg, localapp, root / 'state'):
        path.mkdir(parents=True, exist_ok=True)
    env.update(
        HOME=str(home),
        XDG_DATA_HOME=str(xdg),
        XDG_CONFIG_HOME=str(root / 'xdg-config'),
        LOCALAPPDATA=str(localapp),
        IDORIS_DB_PATH=str(root / 'state.sqlite3'),
        IDORIS_STATE_DIR=str(root / 'state'),
        IDORIS_PORT=str(port()),
        IDORIS_ADMIN_PORT=str(port()),
    )
    env.pop('IDORIS_ALLOW_MOCK', None)
    return env


@contextlib.contextmanager
def server(binary, cwd, env, token):
    with (cwd / 'server.log').open('w+') as log:
        process = subprocess.Popen(
            [str(binary), 'serve', '--admin-token-stdin'],
            cwd=cwd,
            env=env,
            stdin=subprocess.PIPE,
            stdout=log,
            stderr=log,
            text=True,
        )
        process.stdin.write(token + '\n')
        process.stdin.close()
        try:
            deadline = time.monotonic() + 30
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    log.seek(0)
                    raise RuntimeError(log.read())
                try:
                    if request('http://127.0.0.1:' + env['IDORIS_PORT'] + '/health')[0] == 200:
                        break
                except (OSError, urllib.error.URLError):
                    pass
                time.sleep(0.1)
            else:
                raise RuntimeError('readiness timeout')
            yield
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            log.seek(0)
            assert token not in log.read(), 'Admin token leaked to logs'


def issue_key(binary, env, roles):
    issued = subprocess.run(
        [str(binary), 'key', 'issue', '--spec-stdin'],
        env=env,
        input=json.dumps({
            'owner': 'onboarding-v031-fixture',
            'allowed_privacy': ['local_only'],
            'allowed_roles': roles,
        }),
        text=True,
        capture_output=True,
        check=True,
    )
    return json.loads(issued.stdout)['secret']


def chat_body(model='fixture-model'):
    return {
        'model': model,
        'messages': [{'role': 'user', 'content': 'hello'}],
        'stream': False,
    }


def assert_transport_startup_error(raw: bytes) -> str:
    """Accept frozen-source transport latch text; do not pin a single historical phrase."""
    payload = json.loads(raw)
    remediation = payload['error']['remediation']
    assert payload['error']['type'] == 'local_only_unavailable', payload
    # Frozen oMLX adapter classifies connect/timeout without formatting the raw reqwest error.
    assert 'oMLX' in remediation and '/api/status' in remediation, remediation
    assert (
        'could not connect' in remediation
        or 'timed out' in remediation
        or 'transport error' in remediation
    ), remediation
    return remediation


def case_unavailable(binary, root, env, token, key, card, components, base):
    """Backend down: health may be 200, but chat must fail with zero model POSTs."""
    SEEN.clear()
    LOADED.clear()
    with socket.socket() as closed:
        closed.bind(('127.0.0.1', 0))
        endpoint = 'http://127.0.0.1:' + str(closed.getsockname()[1])
        # keep socket open so nothing accepts
        write_card(components, card, bind_roles=True, endpoint=endpoint)
        env['IDORIS_COMPONENTS_DIR'] = str(components)
        with server(binary, root, env, token):
            status, _, health_raw = request(base + '/health')
            assert status == 200, (status, health_raw)
            health = json.loads(health_raw)
            assert health.get('version'), health
            before = len(SEEN)
            status, _, raw = request(base + '/v1/chat/completions', key, chat_body('idoris/daily'))
            assert status == 503, (status, raw)
            assert_transport_startup_error(raw)
            assert len(SEEN) == before, ('unavailable path must not POST to model fixture', SEEN)
            status, _, raw = request(base + '/v1/chat/completions', key, chat_body('fixture-model'))
            assert status == 503, (status, raw)
            assert len(SEEN) == before, ('concrete unavailable must not POST', SEEN)
    print('PASS unavailable: health 200 alone insufficient; chat 503; zero fixture POSTs')


def case_missing_binding(binary, root, env, token, key, card, components, base, fixture_port):
    """Live fixture without role bindings: concrete may work; idoris/daily must fail closed with zero role POSTs."""
    SEEN.clear()
    LOADED.clear()
    endpoint = 'http://127.0.0.1:' + str(fixture_port)
    write_card(components, card, bind_roles=False, endpoint=endpoint)
    env['IDORIS_COMPONENTS_DIR'] = str(components)
    with server(binary, root, env, token):
        status, _, raw = request(base + '/v1/models', key)
        assert status == 200, (status, raw)
        assert json.loads(raw)['data'][0]['id'] == 'fixture-model', raw
        before = len(SEEN)
        status, _, raw = request(base + '/v1/chat/completions', key, chat_body('idoris/daily'))
        assert status == 503, (status, raw)
        err = json.loads(raw)['error']
        assert err['type'] == 'local_only_unavailable', raw
        assert len(SEEN) == before, ('missing binding must not POST model', SEEN)
        # Concrete override remains available on a valid HTTP card without bindings.
        status, headers, raw = request(base + '/v1/chat/completions', key, chat_body('fixture-model'))
        assert status == 200, (status, raw)
        assert json.loads(raw)['choices'][0]['message']['content'] == 'fixture-ok', raw
        assert headers.get('X-iDoris-Served-Locality') == 'loopback', headers
        assert headers.get('X-iDoris-Record-Id'), headers
        assert SEEN[-1]['model'] == 'fixture-model', SEEN
    print('PASS missing-binding: idoris/daily 503 + zero role POSTs; concrete fixture call ok')


def case_success(binary, root, env, token, key, card, components, base, fixture_port):
    """Bound daily role must resolve to concrete fixture model; fixture must observe the call."""
    SEEN.clear()
    LOADED.clear()
    endpoint = 'http://127.0.0.1:' + str(fixture_port)
    # Resident path: fixture owns residency; role binding supplies dispatch identity.
    text = card.replace('http://127.0.0.1:8000', endpoint)
    text = (
        text.replace('id: omlx', 'id: fixture-local')
        .replace('omlx@0.6.4', 'fixture@1')
        .replace('mode: on_demand', 'mode: resident')
        .replace('admission: requires_eviction', 'admission: coexist')
        .replace('idle_ttl_s: 300', 'pinned: true')
    )
    text = text.rstrip() + '\n' + ROLE_EXTENSION.lstrip('\n')
    (components / 'local.yaml').write_text(text)
    env['IDORIS_COMPONENTS_DIR'] = str(components)
    with server(binary, root, env, token):
        status, _, health_raw = request(base + '/health')
        assert status == 200, health_raw
        models = json.loads(request(base + '/v1/models', key)[2])
        assert models['data'][0]['id'] == 'fixture-model', models
        before = len(SEEN)
        status, headers, raw = request(base + '/v1/chat/completions', key, chat_body('idoris/daily'))
        assert status == 200, (status, raw)
        assert json.loads(raw)['choices'][0]['message']['content'] == 'fixture-ok', raw
        assert headers.get('X-iDoris-Served-Locality') == 'loopback', dict(headers)
        assert headers.get('X-iDoris-Record-Id'), dict(headers)
        assert len(SEEN) == before + 1, SEEN
        # Role alias must be rewritten to the configured concrete backend model.
        assert SEEN[-1]['model'] == 'fixture-model', SEEN
        status, _, raw = request(base + '/v1/chat/completions', key, chat_body('fixture-model'))
        assert status == 200, (status, raw)
        assert SEEN[-1]['model'] == 'fixture-model', SEEN
        assert len(SEEN) == before + 2, SEEN
    print('PASS success: key+serve+discovery; idoris/daily -> fixture-model; Record-Id; loopback; fixture POSTs')


def case_late_backend(binary, root, env, token, key, card, components, base):
    """Unavailable at startup latches until owned service restart (frozen source behavior)."""
    SEEN.clear()
    LOADED.clear()
    with socket.socket() as unavailable:
        unavailable.bind(('127.0.0.1', 0))
        address = unavailable.getsockname()
        endpoint = 'http://127.0.0.1:' + str(address[1])
        write_card(components, card, bind_roles=True, endpoint=endpoint)
        env['IDORIS_COMPONENTS_DIR'] = str(components)
        body = chat_body('fixture-model')
        with contextlib.ExitStack() as cleanup:
            with server(binary, root, env, token):
                status, _, raw = request(base + '/v1/chat/completions', key, body)
                assert status == 503, (status, raw)
                startup_error = assert_transport_startup_error(raw)
                unavailable.close()
                upstream = http.server.ThreadingHTTPServer(address, Fixture)
                thread = threading.Thread(target=upstream.serve_forever, daemon=True)
                thread.start()
                cleanup.callback(thread.join)
                cleanup.callback(upstream.server_close)
                cleanup.callback(upstream.shutdown)
                status, _, raw = request(base + '/v1/models', key)
                assert status == 200, (status, raw)
                assert json.loads(raw)['data'][0]['id'] == 'fixture-model', raw
                count = len(SEEN)
                status, _, raw = request(base + '/v1/chat/completions', key, body)
                assert status == 503, (status, raw)
                assert json.loads(raw)['error']['remediation'] == startup_error, raw
                assert len(SEEN) == count, 'latched startup error unexpectedly called backend'
            with server(binary, root, env, token):
                status, _, raw = request(base + '/v1/chat/completions', key, body)
                assert status == 200, (status, raw)
                assert json.loads(raw)['choices'][0]['message']['content'] == 'fixture-ok', raw
                assert SEEN[-1]['model'] == 'fixture-model', SEEN
    print('PASS late-backend: unavailable startup; models 200 + latched chat 503; restart + chat 200')


def main(binary: str, case: str):
    binary = Path(binary).resolve(strict=True)
    card = packaged_omlx_card(binary)
    SEEN.clear()
    LOADED.clear()
    with tempfile.TemporaryDirectory(prefix='idoris-onboarding-v031-') as temp:
        root = Path(temp)
        env = isolated_env(root)
        components = root / 'components'
        components.mkdir()
        # Issue keys against isolated DB before serve.
        limited = issue_key(binary, env, ['daily'])
        key = issue_key(
            binary,
            env,
            ['fast', 'daily', 'deep', 'vision', 'embed', 'rerank', 'decide', 'auto'],
        )
        token = secrets.token_hex(32)
        base = 'http://127.0.0.1:' + env['IDORIS_PORT']
        # Auth smoke: no-key must 401; health alone is not the success criterion.
        write_card(components, card, bind_roles=False, endpoint='http://127.0.0.1:1')
        env['IDORIS_COMPONENTS_DIR'] = str(components)
        with server(binary, root, env, token):
            health = json.loads(request(base + '/health')[2])
            assert health.get('version'), health
            assert request(base + '/v1/chat/completions', body=chat_body('m'))[0] == 401
            assert request(
                'http://127.0.0.1:' + env['IDORIS_ADMIN_PORT'] + '/admin/api/v1/status'
            )[0] == 401
            assert request(
                'http://127.0.0.1:' + env['IDORIS_ADMIN_PORT'] + '/admin/api/v1/status',
                token,
            )[0] == 200
        print('PASS bootstrap: isolated HOME/DB/ports; version present; no-key 401; admin auth')

        want = {'success', 'missing-binding', 'unavailable', 'late-backend'}
        if case != 'all':
            want = {case}

        if 'unavailable' in want:
            case_unavailable(binary, root, env, token, key, card, components, base)

        if 'late-backend' in want:
            case_late_backend(binary, root, env, token, key, card, components, base)

        if 'missing-binding' in want or 'success' in want:
            upstream = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
            thread = threading.Thread(target=upstream.serve_forever, daemon=True)
            thread.start()
            try:
                if 'missing-binding' in want:
                    case_missing_binding(
                        binary, root, env, token, key, card, components, base,
                        upstream.server_port,
                    )
                if 'success' in want:
                    case_success(
                        binary, root, env, token, key, card, components, base,
                        upstream.server_port,
                    )
                # limited key must not expand roles
                if 'success' in want:
                    write_card(
                        components,
                        card,
                        bind_roles=True,
                        endpoint='http://127.0.0.1:' + str(upstream.server_port),
                    )
                    # use resident+bind for role denial check
                    text = (components / 'local.yaml').read_text()
                    text = (
                        text.replace('mode: on_demand', 'mode: resident')
                        .replace('admission: requires_eviction', 'admission: coexist')
                        .replace('idle_ttl_s: 300', 'pinned: true')
                    )
                    (components / 'local.yaml').write_text(text)
                    with server(binary, root, env, token):
                        status, _, raw = request(
                            base + '/v1/chat/completions',
                            limited,
                            chat_body('idoris/deep'),
                        )
                        assert status == 403, (status, raw)
                    print('PASS role-ceiling: limited key denied for unbound-authorized deep')
            finally:
                upstream.shutdown()
                upstream.server_close()
                thread.join()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary')
    parser.add_argument(
        '--case',
        default='all',
        choices=['all', 'success', 'missing-binding', 'unavailable', 'late-backend'],
    )
    args = parser.parse_args()
    main(args.binary, args.case)
