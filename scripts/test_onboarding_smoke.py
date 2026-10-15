#!/usr/bin/env python3
"""Pin v0.3.0 behavior (adapt expectations before use with v0.3.1).

Exercise a release binary in disposable storage, with a loopback HTTP fixture.

No model downloads, subscription commands, paid calls, or real backend requests.
Usage: python3 scripts/test_onboarding_smoke.py /absolute/path/to/extracted/idoris
"""
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
SEEN = []
LOADED = []


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
            return self.reply({'loaded_models': LOADED.copy(), 'model_memory_used': len(LOADED) * 1024**3,
                               'model_memory_max': 8 * 1024**3, 'pressure': 'ok'})
        if self.path == '/v1/models/status':
            return self.reply({'models': [{'id': 'fixture-model', 'estimated_size': 1024**3,
                                          'loaded': bool(LOADED), 'pinned': False}]})
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
        self.reply({'id': 'fixture-chat', 'object': 'chat.completion',
                    'model': 'fixture-model', 'choices': [{'index': 0,
                    'message': {'role': 'assistant', 'content': 'fixture-ok'},
                    'finish_reason': 'stop'}],
                    'usage': {'prompt_tokens': 1, 'completion_tokens': 1, 'total_tokens': 2}})


def port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def request(url, token=None, body=None):
    headers = {'Content-Type': 'application/json', 'X-iDoris-Privacy': 'local_only'}
    if token:
        headers['Authorization'] = 'Bearer ' + token
    req = urllib.request.Request(url, headers=headers,
                                 data=None if body is None else json.dumps(body).encode())
    try:
        response = OPENER.open(req, timeout=8)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        raw = response.read()
        return response.status, response.headers, raw


@contextlib.contextmanager
def server(binary, cwd, env, token):
    with (cwd / 'server.log').open('w+') as log:
        process = subprocess.Popen([str(binary), 'serve', '--admin-token-stdin'],
                                   cwd=cwd, env=env, stdin=subprocess.PIPE,
                                   stdout=log, stderr=log, text=True)
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
                time.sleep(.1)
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


def late_backend_case(binary, root, env, token, key, card, components, base):
    """Released 0.3.0 latches startup_error until our own service restarts."""
    LOADED.clear()
    with socket.socket() as unavailable:
        unavailable.bind(('127.0.0.1', 0))
        address = unavailable.getsockname()
        endpoint = 'http://127.0.0.1:' + str(address[1])
        (components / 'local.yaml').write_text(card.replace('http://127.0.0.1:8000', endpoint))
        body = {'model': 'fixture-model', 'messages': [{'role': 'user', 'content': 'hello'}],
                'stream': False}
        with contextlib.ExitStack() as cleanup:
            with server(binary, root, env, token):
                status, _, raw = request(base + '/v1/chat/completions', key, body)
                assert status == 503, (status, raw)
                startup_error = json.loads(raw)['error']['remediation']
                assert 'oMLX GET /api/status' in startup_error, raw
                assert ('could not connect' in startup_error or
                        'timed out (transport error)' in startup_error), raw
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
            # The server context stops and waits for only the subprocess it owns.
            with server(binary, root, env, token):
                status, _, raw = request(base + '/v1/chat/completions', key, body)
                assert status == 200, (status, raw)
                assert json.loads(raw)['choices'][0]['message']['content'] == 'fixture-ok', raw
                assert SEEN[-1]['model'] == 'fixture-model'
    print('PASS late backend: unavailable startup; models 200 + latched chat 503; owned service restart + chat 200')


