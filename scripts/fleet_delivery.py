#!/usr/bin/env python3
"""Reconcile private SSH destinations to the newest published stable release."""

from __future__ import annotations

import concurrent.futures
import datetime as dt
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.parse
import urllib.request
import uuid

REPOSITORY = "clickety-clacks/harklight-agent"
RELEASES_API = f"https://api.github.com/repos/{REPOSITORY}/releases"
TARGETS = {
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
}
INSTALLER = Path(__file__).with_name("install_verified_release.sh")
SSH_OPTIONS = ("-o", "BatchMode=yes", "-o", "ConnectTimeout=10")
MAX_ROUNDS = 8
STATUS_SCRIPT = b"""#!/bin/sh
set -u
active_link="$HOME/.local/bin/harklight-agent"
resolved=""
if [ -L "$active_link" ]; then
    raw_target=$(readlink "$active_link" 2>/dev/null || true)
    case "$raw_target" in
        /*) resolved=$raw_target ;;
        *) link_dir=$(cd "$(dirname "$active_link")" 2>/dev/null && pwd); [ -n "$link_dir" ] && resolved="$link_dir/$raw_target" ;;
    esac
elif [ -f "$active_link" ]; then
    resolved=$active_link
fi
version_output=""
active_sha=""
if [ -x "$active_link" ]; then
    version_output=$("$active_link" --version 2>/dev/null || true)
    if command -v sha256sum >/dev/null 2>&1; then
        active_sha=$(sha256sum "$active_link" 2>/dev/null | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        active_sha=$(shasum -a 256 "$active_link" 2>/dev/null | awk '{print $1}')
    fi
fi
printf 'FLEETSTATUS1\\0%s\\0%s\\0%s\\0' "$version_output" "$resolved" "$active_sha"
"""


class DeliveryError(Exception):
    pass


class HostStatusError(DeliveryError):
    def __init__(self, outcome: str, reason: str):
        super().__init__(reason)
        self.outcome = outcome


def now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")


def stable_version(tag: str) -> str | None:
    value = tag[1:] if tag.startswith("v") else tag
    if re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value):
        return value
    return None


def version_key(version: str) -> tuple[int, int, int]:
    normalized = stable_version(version)
    if normalized is None:
        raise DeliveryError("invalid stable version")
    return tuple(int(part) for part in normalized.split("."))


def select_target(releases: list[dict], withdrawn: list[str]) -> dict | None:
    excluded = {stable_version(item) for item in withdrawn}
    candidates = []
    for release in releases:
        if release.get("draft") or release.get("prerelease"):
            continue
        version = stable_version(str(release.get("tag_name", "")))
        if version is not None and version not in excluded:
            candidates.append((version_key(version), release))
    return max(candidates, key=lambda item: item[0])[1] if candidates else None


def parse_checksum(contents: str, filename: str) -> str:
    matches = []
    for line in contents.splitlines():
        fields = line.strip().split(maxsplit=1)
        if len(fields) == 2 and fields[1].lstrip("*") == filename:
            if not re.fullmatch(r"[0-9a-fA-F]{64}", fields[0]):
                raise DeliveryError("invalid checksum entry")
            matches.append(fields[0].lower())
    if len(matches) != 1:
        raise DeliveryError("checksum entry missing or duplicated")
    return matches[0]


