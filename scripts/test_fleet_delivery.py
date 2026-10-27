import hashlib
import io
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

from fleet_delivery import (
    DeliveryError,
    attempt_host,
    fetch_archive,
    host_result,
    parse_host_status,
    parse_checksum,
    parse_remote,
    public_summary,
    run_hosts,
    select_target,
)

PLATFORM = "x86_64-unknown-linux-gnu"


def release(tag, draft=False, prerelease=False):
    return {"tag_name": tag, "draft": draft, "prerelease": prerelease}


def make_archive(root: Path, version: str, binary_version: str | None) -> Path:
    package = f"harklight-agent-{version}-{PLATFORM}"
    if binary_version is None:
        binary = b"#!/bin/sh\nexit 126\n"
    else:
        binary = f"#!/bin/sh\nprintf '%s\\n' 'harklight-agent {binary_version}'\n".encode()
    buffer = io.BytesIO()
    member = tarfile.TarInfo(f"{package}/harklight-agent")
    member.mode = 0o755
    member.size = len(binary)
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        archive.addfile(member, io.BytesIO(binary))
    path = root / "release.tar.gz"
    path.write_bytes(buffer.getvalue())
    return path


def run_remote(home: Path, archive: Path, version: str, digest: str):
    remote_archive = home.parent / f"transfer-{version}-{os.urandom(4).hex()}.tar.gz"
    shutil.copyfile(archive, remote_archive)
    env = os.environ.copy()
    env["HOME"] = str(home)
    return subprocess.run(
        [
            "sh",
            str(Path(__file__).with_name("install_verified_release.sh")),
            version,
            PLATFORM,
            digest,
            str(remote_archive),
        ],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )


class FleetDeliveryTests(unittest.TestCase):
    def test_active_host_status_parser_captures_version_path_and_hash(self):
        digest = "a" * 64
        status = parse_host_status(
            b"login notice\nFLEETSTATUS1\0harklight-agent 0.1.3\0/home/user/.local/share/harklight-agent/releases/v0.1.3/harklight-agent\0"
            + digest.encode()
            + b"\0"
        )
        self.assertEqual(status["version_output"], "harklight-agent 0.1.3")
        self.assertEqual(status["resolved_path"], "/home/user/.local/share/harklight-agent/releases/v0.1.3/harklight-agent")
        self.assertEqual(status["binary_sha256"], digest)

    def test_archive_failure_retains_the_active_binary_readback(self):
        host = {"destination": "configured-route", "target": PLATFORM}
        status = {
            "version_output": "harklight-agent 0.1.3",
            "resolved_path": "/private/releases/v0.1.3/harklight-agent",
            "binary_sha256": "a" * 64,
            "observed_at": "2026-10-27T10:00:00+00:00",
        }
        with patch("fleet_delivery.read_host_status", return_value=status), patch(
            "fleet_delivery.fetch_archive", side_effect=DeliveryError("release archive checksum mismatch")
        ):
            result = attempt_host(host, release("v0.1.4"), [], Path("/private/config.json"), None)
        self.assertEqual(result["outcome"], "failed")
        self.assertEqual(result["reason"], "release archive checksum mismatch")
        self.assertEqual(result["version_output"], "harklight-agent 0.1.3")
        self.assertEqual(result["resolved_path"], status["resolved_path"])
        self.assertEqual(result["binary_sha256"], status["binary_sha256"])
        self.assertEqual(result["prior_release_kept"]["version"], "0.1.3")

    def test_target_is_newest_published_stable_and_not_withdrawn(self):
        releases = [
            release("v0.1.3"),
            release("v0.1.4-rc.1", prerelease=True),
            release("v0.1.5", draft=True),
            release("v0.1.9"),
            release("v0.1.12"),
        ]
        self.assertEqual(select_target(releases, ["0.1.12"])["tag_name"], "v0.1.9")
        self.assertEqual(select_target(releases, ["v0.1.9", "0.1.12"])["tag_name"], "v0.1.3")
        self.assertIsNone(select_target(releases, ["0.1.3", "0.1.9", "0.1.12"]))

    def test_checksum_requires_one_exact_filename(self):
        digest = "a" * 64
        checksums = f"{digest} other.tar.gz\n{digest} *release.tar.gz\n"
        self.assertEqual(parse_checksum(checksums, "release.tar.gz"), digest)
        with self.assertRaises(DeliveryError):
            parse_checksum(checksums, "missing.tar.gz")
        with self.assertRaises(DeliveryError):
            parse_checksum(f"{digest} release.tar.gz\n{digest} release.tar.gz\n", "release.tar.gz")
        with self.assertRaises(DeliveryError):
            parse_checksum(f"{'g' * 64} release.tar.gz\n", "release.tar.gz")

    def test_archive_bytes_must_match_the_published_sum(self):
        archive = b"verified archive bytes"
        filename = f"harklight-agent-0.1.4-{PLATFORM}.tar.gz"
        digest = hashlib.sha256(archive).hexdigest()
        release_metadata = release("v0.1.4")
        release_metadata["assets"] = [
            {
                "name": "SHA256SUMS",
                "browser_download_url": "https://github.com/clickety-clacks/harklight-agent/releases/download/v0.1.4/SHA256SUMS",
            },
            {
                "name": filename,
                "browser_download_url": f"https://github.com/clickety-clacks/harklight-agent/releases/download/v0.1.4/{filename}",
            },
        ]

        def matching_download(url, _token):
            return f"{digest}  {filename}\n".encode() if url.endswith("SHA256SUMS") else archive

        with patch("fleet_delivery.request_bytes", side_effect=matching_download):
            verified = fetch_archive(release_metadata, PLATFORM, "read-only-token")
        self.assertEqual(verified["bytes"], archive)
        self.assertEqual(verified["sha256"], digest)

        with patch("fleet_delivery.request_bytes", side_effect=lambda url, _token: (
            f"{digest}  {filename}\n".encode() if url.endswith("SHA256SUMS") else archive + b"x"
        )):
            with self.assertRaises(DeliveryError):
                fetch_archive(release_metadata, PLATFORM, "read-only-token")

    def test_remote_protocol_ignores_prefix_output_and_summary_omits_destinations(self):
        output = b"login notice\nFLEET1\0updated\0\0harklight-agent 0.1.4\0/private/path\0" + b"a" * 64 + b"\0" + b"0.1.3\0/private/old\0" + b"b" * 64 + b"\0"
        result = parse_remote(output)
        self.assertEqual(result["outcome"], "updated")
        summary = public_summary({
            "status": "reconciled", "target_version": "0.1.4", "done": False,
            "counts": {"hosts": 1, "updated": 0, "already_current": 0, "unreachable": 1, "failed": 0},
            "hosts": [host_result({"destination": "private-route", "target": PLATFORM}, "0.1.4", [], "unreachable")],
        })
        self.assertNotIn("private-route", summary)

    def test_one_failed_destination_does_not_stop_other_attempts(self):
        hosts = [
            {"destination": "offline-route", "target": PLATFORM},
            {"destination": "working-route", "target": PLATFORM},
        ]
        target = release("v0.1.4")

        def attempt(host, *_args):
            if host["destination"] == "offline-route":
                raise OSError("connection timed out")
            return host_result(host, "0.1.4", [], "updated")

        with patch("fleet_delivery.attempt_host", side_effect=attempt):
            results = run_hosts(hosts, target, [], Path("/private/config.json"), None)
        self.assertEqual([row["outcome"] for row in results], ["failed", "updated"])
        self.assertEqual(results[0]["destination"], "offline-route")

    def test_verified_update_switches_only_after_version_check_and_keeps_prior(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            home = root / "home"
            prior = home / ".local/share/harklight-agent/releases/v0.1.3/harklight-agent"
            prior.parent.mkdir(parents=True)
            prior.write_text("#!/bin/sh\nprintf '%s\\n' 'harklight-agent 0.1.3'\n")
            prior.chmod(0o755)
            prior_hash = hashlib.sha256(prior.read_bytes()).hexdigest()
            link = home / ".local/bin/harklight-agent"
            link.parent.mkdir(parents=True)
            link.symlink_to(prior)
            archive = make_archive(root, "0.1.4", "0.1.4")
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()

            result = run_remote(home, archive, "0.1.4", digest)

            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual(result.stdout.split(b"\0")[1], b"updated")
            self.assertEqual(
                link.resolve(),
                (home / ".local/share/harklight-agent/releases/v0.1.4/harklight-agent").resolve(),
            )
            self.assertEqual(hashlib.sha256(prior.read_bytes()).hexdigest(), prior_hash)

    def test_checksum_or_unusable_candidate_leaves_active_link_unchanged(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            home = root / "home"
            prior = home / ".local/share/harklight-agent/releases/v0.1.3/harklight-agent"
            prior.parent.mkdir(parents=True)
            prior.write_text("#!/bin/sh\nprintf '%s\\n' 'harklight-agent 0.1.3'\n")
            prior.chmod(0o755)
            link = home / ".local/bin/harklight-agent"
            link.parent.mkdir(parents=True)
            link.symlink_to(prior)
            archive = make_archive(root, "0.1.4", "0.1.4")
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()

            checksum_failure = run_remote(home, archive, "0.1.4", "0" * 64)
            self.assertNotEqual(checksum_failure.returncode, 0)
            self.assertEqual(checksum_failure.stdout.split(b"\0")[1], b"failed")
            self.assertEqual(link.resolve(), prior.resolve())

            wrong_archive = make_archive(root, "0.1.4", "0.1.9")
            wrong_digest = hashlib.sha256(wrong_archive.read_bytes()).hexdigest()
            version_failure = run_remote(home, wrong_archive, "0.1.4", wrong_digest)
            self.assertNotEqual(version_failure.returncode, 0)
            self.assertEqual(version_failure.stdout.split(b"\0")[1], b"failed")
            self.assertEqual(link.resolve(), prior.resolve())

            unusable = make_archive(root, "0.1.4", None)
            unusable_digest = hashlib.sha256(unusable.read_bytes()).hexdigest()
            run_failure = run_remote(home, unusable, "0.1.4", unusable_digest)
            self.assertNotEqual(run_failure.returncode, 0)
            self.assertEqual(run_failure.stdout.split(b"\0")[1], b"failed")
            self.assertEqual(link.resolve(), prior)

    def test_withdrawn_target_reactivates_retained_release_without_mutating_newer_one(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            home = root / "home"
            newer = home / ".local/share/harklight-agent/releases/v0.1.4/harklight-agent"
            prior = home / ".local/share/harklight-agent/releases/v0.1.3/harklight-agent"
            newer.parent.mkdir(parents=True)
            prior.parent.mkdir(parents=True)
            newer.write_text("#!/bin/sh\nprintf '%s\\n' 'harklight-agent 0.1.4'\n")
            prior.write_text("#!/bin/sh\nprintf '%s\\n' 'harklight-agent 0.1.3'\n")
            newer.chmod(0o755)
            prior.chmod(0o755)
            newer_hash = hashlib.sha256(newer.read_bytes()).hexdigest()
            prior_hash = hashlib.sha256(prior.read_bytes()).hexdigest()
            link = home / ".local/bin/harklight-agent"
            link.parent.mkdir(parents=True)
            link.symlink_to(newer)
            archive = make_archive(root, "0.1.3", "0.1.3")
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()

            result = run_remote(home, archive, "0.1.3", digest)

            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual(result.stdout.split(b"\0")[1], b"updated")
            self.assertEqual(link.resolve(), prior)
            self.assertEqual(hashlib.sha256(newer.read_bytes()).hexdigest(), newer_hash)
            self.assertEqual(hashlib.sha256(prior.read_bytes()).hexdigest(), prior_hash)


if __name__ == "__main__":
    unittest.main()