def main(binary, late_backend_only=False):
    binary = Path(binary).resolve(strict=True)
    SEEN.clear()
    LOADED.clear()
    env = {k: v for k, v in os.environ.items() if not k.startswith('IDORIS_')}
    with tempfile.TemporaryDirectory(prefix='idoris-onboarding-') as temp:
        root = Path(temp)
        env.update(IDORIS_DB_PATH=str(root / 'state.sqlite3'),
                   IDORIS_STATE_DIR=str(root / 'state'),
                   IDORIS_PORT=str(port()), IDORIS_ADMIN_PORT=str(port()))
        issued = subprocess.run([str(binary), 'key', 'issue', '--spec-stdin'], env=env,
                                input=json.dumps({'owner': 'onboarding-fixture',
                                'allowed_privacy': ['local_only'], 'allowed_roles': ['daily']}),
                                text=True, capture_output=True, check=True)
        key = json.loads(issued.stdout)['secret']
        limited_key = key
        spec = {'owner': 'onboarding-concrete-fixture', 'allowed_privacy': ['local_only'],
                'allowed_roles': ['fast', 'daily', 'deep', 'vision', 'embed', 'rerank', 'decide', 'auto']}
        issued = subprocess.run([str(binary), 'key', 'issue', '--spec-stdin'], env=env,
                                input=json.dumps(spec), text=True, capture_output=True, check=True)
        key = json.loads(issued.stdout)['secret']
        token = secrets.token_hex(32)
        base = 'http://127.0.0.1:' + env['IDORIS_PORT']
        admin = 'http://127.0.0.1:' + env['IDORIS_ADMIN_PORT']
        # Keep packaged configuration, but avoid contacting an owner's real oMLX.
        components = root / 'components'
        components.mkdir()
        card = (binary.parent / 'config/components/omlx.yaml').read_text()
        with socket.socket() as unavailable:
            unavailable.bind(('127.0.0.1', 0))
            endpoint = 'http://127.0.0.1:' + str(unavailable.getsockname()[1])
            (components / 'local.yaml').write_text(card.replace('http://127.0.0.1:8000', endpoint))
            env['IDORIS_COMPONENTS_DIR'] = str(components)
            with server(binary, root, env, token):
                health = json.loads(request(base + '/health')[2])
                assert health['version'] == '0.3.0', health
                assert request(base + '/v1/chat/completions', body={'model': 'm', 'messages': []})[0] == 401
                assert json.loads(request(base + '/v1/models', key)[2])['data'] == []
                assert request(admin + '/admin/api/v1/status')[0] == 401
                assert request(admin + '/admin/api/v1/status', token)[0] == 200
                assert b'Session token' in request(admin + '/admin/')[2]
        print('PASS release startup outside repository; version; no-key 401; unavailable model list; Admin auth/page')
        late_backend_case(binary, root, env, token, key, card, components, base)
        if late_backend_only:
            return
        SEEN.clear()
        LOADED.clear()
        upstream = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Fixture)
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        try:
            card = card.replace('http://127.0.0.1:8000', 'http://127.0.0.1:' + str(upstream.server_port))
            card = card.replace('id: omlx', 'id: fixture-local').replace('omlx@0.6.4', 'fixture@1')
            card = card.replace('mode: on_demand', 'mode: resident').replace('admission: requires_eviction', 'admission: coexist').replace('idle_ttl_s: 300', 'pinned: true')
            (components / 'local.yaml').write_text(card)
            with server(binary, root, env, token):
                models = json.loads(request(base + '/v1/models', key)[2])
                assert models['data'][0]['id'] == 'fixture-model', models
                body = {'model': 'fixture-model', 'messages': [{'role': 'user', 'content': 'hello'}], 'stream': False}
                status, headers, raw = request(base + '/v1/chat/completions', key, body)
                assert status == 200, (status, raw)
                assert json.loads(raw)['choices'][0]['message']['content'] == 'fixture-ok'
                assert headers['X-iDoris-Served-Locality'] == 'loopback'
                assert headers['X-iDoris-Record-Id']
                body['model'] = 'idoris/daily'
                status, _, raw = request(base + '/v1/chat/completions', key, body)
                assert status == 200, (status, raw)
                body['model'] = 'idoris/deep'
                assert request(base + '/v1/chat/completions', limited_key, body)[0] == 403
                result = subprocess.run([str(binary), 'admin', 'status', '--token-stdin'],
                                        env=env, input=token + '\n', text=True, capture_output=True, check=True)
                assert json.loads(result.stdout)
                assert len(SEEN) == 2, SEEN
                assert SEEN[1]['model'] == 'idoris/daily', SEEN
                print('PASS persisted key bootstrap/restart; model discovery; concrete+daily calls; locality/record headers; role denial; Admin CLI')
                print('Fixture received model fields:', [item['model'] for item in SEEN])
            # Restore the packaged lifecycle card shape against our oMLX-shaped fixture.
            lifecycle = (binary.parent / 'config/components/omlx.yaml').read_text()
            lifecycle = lifecycle.replace('http://127.0.0.1:8000', 'http://127.0.0.1:' + str(upstream.server_port))
            (components / 'local.yaml').write_text(lifecycle)
            with server(binary, root, env, token):
                body['model'] = 'fixture-model'
                status, _, raw = request(base + '/v1/chat/completions', key, body)
                assert status == 200, (status, raw)
                assert LOADED == ['fixture-model']
                assert SEEN[-1]['model'] == 'fixture-model'
                count = len(SEEN)
                body['max_tokens'] = 8
                status, _, raw = request(base + '/v1/chat/completions', key, body)
                assert status == 400 and json.loads(raw)['error']['type'] == 'unsupported_field'
                del body['max_tokens']
                body['model'] = 'idoris/daily'
                status, _, raw = request(base + '/v1/chat/completions', limited_key, body)
                assert status == 503, (status, raw)
                assert json.loads(raw)['error']['type'] == 'local_only_unavailable'
                assert json.loads(raw)['error']['remediation'] == 'model not found: omlx', raw
                assert len(SEEN) == count, 'unresolved role unexpectedly invoked a model'
                print('PASS oMLX-shaped list/status/load/chat; concrete model works; default daily alias fails closed (known product gap)')
        finally:
            upstream.shutdown()
            upstream.server_close()
            thread.join()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary')
    parser.add_argument('--late-backend-only', action='store_true')
    args = parser.parse_args()
    main(args.binary, args.late_backend_only)
