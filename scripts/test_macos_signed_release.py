#!/usr/bin/env python3
"""Behavior tests only: fake Apple tools NEVER establish signing acceptance."""
import io
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import sys
import tempfile
import unittest
from unittest.mock import patch

import macos_signed_release as release

TEAM = "ABCDEFGHIJ"
IDENTITY = "A" * 40
SUBMISSION = "12345678-1234-1234-1234-123456789abc"


class AppleDouble:
    def __init__(self, fail=None, status="Accepted", corrupt=False):
        self.stages = []
        self.fail, self.status, self.corrupt = fail, status, corrupt
        self.payload = None

    def __call__(self, stage, argv):
        self.stages.append(stage)
        if stage == self.fail:
            raise release.ReleaseError("injected tool failure")
        if stage == "identity lookup":
            return f'1) {IDENTITY} "Developer ID Application: Test ({TEAM})"'
        if stage == "CLI signing":
            with Path(argv[-1]).open("ab") as stream:
                stream.write(b"signed")
        if stage == "signature metadata":
            return (f"TeamIdentifier={TEAM}\nAuthority=Developer ID Application: Test ({TEAM})\n"
                    "Timestamp=Oct 15, 2026\nCodeDirectory flags=0x10000(runtime)\n")
        if stage == "DMG creation":
            self.payload = Path(argv[argv.index("-srcfolder") + 1])
            Path(argv[-1]).write_bytes(b"fake UDIF")
        if stage == "DMG signing":
            with Path(argv[-1]).open("ab") as stream:
                stream.write(b"image signature")
        if stage == "notary submit":
            return json.dumps({"id": SUBMISSION, "message": "Successfully uploaded file.",
                               "path": "test-image.dmg"})
        if stage == "notary info":
            return json.dumps({"id": SUBMISSION, "status": self.status})
        if stage == "ticket stapling":
            with Path(argv[-1]).open("ab") as stream:
                stream.write(b"final ticket")
        if stage == "DMG mount":
            mount = Path(argv[argv.index("-mountpoint") + 1])
            shutil.copytree(self.payload, mount, dirs_exist_ok=True)
            if self.corrupt:
                (mount / "config/catalog.yaml").write_text("corrupt")
        return ""


