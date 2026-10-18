#!/usr/bin/env python3
"""Validate package startup/version in isolated fresh user state.

Payload signatures must be verified separately. The child inherits no IDORIS_*
variables and must create idoris.sqlite3 under the disposable home.
"""
import argparse
import errno
import json
import os
from pathlib import Path
import socket
import subprocess
import tarfile
import tempfile
import time
import urllib.error
import urllib.request

CONFIG_FILES = (
    "config/components/omlx.yaml", "config/components/subscription.yaml",
    "config/routing-policy.yaml", "config/catalog.yaml",
)
PAYLOAD_FILES = {"idoris", *CONFIG_FILES}


def validate_payload(directory):
    bundle = Path(os.path.abspath(directory))
    for name in PAYLOAD_FILES:
        path = bundle / name
        if any(part.is_symlink() for part in (path, *path.parents)):
            raise RuntimeError("payload paths must not traverse symbolic links")
        if not path.is_file():
            raise RuntimeError("payload requires idoris and four regular public configs")
    if not os.access(bundle / "idoris", os.X_OK):
        raise RuntimeError("payload CLI must be executable")
    return bundle


def smoke_payload(directory, expected):
    """Execute the original payload in place, without writing into its directory."""
    bundle = validate_payload(directory)
    with tempfile.TemporaryDirectory(prefix="idoris-release-smoke-") as temp:
        root = Path(temp)
        cwd, home = root / "empty-cwd", root / "home"
        cwd.mkdir()
        home.mkdir()
        # REUSEADDR permits quick repeats after TIME_WAIT on macOS, but can
        # bind over an existing wildcard listener. Refuse accepting listeners
        # on both loopbacks before spawning; unexpected probe errors fail closed.
        for port in (8740, 8741):
            for family, host in ((socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")):
                try:
                    with socket.socket(family) as listener_probe:
                        listener_probe.settimeout(0.25)
                        result = listener_probe.connect_ex((host, port))
                except OSError as error:
                    if error.errno not in (errno.EAFNOSUPPORT, errno.EPROTONOSUPPORT):
                        raise
                    continue
                if result == 0:
                    raise OSError(errno.EADDRINUSE, "release smoke port already has a listener")
                if result not in (errno.ECONNREFUSED, errno.EADDRNOTAVAIL):
                    raise OSError(result, "cannot establish release smoke port availability")
            with socket.socket() as probe:
                probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                probe.bind(("127.0.0.1", port))
        env = {k: v for k, v in os.environ.items() if not k.startswith("IDORIS_")}
        # Override only the child's user-directory environment. Never read/write
        # the operator's existing key store, tenant budgets, or load fences.
        env.update(HOME=str(home),
                   XDG_DATA_HOME=str(home / "data"),
                   XDG_STATE_HOME=str(home / "state"),
                   LOCALAPPDATA=str(home / "appdata"))
        with (root / "server.log").open("w+") as log:
            # Stay in the CLI process group so a group kill on CLI timeout
            # reaps this payload. Do not setsid or start_new_session.
            process = subprocess.Popen([str(bundle / "idoris")], cwd=cwd, env=env,
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
            try:
                opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
                deadline = time.monotonic() + 45
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        raise RuntimeError(f"server exited: {process.returncode}")
                    try:
                        with opener.open("http://127.0.0.1:8740/health", timeout=2) as response:
                            health = json.load(response)
                    except (urllib.error.URLError, TimeoutError):
                        time.sleep(0.25)
                        continue
                    if process.poll() is not None:
                        raise RuntimeError("server exited during health check")
                    if (health.get("status"), health.get("version"), health.get("components")) != ("ok", expected, 1):
                        raise RuntimeError(f"unexpected health: {health!r}; expected version {expected!r}")
                    if not list(home.rglob("idoris.sqlite3")):
                        raise RuntimeError("default database was not created in isolated user state")
                    print("release smoke: fresh user state + default database verified")
                    return
                raise RuntimeError("/health readiness timed out")
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                # Server logs may contain temporary Admin tokens. Do not publish them.


def main(archive, expected):
    """Keep the legacy archive + expected-version invocation."""
    with tempfile.TemporaryDirectory(prefix="idoris-release-archive-") as temp:
        bundle = Path(temp).resolve()
        with tarfile.open(archive, "r:gz") as package:
            members = package.getmembers()
            if (len(members) != len(PAYLOAD_FILES)
                    or {m.name for m in members} != PAYLOAD_FILES
                    or any(not m.isfile() for m in members)):
                raise RuntimeError("archive must contain only idoris and the four runtime config files")
            package.extractall(bundle)
        smoke_payload(bundle, expected)


def cli():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", nargs="?")
    parser.add_argument("expected", nargs="?")
    parser.add_argument("--payload-dir", type=Path,
                        help="already signature-verified, read-only payload containing idoris and config/")
    parser.add_argument("--expected-version", help="exact /health version for --payload-dir")
    args = parser.parse_args()
    if args.payload_dir is not None:
        if args.archive or args.expected or not args.expected_version:
            parser.error("use --payload-dir DIR --expected-version VERSION without archive arguments")
        smoke_payload(args.payload_dir, args.expected_version)
    else:
        if not args.archive or not args.expected or args.expected_version:
            parser.error("use ARCHIVE EXPECTED_VERSION or --payload-dir DIR --expected-version VERSION")
        main(args.archive, args.expected)


if __name__ == "__main__":
    cli()
