"""Package/startup controls; no test establishes Apple signing acceptance."""
import importlib.util
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tarfile
import tempfile
import textwrap
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("release-smoke.py")
SPEC = importlib.util.spec_from_file_location("release_smoke", SCRIPT)
smoke = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(smoke)
WORKFLOW = SCRIPT.parent.parent / ".github/workflows/release.yml"

SERVER = r'''#!/usr/bin/env python3
import json, os
from pathlib import Path
from http.server import BaseHTTPRequestHandler, HTTPServer
assert not any(k.startswith("IDORIS_") for k in os.environ)
assert Path.cwd() != Path(__file__).parent
assert Path(os.environ["HOME"]) != Path(__file__).parent
for name in ("components/omlx.yaml", "components/subscription.yaml", "routing-policy.yaml", "catalog.yaml"):
    assert (Path(__file__).parent / "config" / name).is_file()
class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(json.dumps({"status":"ok", "version":"0.3.0", "components":1}).encode())
    def log_message(self, *args): pass
HTTPServer.allow_reuse_address = True
HTTPServer(("127.0.0.1", 8740), Handler).serve_forever()
'''


class SmokeBehavior(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.bundle = self.root / "payload ; literal"
        self.bundle.mkdir()
        (self.bundle / "idoris").write_text(SERVER)
        (self.bundle / "idoris").chmod(0o755)
        for name in smoke.CONFIG_FILES:
            target = self.bundle / name
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("public: true\n")

    def test_payload_runs_in_place_without_modification_and_scrubs_environment(self):
        before = {n: (self.bundle / n).read_bytes() for n in smoke.PAYLOAD_FILES}
        original = subprocess.Popen
        children = []
        def spawn(*args, **kwargs):
            children.append(original(*args, **kwargs))
            return children[-1]
        with patch.dict(os.environ, {"IDORIS_COMPONENTS_DIR": "/invalid"}), patch.object(smoke.subprocess, "Popen", side_effect=spawn):
            smoke.smoke_payload(self.bundle, "0.3.0")
        self.assertEqual(before, {n: (self.bundle / n).read_bytes() for n in smoke.PAYLOAD_FILES})
        self.assertEqual(len(children), 1)
        self.assertIsNotNone(children[0].poll(), "owned server must be reaped")

    def test_wrong_version_fails_and_reaps_real_http_server(self):
        original = subprocess.Popen
        children = []
        def spawn(*args, **kwargs):
            children.append(original(*args, **kwargs))
            return children[-1]
        with patch.object(smoke.subprocess, "Popen", side_effect=spawn):
            with self.assertRaisesRegex(RuntimeError, "expected version '0.3.1'"):
                smoke.smoke_payload(self.bundle, "0.3.1")
        self.assertIsNotNone(children[0].poll())

    def test_existing_listener_cannot_supply_a_false_green(self):
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind(("127.0.0.1", 8740))
            listener.listen()
            with patch.object(smoke.subprocess, "Popen") as spawn:
                with self.assertRaises(OSError):
                    smoke.smoke_payload(self.bundle, "0.3.0")
                spawn.assert_not_called()

    def test_nonexecutable_cli_refuses_before_execution(self):
        (self.bundle / "idoris").chmod(0o644)
        with patch.object(smoke.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(RuntimeError, "executable"):
                smoke.smoke_payload(self.bundle, "0.3.0")
            spawn.assert_not_called()

    def test_missing_config_refuses_before_execution(self):
        (self.bundle / smoke.CONFIG_FILES[0]).unlink()
        with patch.object(smoke.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(RuntimeError, "regular public configs"):
                smoke.smoke_payload(self.bundle, "0.3.0")
            spawn.assert_not_called()

    def test_symlink_files_or_parent_refuse_before_execution(self):
        for name in ("idoris", smoke.CONFIG_FILES[0], "config/components"):
            with self.subTest(name=name):
                target = self.bundle / name
                saved = self.root / "saved"
                target.rename(saved)
                target.symlink_to(saved, target_is_directory=saved.is_dir())
                try:
                    with patch.object(smoke.subprocess, "Popen") as spawn:
                        with self.assertRaisesRegex(RuntimeError, "symbolic links"):
                            smoke.smoke_payload(self.bundle, "0.3.0")
                        spawn.assert_not_called()
                finally:
                    target.unlink()
                    saved.rename(target)

    def archive(self, extra=None):
        archive = self.root / "package.tar.gz"
        with tarfile.open(archive, "w:gz") as package:
            for name in sorted(smoke.PAYLOAD_FILES):
                if extra == "missing" and name == smoke.CONFIG_FILES[0]:
                    continue
                package.add(self.bundle / name, arcname=name)
            if extra and extra != "missing":
                entry = tarfile.TarInfo(extra)
                if extra == "symlink":
                    entry.type, entry.linkname = tarfile.SYMTYPE, "/outside"
                package.addfile(entry, io.BytesIO(b""))
        return archive

    def test_legacy_archive_cli_invocation_still_runs(self):
        result = subprocess.run([sys.executable, str(SCRIPT), str(self.archive()), "0.3.0"],
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_bad_archive_members_refuse_without_extracting_or_running(self):
        for extra in ("missing", "idoris", "../escape", "/absolute", "extra", "symlink"):
            with self.subTest(extra=extra):
                with patch.object(smoke.subprocess, "Popen") as spawn:
                    with self.assertRaisesRegex(RuntimeError, "archive must contain only"):
                        smoke.main(self.archive(extra), "0.3.0")
                    spawn.assert_not_called()
        invalid = self.root / "invalid.tar.gz"
        invalid.write_bytes(b"not a tar archive")
        with self.assertRaises(tarfile.TarError):
            smoke.main(invalid, "0.3.0")

    def test_payload_cli_invocation_and_mixed_arguments(self):
        result = subprocess.run([sys.executable, str(SCRIPT), "--payload-dir", str(self.bundle),
                                 "--expected-version", "0.3.0"], capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = subprocess.run([sys.executable, str(SCRIPT), "old.tar.gz", "0.3.0",
                                 "--payload-dir", str(self.bundle), "--expected-version", "0.3.0"],
                                capture_output=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)


class WorkflowBehavior(unittest.TestCase):
    def test_tag_gate_refuses_old_release_wrong_checkout_or_nonmain_source(self):
        block = WORKFLOW.read_text().split("name: Require tag to point at a commit on main", 1)[1]
        command = textwrap.dedent(block.split("        run: |\n", 1)[1].split("      - name: Install Rust toolchain", 1)[0])
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            git = root / "git"
            git.write_text("#!/usr/bin/env python3\nimport os, sys\n"
                           "if sys.argv[1] == 'rev-parse':\n"
                           " print(os.environ['CHECKOUT_SHA'] if sys.argv[2] == 'HEAD' else os.environ['GITHUB_SHA'])\n"
                           "if sys.argv[1] == 'merge-base': sys.exit(int(os.environ['ANCESTRY_EXIT']))\n")
            git.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       GITHUB_REF_NAME="v0.3.1", GITHUB_SHA="exact-source", CHECKOUT_SHA="exact-source", ANCESTRY_EXIT="0")
            for changes, success in [({}, True), ({"GITHUB_REF_NAME":"v0.3.0"}, False),
                                     ({"CHECKOUT_SHA":"other-source"}, False), ({"ANCESTRY_EXIT":"1"}, False)]:
                with self.subTest(changes=changes):
                    result = subprocess.run(["bash", "-c", command], env=dict(env, **changes),
                                            capture_output=True, timeout=5)
                    self.assertEqual(result.returncode == 0, success, result.stderr)

    def test_draft_creation_has_no_assets_and_preserves_quoted_tag(self):
        source = WORKFLOW.read_text()
        release_job = source.split("\n  release:\n", 1)[1]
        self.assertIn("if: ${{ github.event_name == 'push' }}", release_job)
        self.assertNotIn("download-artifact", release_job)
        command = textwrap.dedent(release_job.split("        run: |\n", 1)[1])
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            gh = root / "gh"
            gh.write_text("#!/usr/bin/env python3\nimport json, os, sys\n"
                          "open(os.environ['ARGV_LOG'], 'w').write(json.dumps(sys.argv[1:]))\n"
                          "sys.exit(int(os.environ.get('FAKE_GH_EXIT', '0')))\n")
            gh.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       GITHUB_REF_NAME="v0.3.1; touch should-not-exist", GITHUB_REPOSITORY="owner/repo",
                       ARGV_LOG=str(root / "argv.json"))
            result = subprocess.run(["bash", "-c", command], cwd=root, env=env, timeout=5)
            self.assertEqual(result.returncode, 0)
            self.assertEqual(json.loads((root / "argv.json").read_text()),
                             ["release", "create", env["GITHUB_REF_NAME"], "--repo", "owner/repo",
                              "--verify-tag", "--draft", "--title", env["GITHUB_REF_NAME"], "--generate-notes"])
            self.assertFalse((root / "should-not-exist").exists())
            env["FAKE_GH_EXIT"] = "1"  # Existing release/provider failure must stop the step.
            self.assertNotEqual(subprocess.run(["bash", "-c", command], cwd=root, env=env, timeout=5).returncode, 0)


if __name__ == "__main__":
    unittest.main()