class ReleaseBehavior(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.binary = self.root / "CLI ; $(not-a-command)"
        self.binary.write_bytes(struct.pack("<8I", 0xFEEDFACF, 0x0100000C, 0, 2, 0, 0, 0, 0))
        self.config = self.root / "config"
        for name in release.CONFIG:
            path = self.config / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("public: true\n")
        self.args = release.parser().parse_args([
            "--binary", str(self.binary), "--config-root", str(self.config),
            "--output", str(self.root / "result"), "--version", "0.3.1",
            "--source-sha", "b" * 40, "--binary-sha256", release.digest(self.binary),
            "--identity", IDENTITY, "--team-id", TEAM, "--notary-profile", "test-alias"])
        self.addCleanup(patch.stopall)
        patch("macos_signed_release.platform.system", return_value="Darwin").start()
        patch("sys.stdout", new_callable=io.StringIO).start()
        patch("sys.stderr", new_callable=io.StringIO).start()

    def test_final_bytes_and_public_manifest(self):
        fake = AppleDouble()
        release.release(self.args, fake)
        image = next(self.args.output.glob("*.dmg"))
        self.assertTrue(image.read_bytes().endswith(b"final ticket"))
        manifest = json.loads(next(self.args.output.glob("*.json")).read_text())
        self.assertEqual(manifest["sha256"], release.digest(image))
        self.assertNotEqual(manifest["signed_binary_sha256"], self.args.binary_sha256)
        self.assertEqual(next(self.args.output.glob("*.sha256")).read_text(),
                         f"{release.digest(image)}  {image.name}\n")
        self.assertNotIn(str(self.root), json.dumps(manifest))
        self.assertNotIn("test-alias", json.dumps(manifest))
        self.assertLess(fake.stages.index("ticket stapling"), fake.stages.index("DMG mount"))
        self.assertEqual(fake.stages[-1], "DMG detach")

    def test_every_failed_stage_stops_without_publishing(self):
        baseline = AppleDouble()
        release.release(self.args, baseline)
        shutil.rmtree(self.args.output)
        for stage in dict.fromkeys(baseline.stages):
            with self.subTest(stage=stage):
                fake = AppleDouble(fail=stage)
                with self.assertRaises(release.ReleaseError):
                    release.release(self.args, fake)
                self.assertFalse(self.args.output.exists())
                # Only necessary unmount cleanup may follow a failed mounted check.
                tail = fake.stages[fake.stages.index(stage) + 1:]
                self.assertTrue(not tail or tail == ["DMG detach"], tail)

    def test_rejection_never_staples(self):
        for status in ("Invalid", "Rejected", "unknown", None):
            fake = AppleDouble(status=status)
            with self.assertRaises(release.ReleaseError):
                release.release(self.args, fake)
            self.assertNotIn("ticket stapling", fake.stages)
            self.assertFalse(self.args.output.exists())

    def test_corrupted_container_is_not_published_and_detaches(self):
        fake = AppleDouble(corrupt=True)
        with self.assertRaisesRegex(release.ReleaseError, "payload checksum"):
            release.release(self.args, fake)
        self.assertEqual(fake.stages[-1], "DMG detach")
        self.assertFalse(self.args.output.exists())

    def test_plan_performs_no_subprocess_or_acceptance(self):
        self.args.plan = True
        with patch("subprocess.run", side_effect=AssertionError("must not run")):
            release.release(self.args)
        self.assertFalse(self.args.output.exists())
        self.assertIn('"acceptance": "not evaluated"', sys.stdout.getvalue())

    def test_bad_metadata_rejected_before_tools(self):
        for field, value in (("version", "../0.3.1"), ("source_sha", "main"),
                             ("binary_sha256", "0" * 64), ("identity", "-"),
                             ("team_id", "bad"), ("notary_profile", "--password secret"),
                             ("notary_timeout", 0), ("command_timeout", 999999)):
            old = getattr(self.args, field)
            setattr(self.args, field, value)
            with self.subTest(field=field), self.assertRaises(release.ReleaseError):
                release.release(self.args, lambda *_: self.fail("unexpected tool invocation"))
            setattr(self.args, field, old)

    def test_archives_scripts_dylibs_wrong_cpu_rejected(self):
        for data in (b"#!/bin/sh\n", b"PK" + b"0" * 50, b"\x1f\x8b" + b"0" * 50,
                     struct.pack("<8I", 0xFEEDFACF, 0x0100000C, 0, 6, 0, 0, 0, 0),
                     struct.pack("<8I", 0xFEEDFACF, 0x01000007, 0, 2, 0, 0, 0, 0)):
            self.binary.write_bytes(data)
            self.args.binary_sha256 = release.digest(self.binary)
            with self.assertRaises(release.ReleaseError):
                release.validate(self.args)

    def test_symlinks_missing_configs_and_existing_output_rejected(self):
        path = self.config / release.CONFIG[0]
        path.unlink()
        path.symlink_to(self.binary)
        with self.assertRaises(release.ReleaseError):
            release.validate(self.args)
        path.unlink()
        with self.assertRaises(release.ReleaseError):
            release.validate(self.args)
        path.write_text("public: true")
        self.args.output.mkdir()
        with self.assertRaises(release.ReleaseError):
            release.validate(self.args)

    def test_wrong_team_or_missing_runtime_or_timestamp_fails(self):
        for metadata in (f"TeamIdentifier={TEAM}\n", "TeamIdentifier=WRONG\n",
                         f"TeamIdentifier={TEAM}\nAuthority=Developer ID Application: Test ({TEAM})\nTimestamp=x\n"):
            def run(stage, argv):
                return metadata if stage == "signature metadata" else ""
            with self.assertRaises(release.ReleaseError):
                release.signature(run, self.binary, TEAM, runtime=True)

    def test_identity_mismatch_stops_before_signing(self):
        for listing in ("0 valid identities found", f'{IDENTITY} "Developer ID Application: Test (XXXXXXXXXX)"',
                        f'{IDENTITY} "Apple Development: Test ({TEAM})"'):
            calls = []
            def run(stage, argv):
                calls.append(stage)
                return listing
            with self.assertRaises(release.ReleaseError):
                release.release(self.args, run)
            self.assertEqual(calls, ["identity lookup"])
            self.assertFalse(self.args.output.exists())


class NotaryBehavior(unittest.TestCase):
    def test_id_only_submit_queries_info_before_acceptance(self):
        for receipt in ({"id": SUBMISSION},
                        {"id": SUBMISSION, "message": "Successfully uploaded file.", "path": "safe.dmg"}):
            calls = []
            responses = iter([receipt, {"id": SUBMISSION, "status": "In Progress"},
                              {"id": SUBMISSION, "status": "Accepted"}])
            def run(stage, argv):
                calls.append(argv[2])
                return json.dumps(next(responses))
            self.assertEqual(release.notarize(run, Path("safe.dmg"), "alias", 60,
                                             sleep=lambda _: None), SUBMISSION)
            self.assertEqual(calls, ["submit", "info", "info"])

    def test_upload_receipt_alone_cannot_establish_acceptance(self):
        for status in ("Invalid", "Rejected", "unexpected", None):
            responses = iter([{"id": SUBMISSION, "status": "Accepted"},
                              {"id": SUBMISSION, "status": status}])
            with self.subTest(status=status), self.assertRaises(release.ReleaseError):
                release.notarize(lambda *_: json.dumps(next(responses)),
                                 Path("safe.dmg"), "alias", 60, sleep=lambda _: None)

    def test_malformed_info_after_valid_upload_fails(self):
        responses = iter([json.dumps({"id": SUBMISSION}), "{}"])
        with self.assertRaisesRegex(release.ReleaseError, "malformed"):
            release.notarize(lambda *_: next(responses), Path("safe.dmg"), "alias", 60)

    def test_poll_until_accepted(self):
        calls = []
        def run(stage, argv):
            calls.append(argv)
            return json.dumps({"id": SUBMISSION, "status": "In Progress" if len(calls) == 1 else "Accepted"})
        self.assertEqual(release.notarize(run, Path("safe.dmg"), "alias", 60, sleep=lambda _: None), SUBMISSION)
        self.assertEqual([c[2] for c in calls], ["submit", "info"])

    def test_malformed_responses_and_mismatched_id(self):
        for raw in ("not JSON", "{}", '{"id":"bad"}', '[]', '{"id":null}', '{"id":123}'):
            with self.assertRaises(release.ReleaseError):
                release.notarize(lambda *_: raw, Path("safe.dmg"), "alias", 1)
        responses = iter([json.dumps({"id": SUBMISSION, "status": "In Progress"}),
                          json.dumps({"id": "aaaaaaaa-1234-1234-1234-123456789abc", "status": "Accepted"})])
        with self.assertRaisesRegex(release.ReleaseError, "different submission"):
            release.notarize(lambda *_: next(responses), Path("safe.dmg"), "alias", 60, sleep=lambda _: None)

    def test_pending_deadline_stops(self):
        clock = iter((0, 0, 2))
        with self.assertRaisesRegex(release.ReleaseError, "deadline"):
            release.notarize(lambda *_: json.dumps({"id": SUBMISSION, "status": "In Progress"}),
                             Path("safe.dmg"), "alias", 1, clock=lambda: next(clock), sleep=lambda _: None)

    def test_runner_does_not_forward_ambient_api_credentials(self):
        keys = ("IDORIS_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GH_TOKEN", "UNRELATED_TEST_SECRET")
        with patch.dict(os.environ, {key: "fixture-only" for key in keys}):
            presence = release.Runner(5)("environment presence control", [
                sys.executable, "-c",
                "import json, os, sys; print(json.dumps({key: key in os.environ for key in sys.argv[1:]}))",
                *keys])
        self.assertEqual(json.loads(presence), {key: False for key in keys})

    def test_runner_preserves_required_nonsecret_context(self):
        context = {"HOME": "/fixture/operator-home", "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
                   "TMPDIR": tempfile.gettempdir(), "LANG": "en_US.UTF-8", "LC_ALL": "C",
                   "DEVELOPER_DIR": "/fixture/selected-xcode"}
        with patch.dict(os.environ, context, clear=True):
            observed = release.Runner(5)("environment context control", [
                sys.executable, "-c", "import json, os; print(json.dumps(dict(os.environ)))"])
        observed = json.loads(observed)
        self.assertEqual({key: observed.get(key) for key in context}, context)
        # macOS may add its own encoding field after the process starts.
        os_added = {"__CF_USER_TEXT_ENCODING"} if sys.platform == "darwin" else set()
        self.assertLessEqual(set(observed) - set(context), os_added)

    def test_runner_failure_and_timeout_hide_private_output(self):
        runner = release.Runner(1)
        with self.assertRaises(release.ReleaseError) as result:
            runner("test failure", [sys.executable, "-c", "import sys; print('private-value'); sys.exit(2)"])
        self.assertNotIn("private-value", str(result.exception))
        with self.assertRaisesRegex(release.ReleaseError, "timed out"):
            runner("test timeout", [sys.executable, "-c", "import time; time.sleep(10)"])


class DigestBehavior(unittest.TestCase):
    def test_empty_and_multi_chunk_files_match_sha256(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp).resolve() / "payload"
            for data in (b"", b"abc", bytes(range(256)) * 8193):
                path.write_bytes(data)
                self.assertEqual(release.digest(path), hashlib.sha256(data).hexdigest())


if __name__ == "__main__":
    unittest.main()
