#!/usr/bin/env python3
"""Run the actual release archive with no IDORIS_* variables, outside the repository."""
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
            names = {"idoris", "config/components/omlx.yaml", "config/routing-policy.yaml"}
            if {m.name for m in members} != names or any(not m.isfile() for m in members):
                raise RuntimeError("archive must contain only idoris and the two runtime config files")
            package.extractall(bundle)
        # Refuse an occupied port so another process cannot supply a false green.
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 8740))
        env = {k: v for k, v in os.environ.items() if not k.startswith("IDORIS_")}
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
