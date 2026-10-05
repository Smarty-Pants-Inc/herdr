#!/usr/bin/env python3
"""Plan CI lanes from a GitHub event and a complete, local Git checkout."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path

MACOS_FILTER = (
    "not binary(live_handoff) or "
    "test(=live_handoff_import_exits_when_its_test_owner_dies)"
)
CONPTY_PREFIXES = (
    "src/", "assets/sounds/", "vendor/libghostty-vt/", "vendor/portable-pty/",
    "packaging/windows/", ".cargo/",
)
CONPTY_FILES = {
    ".gitattributes",
    "vendor/libghostty-vt.vendor.json",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".github/workflows/ci.yml",
    "scripts/ci_plan.py",
    "scripts/test_ci_plan.py",
    "scripts/package_windows_conpty.py",
    "scripts/package_windows_conpty.ps1",
    "scripts/windows_smoke_conpty_path.ps1",
    "scripts/windows_conpty_enhanced_input_probe.ps1",
    "scripts/windows_install_conpty_package_test.ps1",
    "distribution/install.ps1",
    "distribution/install.cmd",
    # These are included in the executable, not merely documentation.
    "skills/herdr/SKILL.md",
    "docs/next/api/herdr-api.schema.json",
}


def object_or_empty(value: object) -> dict:
    return value if isinstance(value, dict) else {}


def check_matrix(event_name: str, event: dict) -> dict:
    pr = object_or_empty(event.get("pull_request"))
    head = object_or_empty(pr.get("head"))
    base = object_or_empty(pr.get("base"))
    head_repo = object_or_empty(head.get("repo")).get("full_name")
    base_repo = object_or_empty(base.get("repo")).get("full_name")
    author = object_or_empty(pr.get("user")).get("id")
    head_ref = head.get("ref")
    queue = (
        event_name == "pull_request"
        and type(author) is int
        and author == 37929162
        and isinstance(head_ref, str)
        and head_ref.startswith("mergify/merge-queue/")
        and base.get("ref") in ("master", "smarty-preview-source")
        and isinstance(head_repo, str)
        and bool(head_repo)
        and head_repo == base_repo
    )
    include = [{"os": "ubuntu-latest", "kind": "unix", "nextest_filter": "all()"}]
    if (
        queue
        or (event_name == "push" and event.get("ref") == "refs/heads/master")
        or event_name == "workflow_dispatch"
    ):
        include.append({"os": "macos-latest", "kind": "unix", "nextest_filter": MACOS_FILTER})
    include.append({"os": "windows-latest", "kind": "windows"})
    return {"include": include}


def diff_range(event_name: str, event: dict) -> tuple[str, str, str]:
    if event_name == "pull_request":
        pr = event["pull_request"]
        before, after, separator = pr["base"]["sha"], pr["head"]["sha"], "..."
    elif event_name == "push":
        before, after, separator = event["before"], event["after"], ".."
    else:
        raise ValueError("unsupported event")
    for sha in (before, after):
        if not isinstance(sha, str) or not re.fullmatch(r"[0-9a-fA-F]{40}|[0-9a-fA-F]{64}", sha):
            raise ValueError("expected full commit SHA")
        if not sha.strip("0"):
            raise ValueError("zero commit SHA")
    return before, after, separator


def git(*args: str) -> bytes:
    # Never fetch missing objects, including from a partial clone's promisor remote.
    env = {**os.environ, "GIT_NO_LAZY_FETCH": "1", "GIT_TERMINAL_PROMPT": "0"}
    return subprocess.check_output(
        ["git", "--no-replace-objects", *args],
        stderr=subprocess.PIPE,
        env=env,
        timeout=30,
    )


def needs_conpty(event_name: str, event: dict) -> bool:
    try:
        before, after, separator = diff_range(event_name, event)
        if git("rev-parse", "--is-shallow-repository").strip() != b"false":
            raise ValueError("checkout is not complete")
        for sha in (before, after):
            if git("cat-file", "-t", sha).strip() != b"commit":
                raise ValueError("SHA does not name a commit")
        checkout = git("rev-parse", "--verify", "HEAD^{commit}").strip().decode("ascii")
        if separator == "...":
            bases = git("merge-base", "--all", before, after).split()
            if len(bases) != 1:
                raise ValueError("expected a single merge base")
            # Classify the merge tree actually built, including base-side changes and
            # merge resolutions. A head-only or stale checkout cannot justify a skip.
            git("merge-base", "--is-ancestor", before, checkout)
            git("merge-base", "--is-ancestor", after, checkout)
            before, after = bases[0].decode("ascii"), checkout
        elif checkout.lower() != after.lower():
            raise ValueError("push checkout does not match event after")
        # Disabling renames preserves both paths, especially a source renamed into docs.
        paths = git(
            "diff", "--no-ext-diff", "--no-textconv", "--no-renames", "--name-only", "-z",
            before, after, "--",
        )
        if paths and not paths.endswith(b"\0"):
            raise ValueError("incomplete changed-path output")
        return any(
            path in CONPTY_FILES or path.startswith(CONPTY_PREFIXES)
            for path in paths.decode("utf-8", errors="surrogateescape").split("\0") if path
        )
    except (KeyError, TypeError, ValueError, UnicodeError, OSError, subprocess.SubprocessError) as error:
        print(f"ci_plan: incomplete diff evidence; enabling ConPTY ({error})", file=sys.stderr)
        return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-name", required=True)
    parser.add_argument("--event-path", required=True, type=Path)
    parser.add_argument("--github-output", required=True, type=Path)
    args = parser.parse_args()
    try:
        event = json.loads(args.event_path.read_text(encoding="utf-8"))
        if not isinstance(event, dict):
            raise ValueError("event must be an object")
    except (OSError, ValueError, UnicodeError) as error:
        print(f"ci_plan: cannot read event; using conservative plan ({error})", file=sys.stderr)
        event = {}
    matrix = json.dumps(check_matrix(args.event_name, event), separators=(",", ":"))
    conpty = str(needs_conpty(args.event_name, event)).lower()
    with args.github_output.open("a", encoding="utf-8") as output:
        output.write(f"matrix={matrix}\nconpty={conpty}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
