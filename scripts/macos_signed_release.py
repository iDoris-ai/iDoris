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
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


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
        with tempfile.TemporaryFile() as output:
            try:
                result = subprocess.run(argv, stdin=subprocess.DEVNULL, stdout=output,
                                        stderr=subprocess.STDOUT, timeout=self.timeout,
                                        check=False, env={**os.environ, "LC_ALL": "C"})
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
    end = clock() + timeout
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


def verify_payload(run, image, work, cli_hash, config_hashes, team):
    mount = work / "mounted"
    mount.mkdir()
    attached = False
    try:
        run("DMG mount", ["/usr/bin/hdiutil", "attach", "-readonly", "-nobrowse",
                          "-noautoopen", "-mountpoint", str(mount), str(image)])
        attached = True
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
    finally:
        if attached:
            run("DMG detach", ["/usr/bin/hdiutil", "detach", str(mount)])


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
    # New directory published atomically only after all verification succeeds.
    with tempfile.TemporaryDirectory(prefix="idoris-sign-", dir=out.parent) as temp:
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
        require(not out.exists(), "output appeared during operation; refusing replacement")
        artifacts.rename(out)
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
