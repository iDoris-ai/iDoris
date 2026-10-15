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
EXPECTED_STAGES = [
    "identity lookup", "CLI signing", "signature verification", "signature metadata",
    "DMG creation", "DMG signing", "signature verification", "signature metadata",
    "notary submit", "notary info", "ticket stapling", "ticket validation",
    "signature verification", "signature metadata", "DMG integrity",
    "DMG Gatekeeper assessment", "DMG mount", "signature verification",
    "signature metadata", "CLI notarization verification", "DMG detach",
]


class AppleDouble:
    def __init__(self, fail=None, status="Accepted", corrupt=False):
        self.stages = []
        self.calls = []
        self.fail, self.status, self.corrupt = fail, status, corrupt
        self.payload = None

    def __call__(self, stage, argv):
        self.stages.append(stage)
        self.calls.append((stage, argv))
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
        self.assertEqual(fake.stages, EXPECTED_STAGES)
        commands = dict(fake.calls)
        self.assertEqual(commands["CLI signing"][:-1], [
            "/usr/bin/codesign", "--force", "--sign", IDENTITY, "--timestamp",
            "--options", "runtime", "--identifier", "ai.idoris.cli"])
        self.assertEqual(commands["DMG signing"][:-1],
                         ["/usr/bin/codesign", "--sign", IDENTITY, "--timestamp"])
        self.assertEqual(commands["ticket validation"][:-1],
                         ["/usr/bin/xcrun", "stapler", "validate"])
        self.assertEqual(commands["DMG integrity"][:-1], ["/usr/bin/hdiutil", "verify"])
        self.assertEqual(commands["DMG Gatekeeper assessment"][:-1], [
            "/usr/sbin/spctl", "--assess", "--type", "open", "--context",
            "context:primary-signature", "--verbose=2"])
        self.assertEqual(commands["CLI notarization verification"][:-1], [
            "/usr/bin/codesign", "--verify", "--strict", "-R=notarized",
            "--check-notarization", "--verbose=4"])

    def test_every_failed_stage_stops_without_publishing(self):
        for stage in dict.fromkeys(EXPECTED_STAGES):
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
        valid = (f"TeamIdentifier={TEAM}\nAuthority=Developer ID Application: Test ({TEAM})\n"
                 "Timestamp=x\nCodeDirectory flags=0x10000(runtime)\n")
        for metadata in (valid.replace(f"TeamIdentifier={TEAM}", "TeamIdentifier=WRONG"),
                         valid.replace("Timestamp=x\n", ""),
                         valid.replace("CodeDirectory flags=0x10000(runtime)\n", "")):
            def run(stage, argv):
                return metadata if stage == "signature metadata" else ""
            with self.assertRaises(release.ReleaseError):
                release.signature(run, self.binary, TEAM, runtime=True)

    def test_extra_file_and_symlink_in_mounted_payload_fail(self):
        for kind in ("extra", "symlink"):
            fake = AppleDouble()
            def run(stage, argv):
                result = fake(stage, argv)
                if stage == "DMG mount":
                    mount = Path(argv[argv.index("-mountpoint") + 1])
                    if kind == "extra":
                        (mount / "unplanned.txt").write_text("extra")
                    else:
                        path = mount / "config/catalog.yaml"
                        path.unlink()
                        path.symlink_to(mount / "config/routing-policy.yaml")
                return result
            with self.subTest(kind=kind), self.assertRaisesRegex(
                    release.ReleaseError, "file set" if kind == "extra" else "symbolic links"):
                release.release(self.args, run)
            self.assertFalse(self.args.output.exists())
            self.assertEqual(fake.stages[-1], "DMG detach")

    def test_staging_binary_checksum_is_independent_of_source_validation(self):
        original_copy = shutil.copyfile
        def corrupt_copy(source, target, **kwargs):
            result = original_copy(source, target, **kwargs)
            if Path(target).name == "idoris":
                with Path(target).open("ab") as stream:
                    stream.write(b"changed during staging")
            return result
        fake = AppleDouble()
        with patch("macos_signed_release.shutil.copyfile", side_effect=corrupt_copy):
            with self.assertRaisesRegex(release.ReleaseError, "binary changed during staging"):
                release.release(self.args, fake)
        self.assertEqual(fake.stages, ["identity lookup"])
        self.assertFalse(self.args.output.exists())

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

    def test_later_poll_cannot_switch_submission_id(self):
        responses = iter([{"id": SUBMISSION}, {"id": SUBMISSION, "status": "In Progress"},
                          {"id": "aaaaaaaa-1234-1234-1234-123456789abc", "status": "Accepted"}])
        with self.assertRaisesRegex(release.ReleaseError, "different submission"):
            release.notarize(lambda *_: json.dumps(next(responses)), Path("safe.dmg"),
                             "alias", 60, sleep=lambda _: None)

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