def is_within(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def private_path(raw: str, label: str) -> Path:
    path = Path(raw).expanduser().resolve()
    repo = Path(__file__).resolve().parents[1]
    workspace = os.environ.get("GITHUB_WORKSPACE")
    if is_within(path, repo) or (workspace and is_within(path, Path(workspace).resolve())):
        raise DeliveryError(f"{label} must be outside the checkout and Actions workspace")
    return path


def load_config(path: Path) -> dict:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise DeliveryError("private fleet configuration is unavailable or invalid") from error
    hosts = data.get("hosts") if isinstance(data, dict) else None
    withdrawn = data.get("withdrawn_versions") if isinstance(data, dict) else None
    readback = data.get("readback_path") if isinstance(data, dict) else None
    if not isinstance(hosts, list) or not hosts:
        raise DeliveryError("private fleet configuration has no destinations")
    if not isinstance(withdrawn, list) or not all(isinstance(item, str) for item in withdrawn):
        raise DeliveryError("withdrawn_versions must be a string array")
    withdrawn = [stable_version(item) for item in withdrawn]
    if any(item is None for item in withdrawn):
        raise DeliveryError("withdrawn_versions contains an invalid version")
    if not isinstance(readback, str) or not Path(readback).is_absolute():
        raise DeliveryError("readback_path must be absolute")
    checked, destinations = [], set()
    for entry in hosts:
        destination = entry.get("destination") if isinstance(entry, dict) else None
        target = entry.get("target") if isinstance(entry, dict) else None
        if not isinstance(destination, str) or not re.fullmatch(r"[A-Za-z0-9_.@-]+", destination):
            raise DeliveryError("destination must be an existing SSH alias or user@host")
        if destination in destinations or target not in TARGETS:
            raise DeliveryError("destination is duplicated or has an unsupported platform")
        destinations.add(destination)
        checked.append({"destination": destination, "target": target})
    return {
        "hosts": checked,
        "withdrawn_versions": sorted(set(withdrawn), key=version_key),
        "readback_path": private_path(readback, "readback_path"),
    }


def config_from_environment() -> Path:
    raw = os.environ.get("HARKLIGHT_FLEET_CONFIG")
    if not raw:
        raise DeliveryError("private fleet configuration is not configured")
    return private_path(raw, "HARKLIGHT_FLEET_CONFIG")


def request_bytes(url: str, token: str | None = None) -> bytes:
    headers = {"User-Agent": "harklight-agent-fleet-delivery"}
    if url.startswith(RELEASES_API):
        headers.update({"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"})
        if token:
            headers["Authorization"] = f"Bearer {token}"
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=30) as response:
            return response.read()
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise DeliveryError("release metadata or asset download failed") from error


def fetch_releases(token: str | None) -> list[dict]:
    result, page = [], 1
    while True:
        query = urllib.parse.urlencode({"per_page": 100, "page": page})
        try:
            batch = json.loads(request_bytes(f"{RELEASES_API}?{query}", token))
        except json.JSONDecodeError as error:
            raise DeliveryError("release metadata is invalid") from error
        if not isinstance(batch, list):
            raise DeliveryError("release metadata response is not a list")
        result.extend(batch)
        if len(batch) < 100:
            return result
        page += 1


def asset_url(release: dict, name: str) -> str:
    assets = release.get("assets")
    matches = [item for item in assets if item.get("name") == name] if isinstance(assets, list) else []
    if len(matches) != 1:
        raise DeliveryError("required release asset is missing or duplicated")
    url = matches[0].get("browser_download_url")
    prefix = f"https://github.com/{REPOSITORY}/releases/download/"
    if not isinstance(url, str) or not url.startswith(prefix):
        raise DeliveryError("release asset URL is not from the expected repository")
    return url


def fetch_archive(release: dict, target: str, token: str | None) -> dict:
    version = stable_version(str(release.get("tag_name", "")))
    if version is None:
        raise DeliveryError("selected release has an invalid version")
    filename = f"harklight-agent-{version}-{target}.tar.gz"
    sums = request_bytes(asset_url(release, "SHA256SUMS"), token).decode("utf-8")
    expected = parse_checksum(sums, filename)
    archive = request_bytes(asset_url(release, filename), token)
    actual = hashlib.sha256(archive).hexdigest()
    if actual != expected:
        raise DeliveryError("release archive checksum mismatch")
    return {"filename": filename, "sha256": expected, "bytes": archive}


def host_result(host: dict, version: str, withdrawn: list[str], outcome: str, reason: str = "") -> dict:
    return {
        "destination": host["destination"],
        "platform_target": host["target"],
        "target_version": version,
        "version_output": None,
        "resolved_path": None,
        "binary_sha256": None,
        "prior_release_kept": None,
        "withdrawn_releases": withdrawn,
        "attempted_at": now(),
        "outcome": outcome,
        "reason": reason or None,
    }


def parse_remote(stdout: bytes) -> dict:
    marker = b"FLEET1\0"
    offset = stdout.find(marker)
    if offset < 0:
        raise DeliveryError("remote readback unavailable")
    fields = stdout[offset + len(marker) :].split(b"\0")
    values = [field.decode("utf-8", errors="replace") for field in fields[:8]]
    if len(values) != 8:
        raise DeliveryError("remote readback incomplete")
    names = ("outcome", "reason", "version_output", "resolved_path", "binary_sha256",
             "prior_version", "prior_path", "prior_sha256")
    return dict(zip(names, values))


