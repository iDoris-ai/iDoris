#!/usr/bin/env python3
"""Package an already-built CLI as a signed, notarized, stapled UDIF DMG.

No builds, credential creation, installation, or publication. See the operator doc.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import uuid

CONFIG = ("components/omlx.yaml", "components/subscription.yaml",
          "routing-policy.yaml", "catalog.yaml")


class ReleaseError(RuntimeError):
    pass


def require(condition, message):
    if not condition:
        raise ReleaseError(message)


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def regular(path):
    path = Path(os.path.abspath(path))
    require(not any(p.is_symlink() for p in (path, *path.parents)),
            "input/output paths must not traverse symbolic links")
    require(path.is_file(), "input must be a regular file")
    return path


def validate(args):
    require(re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.]+)?", args.version),
            "version must be X.Y.Z[-prerelease]")
    require(re.fullmatch(r"[0-9a-f]{40}", args.source_sha), "source SHA must be a full commit hash")
    require(re.fullmatch(r"[0-9a-f]{64}", args.binary_sha256), "binary SHA256 is required")
    require(re.fullmatch(r"[0-9A-Fa-f]{40}", args.identity), "identity must be an exact certificate SHA1")
    require(re.fullmatch(r"[A-Z0-9]{10}", args.team_id), "team ID must have ten uppercase characters")
    require(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", args.notary_profile),
            "notary profile must be an existing alias, not credentials")
    require(1 <= args.command_timeout <= 1800 and 1 <= args.notary_timeout <= 14400,
            "timeouts outside supported bounds")
    binary = regular(args.binary)
    require(digest(binary) == args.binary_sha256, "input binary checksum mismatch")
    # Thin 64-bit executable, not a script/archive/dylib or an unexamined fat slice.
    with binary.open("rb") as stream:
        header = stream.read(32)
    require(len(header) == 32 and header[:4] == b"\xcf\xfa\xed\xfe", "expected thin 64-bit Mach-O")
    _, cpu, _, kind = struct.unpack("<4I", header[:16])
    expected_cpu = {"aarch64": 0x0100000C, "x86_64": 0x01000007}[args.arch]
    require(cpu == expected_cpu and kind == 2, "Mach-O architecture/type mismatch")
    configs = [regular(args.config_root / name) for name in CONFIG]
    out = Path(os.path.abspath(args.output))
    require(not out.exists() and not out.is_symlink(), "output must be a new directory")
    require(out.parent.is_dir() and not any(p.is_symlink() for p in (out.parent, *out.parents)),
            "output parent must exist without symbolic links")
    return binary, configs, out


class Runner:
    def __init__(self, timeout):
        self.timeout = timeout

    def __call__(self, stage, argv):
        # Never print argv, local paths, aliases, environment, or raw tool output.
        # File-backed capture bounds memory even if a tool is unexpectedly noisy.
        # Preserve local keychain, temporary-directory and selected Xcode context.
        # Signing tools do not need ambient API tokens or unrelated credentials.
        env = {key: os.environ[key] for key in
               ("HOME", "PATH", "TMPDIR", "LANG", "DEVELOPER_DIR") if key in os.environ}
        env["LC_ALL"] = "C"
        with tempfile.TemporaryFile() as output:
            try:
                result = subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=output,
                                        stderr=subprocess.STDOUT, timeout=self.timeout,
                                        check=False, env=env)
            except subprocess.TimeoutExpired:
                raise ReleaseError(f"{stage}: timed out; no deliverable produced") from None
            except OSError:
                raise ReleaseError(f"{stage}: tool unavailable; no deliverable produced") from None
            require(result.returncode == 0, f"{stage}: tool failed (exit {result.returncode}); inspect locally with Apple tools")
            output.seek(0)
            text = output.read(1024 * 1024 + 1)
            require(len(text) <= 1024 * 1024, f"{stage}: excessive tool output")
            return text.decode("utf-8", errors="replace")


def notarize(run, image, profile, timeout, clock=time.monotonic, sleep=time.sleep):
    auth = ["--keychain-profile", profile, "--output-format", "json"]
    def request(verb, value):
        raw = run(f"notary {verb}", ["/usr/bin/xcrun", "notarytool", verb, str(value), *auth])
        try:
            data = json.loads(raw)
            require(isinstance(data, dict) and isinstance(data.get("id"), str),
                    "notary service returned malformed response")
            submission = str(uuid.UUID(data["id"]))
        except (ValueError, KeyError, TypeError):
            raise ReleaseError("notary service returned malformed response") from None
        return data, submission
    data, submission = request("submit", image)
    print(f"Notary submission: {submission}", file=sys.stderr)
    # An asynchronous upload receipt guarantees an ID, not a processing status.
    # If a status is present, still reject unexpected values and confirm via info.
    if "status" in data:
        require(data["status"] in ("In Progress", "Accepted"),
                f"notary {submission}: rejected or unknown status; retrieve log locally")
    end = clock() + timeout
    data, returned = request("info", submission)
    require(returned == submission, "notary service returned a different submission ID")
    while True:
        status = data.get("status")
        if status == "Accepted":
            return submission
        require(status == "In Progress", f"notary {submission}: rejected or unknown status; retrieve log locally")
        remaining = end - clock()
        require(remaining > 0, f"notary {submission}: acceptance deadline exceeded; remote processing may continue")
        sleep(min(15, remaining))
        require(clock() < end, f"notary {submission}: acceptance deadline exceeded; remote processing may continue")
        data, returned = request("info", submission)
        require(returned == submission, "notary service returned a different submission ID")


def signature(run, path, team, runtime=False):
    run("signature verification", ["/usr/bin/codesign", "--verify", "--strict", "--verbose=2", str(path)])
    info = run("signature metadata", ["/usr/bin/codesign", "--display", "--verbose=4", str(path)])
    require(f"TeamIdentifier={team}" in info.splitlines(), "signature team mismatch")
    require(any(re.fullmatch(r"Authority=Developer ID Application: .+ \(" + team + r"\)", line)
                for line in info.splitlines()), "signature lacks Developer ID Application authority")
    require(any(line.startswith("Timestamp=") for line in info.splitlines()), "signature lacks secure timestamp")
    if runtime:
        require(re.search(r"flags=.*\bruntime\b", info), "CLI lacks hardened runtime")


def _entries(path):
    try:
        return tuple(sorted(os.listdir(path)))
    except OSError:
        return None


def _cleanup_text(error):
    return str(error) if isinstance(error, ReleaseError) else "filesystem cleanup failed"


def _raise_preserved(primary, secondary):
    # Keep the original failure object when cleanup also fails. A cleanup-only
    # failure stays fatal. OSError text can contain local paths, so it is not copied.
    if isinstance(primary, (KeyboardInterrupt, SystemExit)):
        raise primary
    if secondary is None:
        if primary is not None:
            raise primary
        return
    note = _cleanup_text(secondary)
    if primary is None:
        if isinstance(secondary, ReleaseError):
            raise secondary
        raise ReleaseError(note) from None
    if isinstance(primary, ReleaseError):
        message = str(primary)
        if note not in message:
            primary.args = (f"{message}; cleanup also failed: {note}",)
        raise primary
    raise ReleaseError(f"filesystem operation failed; cleanup also failed: {note}") from None


def _copy_exclusive(source, dest, owned):
    try:
        descriptor = os.open(str(dest), os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
    except FileExistsError:
        raise ReleaseError("output must be a new directory") from None
    owned.append(dest)
    try:
        with os.fdopen(descriptor, "wb") as stream, source.open("rb") as src:
            shutil.copyfileobj(src, stream)
            stream.flush()
            os.fsync(stream.fileno())
    except BaseException:
        try:
            os.unlink(dest)
        except OSError:
            pass
        raise


class _SignWork(tempfile.TemporaryDirectory):
    def __exit__(self, exc_type, exc, tb):
        cleanup_error = None
        try:
            self.cleanup()
        except OSError as error:
            cleanup_error = error
        if cleanup_error is not None and not isinstance(exc, (KeyboardInterrupt, SystemExit)):
            _raise_preserved(exc, cleanup_error)
        return False


def _discard_owned(directory, owned, reserved):
    error = None
    for path in owned:
        try:
            path.unlink()
        except FileNotFoundError:
            pass
        except OSError as failure:
            error = failure
    if reserved:
        try:
            if directory.is_dir() and not any(directory.iterdir()):
                directory.rmdir()
        except OSError as failure:
            error = error or failure
    return error


def verify_payload(run, image, work, cli_hash, config_hashes, team):
    mount = work / "mounted"
    # Only the mountpoint created below is ours. Never detach a preexisting path.
    if mount.is_symlink() or mount.exists() or os.path.ismount(mount):
        raise ReleaseError("refusing preexisting mount path")
    mount.mkdir()
    before = _entries(mount)
    primary = None
    detach = False
    try:
        try:
            run("DMG mount", ["/usr/bin/hdiutil", "attach", "-readonly", "-nobrowse",
                              "-noautoopen", "-mountpoint", str(mount), str(image)])
        except BaseException as error:
            primary = error
        else:
            detach = True
        # attach can mount and then time out before it returns.
        if not detach and (os.path.ismount(mount) or _entries(mount) != before):
            detach = True
        if primary is None:
            try:
                files = {str(p.relative_to(mount)) for p in mount.rglob("*") if p.is_file()}
                require(not any(p.is_symlink() for p in mount.rglob("*")), "DMG contains symbolic links")
                # hdiutil may add filesystem bookkeeping, but no extra payload files.
                files = {p for p in files if not p.startswith(".fseventsd/") and p != ".DS_Store"}
                expected = {"idoris": cli_hash, **config_hashes}
                require(files == set(expected), "DMG payload file set mismatch")
                for rel, checksum in expected.items():
                    require(digest(mount / rel) == checksum, "DMG payload checksum mismatch")
                signature(run, mount / "idoris", team, runtime=True)
                # Apple's "other code" check; spctl execute assesses app bundles, not CLIs.
                run("CLI notarization verification", ["/usr/bin/codesign", "--verify", "--strict",
                                                     "-R=notarized", "--check-notarization",
                                                     "--verbose=4", str(mount / "idoris")])
            except BaseException as error:
                primary = error
    finally:
        cleanup = None
        if detach:
            try:
                run("DMG detach", ["/usr/bin/hdiutil", "detach", str(mount)])
            except BaseException as error:
                cleanup = error
    _raise_preserved(primary, cleanup)


def release(args, run=None):
    binary, configs, out = validate(args)
    name = f"idoris-v{args.version}-{args.arch}-apple-darwin.dmg"
    if args.plan:
        print(json.dumps({"mode": "plan", "artifact": name, "source_sha": args.source_sha,
                          "signing_performed": False, "acceptance": "not evaluated"}))
        return
    require(platform.system() == "Darwin", "signing requires macOS")
    run = run or Runner(args.command_timeout)
    identities = run("identity lookup", ["/usr/bin/security", "find-identity", "-v", "-p", "codesigning"])
    require(re.search(r'\b' + args.identity.upper() + r'\s+"Developer ID Application: [^"\n]+ \('
                      + args.team_id + r'\)"', identities, re.IGNORECASE),
            "exact valid Developer ID Application identity/team not found")
    # Publish only after verification. mkdir(exist_ok=False) reserves the final
    # directory without replacing an empty directory the way rename() can.
    with _SignWork(prefix="idoris-sign-", dir=out.parent) as temp:
        work = Path(temp)
        payload, artifacts = work / "payload", work / "artifacts"
        payload.mkdir()
        artifacts.mkdir()
        cli = payload / "idoris"
        shutil.copyfile(binary, cli)
        cli.chmod(0o755)
        require(digest(cli) == args.binary_sha256, "binary changed during staging")
        config_hashes = {}
        for rel, source in zip(CONFIG, configs):
            target = payload / "config" / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            target.chmod(0o644)
            config_hashes[f"config/{rel}"] = digest(target)
        run("CLI signing", ["/usr/bin/codesign", "--force", "--sign", args.identity,
                            "--timestamp", "--options", "runtime", "--identifier", "ai.idoris.cli", str(cli)])
        signature(run, cli, args.team_id, runtime=True)
        image = artifacts / name
        run("DMG creation", ["/usr/bin/hdiutil", "create", "-format", "UDZO", "-volname",
                             f"iDoris {args.version}", "-srcfolder", str(payload), str(image)])
        run("DMG signing", ["/usr/bin/codesign", "--sign", args.identity, "--timestamp", str(image)])
        signature(run, image, args.team_id)
        submission = notarize(run, image, args.notary_profile, args.notary_timeout)
        run("ticket stapling", ["/usr/bin/xcrun", "stapler", "staple", str(image)])
        run("ticket validation", ["/usr/bin/xcrun", "stapler", "validate", str(image)])
        signature(run, image, args.team_id)
        run("DMG integrity", ["/usr/bin/hdiutil", "verify", str(image)])
        run("DMG Gatekeeper assessment", ["/usr/sbin/spctl", "--assess", "--type", "open",
                                         "--context", "context:primary-signature", "--verbose=2", str(image)])
        verify_payload(run, image, work, digest(cli), config_hashes, args.team_id)
        final_hash = digest(image)
        manifest = {"schema": 1, "version": args.version, "source_sha": args.source_sha,
                    "target": f"{args.arch}-apple-darwin", "team_id": args.team_id,
                    "input_binary_sha256": args.binary_sha256, "signed_binary_sha256": digest(cli),
                    "config_sha256": config_hashes, "artifact": name, "sha256": final_hash,
                    "notary_submission_id": submission, "notary_status": "Accepted",
                    "checks": ["codesign-strict", "stapler-validate", "hdiutil-verify", "spctl-open", "codesign-check-notarization", "mounted-payload-sha256"]}
        (artifacts / f"{name}.sha256").write_text(f"{final_hash}  {name}\n")
        (artifacts / f"{name}.manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        published = (name, f"{name}.sha256", f"{name}.manifest.json")
        owned = []
        reserved = False
        try:
            try:
                out.mkdir(exist_ok=False)
            except FileExistsError:
                raise ReleaseError("output must be a new directory") from None
            reserved = True
            for artifact_name in published:
                _copy_exclusive(artifacts / artifact_name, out / artifact_name, owned)
            require({item.name for item in out.iterdir()} == set(published),
                    "output changed during publication")
            require(digest(out / name) == final_hash, "published artifact checksum mismatch")
            try:
                published_manifest = json.loads((out / f"{name}.manifest.json").read_text())
            except ValueError:
                raise ReleaseError("published manifest does not match artifact") from None
            require(isinstance(published_manifest, dict) and published_manifest.get("sha256") == final_hash,
                    "published manifest does not match artifact")
        except BaseException as error:
            _raise_preserved(error, _discard_owned(out, owned, reserved))
    print(f"Verified signed artifacts: {name}")


def parser():
    result = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "config-root", "output"):
        result.add_argument(f"--{name}", required=True, type=Path)
    for name in ("version", "source-sha", "binary-sha256", "identity", "team-id", "notary-profile"):
        result.add_argument(f"--{name}", required=True)
    result.add_argument("--arch", choices=("aarch64", "x86_64"), default="aarch64")
    result.add_argument("--command-timeout", type=int, default=300)
    result.add_argument("--notary-timeout", type=int, default=3600)
    result.add_argument("--plan", action="store_true", help="validate inputs only; no Apple tools or signing")
    return result


if __name__ == "__main__":
    try:
        release(parser().parse_args())
    except (ReleaseError, OSError) as error:
        # OSError can contain private paths; don't expose its string.
        print(str(error) if isinstance(error, ReleaseError) else "filesystem operation failed", file=sys.stderr)
        sys.exit(1)
