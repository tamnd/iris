#!/usr/bin/env python3
"""Checks that are about discipline rather than about compilation.

The promises in docs/ROADMAP.md are only real if something enforces them. A
compiler will not notice a nightly toolchain creeping into a workflow or a
patched dependency arriving in the lockfile, because both of those make the
build work rather than break it. That is exactly why they need a check.

What it checks:

  1. The pinned toolchain is a release channel and it is the workspace's
     declared minimum supported Rust version.
  2. Every crate declares that same minimum, so the MSRV job is checking one
     number rather than nine.
  3. No manifest carries a [patch] section and no cargo configuration replaces
     a source, so the tree builds against the registry it says it does.
  4. No workflow reaches for nightly except the two that fuzz, which need it,
     and the one that runs Miri, which only exists on nightly.
  5. No hostname or address from the runner fleet has leaked into a committed
     file. Machines are named by hardware in public, because a hostname tells a
     reader nothing they can compare against.
"""

from __future__ import annotations

import pathlib
import re
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Fuzzing needs nightly because libFuzzer's instrumentation is a nightly only
# compiler flag, and Miri is a nightly component. Everything else builds on the
# pinned release toolchain, which is the whole point of the promise.
NIGHTLY_ALLOWED = {"nightly.yml", "soak.yml"}
NIGHTLY_ALLOWED_JOBS = {"miri"}

failures: list[str] = []


def fail(message: str) -> None:
    failures.append(message)


def workspace_rust_version() -> str | None:
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    package = manifest.get("workspace", {}).get("package", {})
    version = package.get("rust-version")
    if not version:
        fail("Cargo.toml: [workspace.package] declares no rust-version")
    return version


def check_toolchain(rust_version: str | None) -> None:
    path = ROOT / "rust-toolchain.toml"
    if not path.exists():
        fail("there is no rust-toolchain.toml, so the toolchain is whatever a developer had")
        return

    channel = tomllib.loads(path.read_text(encoding="utf-8")).get("toolchain", {}).get("channel")
    if not channel:
        fail("rust-toolchain.toml: no channel")
        return
    if channel in {"nightly", "beta"} or channel.startswith(("nightly-", "beta-")):
        fail(f"rust-toolchain.toml: the pinned channel is {channel}, and this project ships on release")
    if rust_version and channel != rust_version:
        fail(
            f"rust-toolchain.toml pins {channel} and the workspace declares rust-version "
            f"{rust_version}, so the toolchain and the promise disagree"
        )


def check_crate_rust_versions(rust_version: str | None) -> None:
    """Every crate inherits the one number, rather than carrying its own.

    A crate that declares its own is not caught by the MSRV job going green,
    because cargo-hack checks each crate against what that crate claims.
    """
    for manifest in sorted((ROOT / "crates").glob("*/Cargo.toml")):
        package = tomllib.loads(manifest.read_text(encoding="utf-8")).get("package", {})
        declared = package.get("rust-version")
        rel = manifest.relative_to(ROOT)
        if declared is None:
            fail(f"{rel}: declares no rust-version")
        elif isinstance(declared, dict):
            if not declared.get("workspace"):
                fail(f"{rel}: rust-version is a table that does not inherit from the workspace")
        elif rust_version and declared != rust_version:
            fail(f"{rel}: declares rust-version {declared} and the workspace declares {rust_version}")


def check_no_patched_dependencies() -> None:
    """No [patch], and no source replacement.

    Both of these mean the tree is not built from what its manifests say, and
    the prior art in this area vendors and patches two large dependencies. That
    is fine for a paper artifact and it compounds monthly for anything else.
    """
    for manifest in sorted(ROOT.rglob("Cargo.toml")):
        if "target" in manifest.relative_to(ROOT).parts:
            continue
        parsed = tomllib.loads(manifest.read_text(encoding="utf-8"))
        if "patch" in parsed:
            fail(f"{manifest.relative_to(ROOT)}: carries a [patch] section")
        if "replace" in parsed:
            fail(f"{manifest.relative_to(ROOT)}: carries a [replace] section")

    for config in [ROOT / ".cargo" / "config.toml", ROOT / ".cargo" / "config"]:
        if not config.exists():
            continue
        parsed = tomllib.loads(config.read_text(encoding="utf-8"))
        for name, source in parsed.get("source", {}).items():
            if "replace-with" in source:
                fail(f"{config.relative_to(ROOT)}: source {name} is replaced")


def check_no_stray_nightly() -> None:
    nightly = re.compile(r"\bnightly\b")
    for workflow in sorted((ROOT / ".github" / "workflows").glob("*.yml")):
        if workflow.name in NIGHTLY_ALLOWED:
            continue
        job = None
        for line_no, line in enumerate(workflow.read_text(encoding="utf-8").splitlines(), 1):
            matched = re.match(r"^  ([a-z0-9_-]+):\s*$", line)
            if matched:
                job = matched.group(1)
            if job in NIGHTLY_ALLOWED_JOBS:
                continue
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            if nightly.search(line):
                fail(f"{workflow.relative_to(ROOT)}:{line_no}: reaches for nightly outside the fuzzing and Miri jobs")


def check_no_machine_identity() -> None:
    """Machines are named by hardware in public, never by hostname or address.

    Patterns are shapes rather than values, so adding a runner to the fleet does
    not mean editing this list, and so a machine that was never written down
    here is caught the same as one that was.
    """
    patterns = [
        (re.compile(r"\b(?:\d{1,3}\.){3}\d{1,3}\b"), "an IP address"),
        (re.compile(r"\bssh\s+[a-z0-9_-]+@"), "an ssh target"),
        # A numbered host and the desktop, which are the shapes the fleet's names
        # actually take. Bare "server" is a word people use in prose about object
        # storage, so it is not one of them.
        (re.compile(r"\bserver\d+\b", re.IGNORECASE), "a hostname"),
        (re.compile(r"\bgaming-?pc\b", re.IGNORECASE), "a hostname"),
    ]
    allow = re.compile(r"\b(?:0\.0\.0\.0|127\.0\.0\.1|255\.255\.255\.255)\b")
    suffixes = {".md", ".rs", ".toml", ".yml", ".yaml", ".py", ".sh", ".ps1"}
    skip_dirs = {".git", "target", "corpus", "artifacts"}
    for path in ROOT.rglob("*"):
        if not path.is_file() or path.suffix not in suffixes:
            continue
        if set(path.relative_to(ROOT).parts) & skip_dirs:
            continue
        if path.name == "discipline.py":
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        for line_no, line in enumerate(text.splitlines(), 1):
            for pattern, what in patterns:
                for hit in pattern.finditer(line):
                    if allow.fullmatch(hit.group()):
                        continue
                    fail(f"{path.relative_to(ROOT)}:{line_no}: looks like {what}: {hit.group()}")


def main() -> int:
    rust_version = workspace_rust_version()
    check_toolchain(rust_version)
    check_crate_rust_versions(rust_version)
    check_no_patched_dependencies()
    check_no_stray_nightly()
    check_no_machine_identity()
    if failures:
        print("Discipline checks failed:\n", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        print(
            "\nThese checks exist because a promise nothing enforces is a sentence in a document.",
            file=sys.stderr,
        )
        return 1
    print("Discipline checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