def failure_result(host: dict, version: str, withdrawn: list[str], code: int, stderr: bytes) -> dict:
    reason = stderr.decode("utf-8", errors="replace").strip()
    network_error = re.search(
        r"connection timed out|operation timed out|no route to host|network is unreachable|"
        r"connection refused|could not resolve hostname|name or service not known",
        reason,
        re.IGNORECASE,
    )
    outcome = "unreachable" if network_error else "failed"
    return host_result(host, version, withdrawn, outcome, reason or f"remote command exited {code}")


def parse_host_status(stdout: bytes) -> dict:
    marker = b"FLEETSTATUS1\0"
    offset = stdout.find(marker)
    if offset < 0:
        raise DeliveryError("active binary readback unavailable")
    fields = stdout[offset + len(marker) :].split(b"\0")
    values = [field.decode("utf-8", errors="replace") for field in fields[:3]]
    if len(values) != 3:
        raise DeliveryError("active binary readback incomplete")
    return {
        "version_output": values[0] or None,
        "resolved_path": values[1] or None,
        "binary_sha256": values[2] or None,
        "observed_at": now(),
    }


def read_host_status(host: dict, version: str, withdrawn: list[str]) -> dict:
    try:
        result = subprocess.run(
            ["ssh", *SSH_OPTIONS, host["destination"], "sh", "-s"],
            input=STATUS_SCRIPT, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            timeout=30, check=False,
        )
    except subprocess.TimeoutExpired as error:
        stderr = error.stderr if isinstance(error.stderr, bytes) else b""
        raise HostStatusError("unreachable", stderr.decode("utf-8", errors="replace") or "status command timed out") from error
    except OSError as error:
        raise HostStatusError("failed", str(error)) from error
    if result.returncode:
        failure = failure_result(host, version, withdrawn, result.returncode, result.stderr)
        raise HostStatusError(failure["outcome"], failure["reason"] or "status command failed")
    try:
        return parse_host_status(result.stdout)
    except DeliveryError as error:
        raise HostStatusError("failed", str(error)) from error


def apply_current_status(row: dict, status: dict) -> dict:
    row["version_output"] = status["version_output"]
    row["resolved_path"] = status["resolved_path"]
    row["binary_sha256"] = status["binary_sha256"]
    if status["version_output"] or status["resolved_path"] or status["binary_sha256"]:
        row["prior_release_kept"] = {
            "version": status["version_output"].removeprefix("harklight-agent ") if status["version_output"] else None,
            "path": status["resolved_path"],
            "sha256": status["binary_sha256"],
        }
    return row


def last_observed_status(row: dict, status: dict) -> dict:
    return {
        "observed_at": status["observed_at"],
        "version_output": status["version_output"],
        "resolved_path": status["resolved_path"],
        "binary_sha256": status["binary_sha256"],
    }


