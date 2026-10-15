#!/usr/bin/env python3
"""Run the archive outside the repository with no IDORIS_* and fresh user state."""
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request


def main(archive, expected):
    with tempfile.TemporaryDirectory(prefix="idoris-release-smoke-") as temp:
        root = Path(temp)
        bundle, cwd = root / "package", root / "empty-cwd"
        bundle.mkdir()
        cwd.mkdir()
        with tarfile.open(archive, "r:gz") as package:
            members = package.getmembers()
            names = {
                "idoris",
                "config/components/omlx.yaml",
                "config/components/subscription.yaml",
                "config/routing-policy.yaml",
                "config/catalog.yaml",
            }
            if {m.name for m in members} != names or any(not m.isfile() for m in members):
                raise RuntimeError("archive must contain only idoris and the four runtime config files")
            package.extractall(bundle)
        # Refuse an occupied port so another process cannot supply a false green.
        for port in (8740, 8741):
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", port))
        env = {k: v for k, v in os.environ.items() if not k.startswith("IDORIS_")}
        # Override only the child's user-directory environment. Never read/write
        # the operator's existing key store, tenant budgets, or load fences.
        test_user_dir = root / "test-user"
        test_user_dir.mkdir()
        env.update(HOME=str(test_user_dir),
                   XDG_DATA_HOME=str(test_user_dir / "data"),
                   XDG_STATE_HOME=str(test_user_dir / "state"),
                   LOCALAPPDATA=str(test_user_dir / "appdata"))
        with (root / "server.log").open("w+") as log:
            process = subprocess.Popen([str(bundle / "idoris")], cwd=cwd, env=env,
                                       stdout=log, stderr=subprocess.STDOUT)
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
                    if (health.get("status"), health.get("version"), health.get("components")) != ("ok", expected, 1):
                        raise RuntimeError(f"unexpected health: {health!r}; expected version {expected!r}")
                    if not list(test_user_dir.rglob("idoris.sqlite3")):
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
                    process.wait()
                log.seek(0)
                print(log.read(), end="")


if __name__ == "__main__":
    main(*sys.argv[1:])