class MountAndPublishBehavior(unittest.TestCase):
    def setUp(self):
        ReleaseBehavior.setUp(self)

    def test_attach_side_effect_then_timeout_detaches_without_publishing(self):
        fake = AppleDouble()

        def run(stage, argv):
            if stage == "DMG mount":
                fake(stage, argv)
                raise release.ReleaseError("DMG mount: timed out; no deliverable produced")
            return fake(stage, argv)

        with self.assertRaisesRegex(release.ReleaseError, r"^DMG mount: timed out"):
            release.release(self.args, run)
        self.assertEqual(fake.stages[-1], "DMG detach")
        self.assertEqual(fake.calls[-1][1][:2], ["/usr/bin/hdiutil", "detach"])
        self.assertFalse(self.args.output.exists())
        self.assertNotIn("Verified signed", sys.stdout.getvalue())

    def test_attach_failure_without_mount_side_effect_does_not_detach(self):
        fake = AppleDouble(fail="DMG mount")
        with self.assertRaisesRegex(release.ReleaseError, "injected tool failure"):
            release.release(self.args, fake)
        self.assertNotIn("DMG detach", fake.stages)
        self.assertFalse(self.args.output.exists())

    def test_preexisting_mount_path_is_not_detached(self):
        work = self.root / "work"
        work.mkdir()
        mount = work / "mounted"
        mount.mkdir()
        sentinel = mount / "SENTINEL"
        sentinel.write_text("unowned")
        calls = []

        def run(stage, argv):
            calls.append((stage, argv))
            return ""

        with self.assertRaisesRegex(release.ReleaseError, "preexisting mount"):
            release.verify_payload(run, work / "image.dmg", work, "a" * 64, {}, TEAM)
        self.assertEqual(calls, [])
        self.assertEqual(sentinel.read_text(), "unowned")
        self.assertEqual(sorted(p.name for p in mount.iterdir()), ["SENTINEL"])

    def test_detach_failure_preserves_verification_error(self):
        fake = AppleDouble(corrupt=True)

        def run(stage, argv):
            result = fake(stage, argv)
            if stage == "DMG detach":
                raise release.ReleaseError("DMG detach: tool failed (exit 9); inspect locally with Apple tools")
            return result

        with self.assertRaises(release.ReleaseError) as caught:
            release.release(self.args, run)
        message = str(caught.exception)
        self.assertIn("DMG payload checksum mismatch", message)
        self.assertIn("DMG detach: tool failed (exit 9)", message)
        self.assertLess(message.index("checksum"), message.index("DMG detach"))
        self.assertFalse(self.args.output.exists())
        self.assertNotIn("Verified signed", sys.stdout.getvalue())

    def test_detach_failure_preserves_attach_timeout(self):
        fake = AppleDouble()

        def run(stage, argv):
            if stage == "DMG mount":
                fake(stage, argv)
                raise release.ReleaseError("DMG mount: timed out; no deliverable produced")
            if stage == "DMG detach":
                fake(stage, argv)
                raise release.ReleaseError("DMG detach: tool failed (exit 9); inspect locally with Apple tools")
            return fake(stage, argv)

        with self.assertRaises(release.ReleaseError) as caught:
            release.release(self.args, run)
        message = str(caught.exception)
        self.assertTrue(message.startswith("DMG mount: timed out; no deliverable produced"))
        self.assertIn("DMG detach: tool failed (exit 9)", message)
        self.assertEqual(fake.stages[-1], "DMG detach")
        self.assertFalse(self.args.output.exists())

    def test_detach_only_failure_is_fatal(self):
        fake = AppleDouble()

        def run(stage, argv):
            result = fake(stage, argv)
            if stage == "DMG detach":
                raise release.ReleaseError("DMG detach: tool failed (exit 9); inspect locally with Apple tools")
            return result

        with self.assertRaisesRegex(release.ReleaseError, r"^DMG detach: tool failed \(exit 9\)"):
            release.release(self.args, run)
        self.assertFalse(self.args.output.exists())
        self.assertNotIn("Verified signed", sys.stdout.getvalue())

    def test_directory_cleanup_failure_preserves_mount_timeout(self):
        fake = AppleDouble()

        def run(stage, argv):
            if stage == "DMG mount":
                fake(stage, argv)
                raise release.ReleaseError("DMG mount: timed out; no deliverable produced")
            return fake(stage, argv)

        original = tempfile.TemporaryDirectory.cleanup

        def cleanup(directory):
            if os.path.basename(directory.name).startswith("idoris-sign-"):
                # Real cleanup detaches its finalizer before rmtree; a failed rmtree
                # must not leave that finalizer to warn with the local path.
                directory._finalizer.detach()
                raise OSError(30, "Read-only file system")
            return original(directory)

        with patch.object(tempfile.TemporaryDirectory, "cleanup", cleanup):
            with self.assertRaises(release.ReleaseError) as caught:
                release.release(self.args, run)
        message = str(caught.exception)
        self.assertTrue(message.startswith("DMG mount: timed out; no deliverable produced"))
        self.assertIn("filesystem cleanup failed", message)
        self.assertNotIn("Read-only file system", message)
        self.assertFalse(self.args.output.exists())
        self.assertNotIn("Verified signed", sys.stdout.getvalue())

    def test_directory_cleanup_failure_without_primary_is_fatal(self):
        original = tempfile.TemporaryDirectory.cleanup

        def cleanup(directory):
            if os.path.basename(directory.name).startswith("idoris-sign-"):
                directory._finalizer.detach()
                raise OSError(30, "Read-only file system", directory.name)
            return original(directory)

        with patch.object(tempfile.TemporaryDirectory, "cleanup", cleanup):
            with self.assertRaisesRegex(release.ReleaseError, "filesystem cleanup failed"):
                release.release(self.args, AppleDouble())
        self.assertNotIn(str(self.root), sys.stderr.getvalue())
        self.assertNotIn("Verified signed", sys.stdout.getvalue())

    def test_existing_output_sentinel_is_untouched(self):
        self.args.output.mkdir()
        sentinel = self.args.output / "SENTINEL"
        sentinel.write_text("other-writer")
        marker = sentinel.stat().st_ino
        with self.assertRaises(release.ReleaseError):
            release.release(self.args, lambda *_: self.fail("unexpected tool invocation"))
        self.assertEqual(sentinel.read_text(), "other-writer")
        self.assertEqual(sentinel.stat().st_ino, marker)
        self.assertEqual([path.name for path in self.args.output.iterdir()], ["SENTINEL"])

    def test_empty_output_collision_is_not_overwritten(self):
        fake = AppleDouble()
        armed = {"on": False}
        collided = {}
        out = self.args.output
        real_exists, real_mkdir, real_rename = Path.exists, Path.mkdir, Path.rename

        def exists(path):
            if armed["on"] and path == out and not real_exists(path):
                os.mkdir(path)
                collided["ino"] = path.stat().st_ino
                return False
            return real_exists(path)

        def mkdir(path, mode=0o777, parents=False, exist_ok=False):
            if armed["on"] and path == out and not real_exists(path):
                os.mkdir(path, mode)
                collided["ino"] = path.stat().st_ino
            return real_mkdir(path, mode=mode, parents=parents, exist_ok=exist_ok)

        def rename(path, target):
            if armed["on"] and target == out and not real_exists(target):
                os.mkdir(target)
                collided["ino"] = target.stat().st_ino
            return real_rename(path, target)

        def run(stage, argv):
            result = fake(stage, argv)
            if stage == "DMG detach":
                armed["on"] = True
            return result

        with patch.object(Path, "exists", exists), patch.object(Path, "mkdir", mkdir), \
                patch.object(Path, "rename", rename):
            with self.assertRaises(release.ReleaseError):
                release.release(self.args, run)
        self.assertEqual(out.stat().st_ino, collided["ino"])
        self.assertEqual(list(out.iterdir()), [])
        self.assertNotIn("Verified signed", sys.stdout.getvalue())

    def test_partial_publication_keeps_foreign_sentinel_only(self):
        out = self.args.output
        real_mkdir = Path.mkdir

        def mkdir(path, mode=0o777, parents=False, exist_ok=False):
            created = not path.exists()
            result = real_mkdir(path, mode=mode, parents=parents, exist_ok=exist_ok)
            if created and path == out:
                (path / "OTHER_WRITER").write_text("keep")
            return result

        with patch.object(Path, "mkdir", mkdir):
            with self.assertRaisesRegex(release.ReleaseError, "output changed during publication"):
                release.release(self.args, AppleDouble())
        self.assertEqual([path.name for path in out.iterdir()], ["OTHER_WRITER"])
        self.assertEqual((out / "OTHER_WRITER").read_text(), "keep")
        self.assertNotIn("Verified signed", sys.stdout.getvalue())


class DigestBehavior(unittest.TestCase):
    def test_empty_and_multi_chunk_files_match_sha256(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp).resolve() / "payload"
            for data in (b"", b"abc", bytes(range(256)) * 8193):
                path.write_bytes(data)
                self.assertEqual(release.digest(path), hashlib.sha256(data).hexdigest())


if __name__ == "__main__":
    unittest.main()