def attempt_host(host: dict, release: dict, withdrawn: list[str],
                 config_path: Path, token: str | None) -> dict:
    version = stable_version(str(release.get("tag_name", ""))) or ""
    try:
        current_status = read_host_status(host, version, withdrawn)
    except HostStatusError as error:
        return host_result(host, version, withdrawn, error.outcome, str(error))
    try:
        # Fetch and verify separately for each destination so a transient or
        # corrupted transfer fails only the destination being attempted.
        archive = fetch_archive(release, host["target"], token)
    except DeliveryError as error:
        return apply_current_status(host_result(host, version, withdrawn, "failed", str(error)), current_status)
    destination = host["destination"]
    remote_file = f"/tmp/harklight-agent-transfer-{uuid.uuid4().hex}.tar.gz"
    local_file = None
    uploaded = False
    try:
        with tempfile.NamedTemporaryFile(prefix="harklight-agent-", suffix=".tar.gz", delete=False) as stream:
            local_file = Path(stream.name)
            os.chmod(local_file, 0o600)
            stream.write(archive["bytes"])
        # A timed-out scp can leave a partial remote file. Mark the generated
        # path for cleanup before starting the transfer, not after it returns.
        uploaded = True
        copied = subprocess.run(
            ["scp", "-q", *SSH_OPTIONS, str(local_file), f"{destination}:{remote_file}"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120, check=False,
        )
        if copied.returncode:
            return apply_current_status(failure_result(host, version, withdrawn, copied.returncode, copied.stderr), current_status)

        current = load_config(config_path)
        target = select_target(fetch_releases(token), current["withdrawn_versions"])
        if (stable_version(str(target.get("tag_name", ""))) if target else None) != version:
            best_effort_remove(destination, remote_file)
            uploaded = False
            return apply_current_status(host_result(host, version, withdrawn, "target_changed", "target changed before activation"), current_status)

        command = [
            "ssh", *SSH_OPTIONS, destination, "sh", "-s", "--", version,
            host["target"], archive["sha256"], remote_file,
        ]
        installed = subprocess.run(
            command, input=INSTALLER.read_bytes(), stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, timeout=120, check=False,
        )
        uploaded = False  # The remote installer removes the exact copied archive on exit.
        try:
            remote = parse_remote(installed.stdout)
        except DeliveryError:
            return failure_result(host, version, withdrawn, installed.returncode, installed.stderr)
        result = host_result(host, version, withdrawn, remote["outcome"], remote["reason"])
        result["version_output"] = remote["version_output"] or None
        result["resolved_path"] = remote["resolved_path"] or None
        result["binary_sha256"] = remote["binary_sha256"] or None
        if remote["prior_path"] or remote["prior_version"] or remote["prior_sha256"]:
            result["prior_release_kept"] = {
                "version": remote["prior_version"] or None,
                "path": remote["prior_path"] or None,
                "sha256": remote["prior_sha256"] or None,
            }
        if installed.returncode and result["outcome"] != "failed":
            result["outcome"] = "failed"
            result["reason"] = "remote installer returned failure"
        return result
    except subprocess.TimeoutExpired as error:
        stderr = error.stderr if isinstance(error.stderr, bytes) else b""
        row = host_result(host, version, withdrawn, "unreachable", stderr.decode("utf-8", errors="replace") or "remote command timed out")
        row["last_observed"] = last_observed_status(row, current_status)
        return row
    except DeliveryError as error:
        return apply_current_status(host_result(host, version, withdrawn, "failed", str(error)), current_status)
    except OSError as error:
        return apply_current_status(host_result(host, version, withdrawn, "failed", str(error)), current_status)
    finally:
        if uploaded:
            best_effort_remove(destination, remote_file)
        if local_file is not None:
            try:
                local_file.unlink()
            except OSError:
                pass


def best_effort_remove(destination: str, remote_file: str) -> None:
    try:
        subprocess.run(
            ["ssh", *SSH_OPTIONS, destination, "rm", "-f", remote_file],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=20,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        pass


def run_hosts(hosts: list[dict], release: dict, withdrawn: list[str],
              config_path: Path, token: str | None) -> list[dict]:
    output = {}
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, len(hosts))) as pool:
        futures = {
            pool.submit(attempt_host, host, release, withdrawn, config_path, token): index
            for index, host in enumerate(hosts)
        }
        for future in concurrent.futures.as_completed(futures):
            index = futures[future]
            host = hosts[index]
            version = stable_version(str(release.get("tag_name", ""))) or ""
            try:
                output[index] = future.result()
            except Exception:
                output[index] = host_result(host, version, withdrawn, "failed", "worker failed")
    return [output[index] for index in range(len(hosts))]


