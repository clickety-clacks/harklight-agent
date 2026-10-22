#!/usr/bin/env python3
"""Write the Homebrew formula for a harklight-agent release.

Environment:
  VERSION   release version without the leading v, for example 0.1.1
  BASE_URL  directory URL that holds the release tarballs; defaults to the
            GitHub release download URL for VERSION
  SHA_AARCH64_APPLE_DARWIN, SHA_X86_64_APPLE_DARWIN,
  SHA_X86_64_UNKNOWN_LINUX_GNU, SHA_AARCH64_UNKNOWN_LINUX_GNU
            SHA-256 of each tarball
  OUT       output path, defaults to harklight-agent.rb
"""
import os
import re
import sys


def need(name):
    value = os.environ.get(name, "").strip()
    if not value:
        sys.exit(f"{name} is required")
    return value


def sha(name):
    value = need(name)
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        sys.exit(f"{name} is not a SHA-256 hex digest")
    return value


version = need("VERSION")
if not re.fullmatch(r"\d+\.\d+\.\d+(-[0-9A-Za-z.]+)?", version):
    sys.exit("VERSION must look like X.Y.Z")
base = os.environ.get("BASE_URL", "").strip() or (
    f"https://github.com/clickety-clacks/harklight-agent/releases/download/v{version}"
)
base = base.rstrip("/")
out = os.environ.get("OUT", "harklight-agent.rb")


def asset(target):
    return f"{base}/harklight-agent-{version}-{target}.tar.gz"


formula = f'''class HarklightAgent < Formula
  desc "Send request-bound Harklight conversations and listen for replies"
  homepage "https://github.com/clickety-clacks/harklight-agent"
  license "MIT"
  version "{version}"

  on_macos do
    on_arm do
      url "{asset("aarch64-apple-darwin")}"
      sha256 "{sha("SHA_AARCH64_APPLE_DARWIN")}"
    end
    on_intel do
      url "{asset("x86_64-apple-darwin")}"
      sha256 "{sha("SHA_X86_64_APPLE_DARWIN")}"
    end
  end

  on_linux do
    on_arm do
      url "{asset("aarch64-unknown-linux-gnu")}"
      sha256 "{sha("SHA_AARCH64_UNKNOWN_LINUX_GNU")}"
    end
    on_intel do
      url "{asset("x86_64-unknown-linux-gnu")}"
      sha256 "{sha("SHA_X86_64_UNKNOWN_LINUX_GNU")}"
    end
  end

  def install
    bin.install "harklight-agent"
    doc.install "README.md", "THIRD_PARTY_NOTICES.txt"
  end

  test do
    assert_match "harklight-agent {version}", shell_output("#{{bin}}/harklight-agent --version")
  end
end
'''

with open(out, "w", encoding="utf-8") as handle:
    handle.write(formula)
print(f"wrote {out}")
