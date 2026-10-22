#!/usr/bin/env python3
"""Collect license texts of every crate compiled into harklight-agent.

Usage: third_party_notices.py <target-triple> <out-file>
Runs cargo metadata for the locked dependency graph resolved for the target
and concatenates each dependency's license files from its source directory.
"""
import glob
import json
import os
import subprocess
import sys

target, out = sys.argv[1], sys.argv[2]
meta = json.loads(
    subprocess.check_output(
        [
            "cargo", "metadata", "--locked", "--format-version", "1",
            "--filter-platform", target,
        ]
    )
)
root = meta["resolve"]["root"]
by_id = {p["id"]: p for p in meta["packages"]}
resolved = {n["id"] for n in meta["resolve"]["nodes"]}
packages = sorted(
    (by_id[i] for i in resolved if i != root),
    key=lambda p: (p["name"], p["version"]),
)
sections = []
missing = []
for p in packages:
    directory = os.path.dirname(p["manifest_path"])
    files = sorted(
        f for pattern in ("LICENSE*", "LICENCE*", "COPYING*", "NOTICE*")
        for f in glob.glob(os.path.join(directory, pattern))
        if os.path.isfile(f)
    )
    header = f"{p['name']} {p['version']}\nlicense: {p.get('license') or 'see files'}\n"
    if p.get("repository"):
        header += f"source: {p['repository']}\n"
    body = ""
    for f in files:
        with open(f, encoding="utf-8", errors="replace") as handle:
            body += f"\n--- {os.path.basename(f)} ---\n{handle.read().strip()}\n"
    if not files:
        missing.append(f"{p['name']} {p['version']}")
    sections.append(header + body)
with open(out, "w", encoding="utf-8") as handle:
    handle.write(
        "Third-party notices for harklight-agent\n"
        f"Target: {target}\n"
        f"Crates: {len(packages)}\n\n"
        + ("\n" + "=" * 72 + "\n").join(sections)
    )
print(f"wrote {out}: {len(packages)} crates, {len(missing)} without license files")
for name in missing:
    print(f"  no license file shipped in crate: {name}")