def write_readback(path: Path, report: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
    try:
        with temporary.open("w", encoding="utf-8") as stream:
            os.chmod(temporary, 0o600)
            json.dump(report, stream, indent=2, sort_keys=True)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def previous_readback(path: Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
        return value if isinstance(value, dict) else {}
    except (OSError, json.JSONDecodeError):
        return {}


def report_for(config: dict, target: dict, hosts: list[dict], status: str, rounds: int) -> dict:
    version = stable_version(str(target.get("tag_name", ""))) if target else None
    outcomes = [row["outcome"] for row in hosts]
    done = bool(hosts) and all(
        row["outcome"] in {"updated", "already_current"}
        and row["version_output"] == f"harklight-agent {version}"
        for row in hosts
    )
    return {
        "updated_at": now(),
        "status": status,
        "target_version": version,
        "withdrawn_releases": config["withdrawn_versions"],
        "done": done,
        "rounds": rounds,
        "counts": {
            "hosts": len(hosts),
            "updated": outcomes.count("updated"),
            "already_current": outcomes.count("already_current"),
            "unreachable": outcomes.count("unreachable"),
            "failed": outcomes.count("failed"),
            "target_changed": outcomes.count("target_changed"),
        },
        "hosts": hosts,
    }


def attach_last_observed(hosts: list[dict], previous_hosts: list[dict]) -> list[dict]:
    previous_by_destination = {
        row.get("destination"): row
        for row in previous_hosts
        if isinstance(row, dict) and isinstance(row.get("destination"), str)
    }
    for row in hosts:
        if row["outcome"] not in {"unreachable", "failed"} or row["version_output"] is not None:
            continue
        previous = previous_by_destination.get(row["destination"])
        if previous is not None:
            row["last_observed"] = {
                "attempted_at": previous.get("attempted_at"),
                "target_version": previous.get("target_version"),
                "version_output": previous.get("version_output"),
                "resolved_path": previous.get("resolved_path"),
                "binary_sha256": previous.get("binary_sha256"),
                "prior_release_kept": previous.get("prior_release_kept"),
            }
    return hosts


def reconcile(path: Path, token: str | None) -> dict:
    config = load_config(path)
    prior = previous_readback(config["readback_path"])
    for round_number in range(1, MAX_ROUNDS + 1):
        releases = fetch_releases(token)
        config = load_config(path)
        target = select_target(releases, config["withdrawn_versions"])
        if target is None:
            report = {
                "updated_at": now(), "status": "no_target", "target_version": None,
                "withdrawn_releases": config["withdrawn_versions"], "done": False,
                "counts": {"hosts": len(config["hosts"])},
                "hosts": prior.get("hosts", []),
            }
            write_readback(config["readback_path"], report)
            return report

        results = run_hosts(config["hosts"], target, config["withdrawn_versions"], path, token)
        results = attach_last_observed(results, prior.get("hosts", []))

        current_config = load_config(path)
        latest = select_target(fetch_releases(token), current_config["withdrawn_versions"])
        changed = (
            (stable_version(str(latest.get("tag_name", ""))) if latest else None)
            != stable_version(str(target.get("tag_name", "")))
            or current_config["hosts"] != config["hosts"]
            or current_config["withdrawn_versions"] != config["withdrawn_versions"]
            or current_config["readback_path"] != config["readback_path"]
            or any(row["outcome"] == "target_changed" for row in results)
        )
        if changed:
            prior = {"hosts": results}
            continue
        report = report_for(config, target, results, "reconciled", round_number)
        write_readback(config["readback_path"], report)
        return report

    current_config = load_config(path)
    latest = select_target(fetch_releases(token), current_config["withdrawn_versions"])
    report = {
        "updated_at": now(), "status": "target_changed_during_run",
        "target_version": stable_version(str(latest.get("tag_name", ""))) if latest else None,
        "withdrawn_releases": current_config["withdrawn_versions"], "done": False,
        "rounds": MAX_ROUNDS, "counts": {"hosts": len(prior.get("hosts", []))},
        "hosts": prior.get("hosts", []),
    }
    write_readback(current_config["readback_path"], report)
    return report


def public_summary(report: dict) -> str:
    counts = report.get("counts", {})
    return (
        f"Fleet reconciliation status={report.get('status')} "
        f"target={report.get('target_version') or 'none'} "
        f"destinations={counts.get('hosts', 0)} updated={counts.get('updated', 0)} "
        f"current={counts.get('already_current', 0)} "
        f"unreachable={counts.get('unreachable', 0)} failed={counts.get('failed', 0)} "
        f"complete={str(bool(report.get('done'))).lower()}"
    )


def main() -> int:
    lock_path = Path.home() / ".local" / "state" / "harklight-agent-fleet-delivery" / "reconcile.lock"
    try:
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        with lock_path.open("a", encoding="utf-8") as lock:
            try:
                fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                print("Fleet reconciliation skipped because another local run is active.")
                return 0

            path = config_from_environment()
            report = reconcile(path, os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN"))
    except DeliveryError:
        print("Fleet reconciliation could not read its private input or release metadata.")
        return 1
    except Exception:
        print("Fleet reconciliation stopped before completing its private readback.")
        return 1
    print(public_summary(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
