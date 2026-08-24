#!/usr/bin/env python3
from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import re
import stat
import subprocess
import tomllib
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

ASSET_TARGETS = (
    "linux-x86_64",
    "linux-aarch64",
    "macos-x86_64",
    "macos-aarch64",
    "windows-x86_64",
)
EXPECTED_ASSET_NAMES = {
    **{target: f"herdr-{target}" for target in ASSET_TARGETS},
    "windows-x86_64": "herdr-windows-x86_64.zip",
}
OMP_EXPECTED_ASSET_NAMES = {
    "linux-x86_64": "omp-linux-x86_64",
    "linux-aarch64": "omp-linux-aarch64",
    "macos-x86_64": "omp-macos-x86_64",
    "macos-aarch64": "omp-macos-aarch64",
}
OMP_NATIVE_ASSET_NAMES = {
    "linux-x86_64": (
        "pi_natives.linux-x64-baseline.node",
        "pi_natives.linux-x64-modern.node",
    ),
    "linux-aarch64": ("pi_natives.linux-arm64.node",),
    "macos-x86_64": ("pi_natives.darwin-x64-baseline.node",),
    "macos-aarch64": ("pi_natives.darwin-arm64.node",),
}
OMP_ASSET_TARGETS = tuple(OMP_EXPECTED_ASSET_NAMES)
PAIR_MANIFEST_SCHEMA = "smarty.paired-release.v1"
PAIR_MANIFEST_ASSET_NAME = "smarty-pair.json"
SPDX_ASSET_NAME = "smarty-pair.spdx.json"
SPDX_CREATOR = "Tool: smarty-preview-1.0"
TRUSTED_BUN_VERSION = "1.4.0"
TRUSTED_ZIG_VERSION = "0.15.2"
SEMANTIC_VERIFICATION_SCHEMA = "smarty.semantic-verification.v1"
SOURCE_ARCHIVE_NAMES = ("herdr-source.tar", "omp-source.tar")
PAIR_PROVENANCE_ASSET_NAME = "smarty-pair.provenance.sigstore.json"
SPDX_PROVENANCE_ASSET_NAME = "smarty-pair.spdx.sigstore.json"
HERDR_REPOSITORY = "Smarty-Pants-Inc/herdr"
PARENT_REPOSITORY = "Smarty-Pants-Inc/smarty-dev"
OMP_REPOSITORY = "Smarty-Pants-Inc/oh-my-pi"
PAIR_ID_DOMAIN = f"{PAIR_MANIFEST_SCHEMA}\0".encode("ascii")
PAIRED_BUILD_ID_RE = re.compile(
    r"^(?P<day>\d{4}-\d{2}-\d{2})-p(?P<parent>[0-9a-f]{40})-"
    r"r(?P<herdr>[0-9a-f]{40})-o(?P<omp>[0-9a-f]{40})$"
)
LEGACY_BOOTSTRAP_SCHEMA = "smarty.windows-bootstrap.v1"
LEGACY_BOOTSTRAP_ID_RE = re.compile(r"^bootstrap-[0-9a-f]{64}$")
CONPTY_METADATA_PATH = Path("packaging/windows/conpty.json")
OMP_BUN_WORKSPACE = "packages/coding-agent"
LEGACY_PAYLOAD_ASSET_NAMES = tuple(EXPECTED_ASSET_NAMES.values()) + tuple(
    OMP_EXPECTED_ASSET_NAMES.values()
)
LEGACY_SIDECAR_ASSET_NAMES = tuple(
    f"{name}.sha256" for name in LEGACY_PAYLOAD_ASSET_NAMES
)
LEGACY_ASSET_NAMES = LEGACY_PAYLOAD_ASSET_NAMES + LEGACY_SIDECAR_ASSET_NAMES
LEGACY_RELEASE_ASSET_NAMES = LEGACY_ASSET_NAMES
OMP_NATIVE_PAYLOAD_ASSET_NAMES = tuple(
    name for target in OMP_ASSET_TARGETS for name in OMP_NATIVE_ASSET_NAMES[target]
)
RELEASE_PAYLOAD_ASSET_NAMES = (
    LEGACY_PAYLOAD_ASSET_NAMES + OMP_NATIVE_PAYLOAD_ASSET_NAMES
)
RELEASE_SIDECAR_ASSET_NAMES = tuple(
    f"{name}.sha256" for name in RELEASE_PAYLOAD_ASSET_NAMES
)
RELEASE_ASSET_NAMES = RELEASE_PAYLOAD_ASSET_NAMES + RELEASE_SIDECAR_ASSET_NAMES
PLATFORM_PROVENANCE_ASSET_NAMES = {
    target: f"smarty-provenance-{target}.sigstore.json" for target in ASSET_TARGETS
}
METADATA_ASSET_NAMES = (
    PAIR_MANIFEST_ASSET_NAME,
    SPDX_ASSET_NAME,
    *tuple(PLATFORM_PROVENANCE_ASSET_NAMES.values()),
    PAIR_PROVENANCE_ASSET_NAME,
    SPDX_PROVENANCE_ASSET_NAME,
)
EVIDENCE_ASSET_NAMES = (
    SPDX_ASSET_NAME,
    *tuple(PLATFORM_PROVENANCE_ASSET_NAMES.values()),
    SPDX_PROVENANCE_ASSET_NAME,
)
FULL_RELEASE_ASSET_NAMES = RELEASE_ASSET_NAMES + METADATA_ASSET_NAMES
PLATFORM_MATRIX = {
    "linux-x86_64": {
        "os": "linux",
        "architecture": "x86_64",
        "abi": "glibc",
        "runner": "smarty-linux-16-core",
        "payloads": {
            "herdr": EXPECTED_ASSET_NAMES["linux-x86_64"],
            "omp": OMP_EXPECTED_ASSET_NAMES["linux-x86_64"],
        },
    },
    "linux-aarch64": {
        "os": "linux",
        "architecture": "aarch64",
        "abi": "glibc",
        "runner": "smarty-linux-arm-16-core",
        "payloads": {
            "herdr": EXPECTED_ASSET_NAMES["linux-aarch64"],
            "omp": OMP_EXPECTED_ASSET_NAMES["linux-aarch64"],
        },
    },
    "macos-x86_64": {
        "os": "macos",
        "architecture": "x86_64",
        "abi": "darwin",
        "runner": "smarty-macos-intel-12-core",
        "payloads": {
            "herdr": EXPECTED_ASSET_NAMES["macos-x86_64"],
            "omp": OMP_EXPECTED_ASSET_NAMES["macos-x86_64"],
        },
    },
    "macos-aarch64": {
        "os": "macos",
        "architecture": "aarch64",
        "abi": "darwin",
        "runner": "smarty-macos-arm-5-core",
        "payloads": {
            "herdr": EXPECTED_ASSET_NAMES["macos-aarch64"],
            "omp": OMP_EXPECTED_ASSET_NAMES["macos-aarch64"],
        },
    },
    "windows-x86_64": {
        "os": "windows",
        "architecture": "x86_64",
        "abi": "msvc",
        "runner": "smarty-windows-16-core",
        "payloads": {
            "herdr": EXPECTED_ASSET_NAMES["windows-x86_64"],
            "omp": None,
        },
    },
}
BUN_PLATFORM_TARGETS = {
    target: (
        "darwin"
        if PLATFORM_MATRIX[target]["os"] == "macos"
        else PLATFORM_MATRIX[target]["os"],
        {"x86_64": "x64", "aarch64": "arm64"}[PLATFORM_MATRIX[target]["architecture"]],
    )
    for target in OMP_ASSET_TARGETS
}
BUN_OS_ALIASES = {"macos": "darwin", "osx": "darwin", "windows": "win32"}
BUN_CPU_ALIASES = {"amd64": "x64", "x86_64": "x64", "aarch64": "arm64"}
PLATFORM_PAYLOAD_ASSET_NAMES = {
    target: (
        tuple(name for name in data["payloads"].values() if name is not None)
        + OMP_NATIVE_ASSET_NAMES.get(target, ())
    )
    for target, data in PLATFORM_MATRIX.items()
}
PAYLOAD_METADATA_BY_NAME = {
    name: {
        "component": component,
        "platform": target,
        "format": "zip" if name.endswith(".zip") else "binary",
        "abi": "musl"
        if data["os"] == "linux" and component == "herdr"
        else data["abi"],
    }
    for target, data in PLATFORM_MATRIX.items()
    for component, name in data["payloads"].items()
    if name is not None
}
PAYLOAD_METADATA_BY_NAME.update(
    {
        name: {
            "component": "omp-native",
            "platform": target,
            "format": "binary",
            "abi": PLATFORM_MATRIX[target]["abi"],
        }
        for target, names in OMP_NATIVE_ASSET_NAMES.items()
        for name in names
    }
)
CARGO_METADATA_TARGETS = {
    "herdr": {
        "linux-x86_64": "x86_64-unknown-linux-musl",
        "linux-aarch64": "aarch64-unknown-linux-musl",
        "macos-x86_64": "x86_64-apple-darwin",
        "macos-aarch64": "aarch64-apple-darwin",
        "windows-x86_64": "x86_64-pc-windows-msvc",
    },
    "omp": {
        "linux-x86_64": "x86_64-unknown-linux-gnu",
        "linux-aarch64": "aarch64-unknown-linux-gnu",
        "macos-x86_64": "x86_64-apple-darwin",
        "macos-aarch64": "aarch64-apple-darwin",
    },
}
CARGO_METADATA_FILENAMES = {
    component: {platform: f"{component}-{platform}.json" for platform in targets}
    for component, targets in CARGO_METADATA_TARGETS.items()
}
LOCK_INPUTS = (
    ("herdr", "Cargo.lock"),
    ("omp", "bun.lock"),
    ("omp", "MODULE.bazel.lock"),
    ("omp", "Cargo.lock"),
    ("omp", ".bazelversion"),
    ("omp", "rust-toolchain.toml"),
)
OMP_SOURCE_FIELDS = ("repository", "commit", "tree", "version", "build_id")
OMP_REPLACEMENT_PREFIX = "REPLACE_WITH_"
HIDDEN_SUBJECTS = (
    "docs: update website manifest",
    "docs: update preview manifest",
    "chore: approve contributor",
    "chore: approve merged contributor",
)
TYPE_HEADINGS = {
    "feat": "Added",
    "fix": "Fixed",
    "perf": "Performance",
    "docs": "Maintenance",
    "ci": "Maintenance",
    "test": "Maintenance",
    "refactor": "Maintenance",
    "chore": "Maintenance",
}
TYPE_ORDER = ("Added", "Fixed", "Performance", "Maintenance", "Other")
COMMIT_RE = re.compile(r"^(?P<kind>[a-z]+)(?:\([^)]+\))?!?:\s+(?P<body>.+)$")


def run_git(args: list[str]) -> str:
    return subprocess.check_output(["git", *args], text=True).strip()


def normalize_version(version: str) -> str:
    return version.strip().removeprefix("v")


def latest_stable_tag(ref: str | None = None) -> str:
    args = ["describe", "--tags", "--match", "v[0-9]*", "--abbrev=0"]
    if ref:
        args.append(ref)
    return run_git(args)


def git_is_ancestor(ancestor: str, descendant: str) -> bool:
    result = subprocess.run(
        ["git", "merge-base", "--is-ancestor", ancestor, descendant],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return result.returncode == 0


def read_json(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def validate_omp_source(data: dict[str, Any]) -> dict[str, str]:
    if set(data) != set(OMP_SOURCE_FIELDS):
        raise ValueError(
            f"OMP source descriptor must contain exactly: {', '.join(OMP_SOURCE_FIELDS)}"
        )
    source = {field: str(data[field]).strip() for field in OMP_SOURCE_FIELDS}
    for field, value in source.items():
        if not value or OMP_REPLACEMENT_PREFIX in value:
            raise ValueError(
                f"OMP source {field} is missing or still a replacement placeholder"
            )
        if "\n" in value or "\r" in value:
            raise ValueError(f"OMP source {field} must be one line")
    if not re.fullmatch(r"[^/\s]+/[^/\s]+", source["repository"]):
        raise ValueError("OMP source repository must be owner/name")
    for field in ("commit", "tree"):
        if not re.fullmatch(r"[0-9a-fA-F]{40}", source[field]):
            raise ValueError(f"OMP source {field} must be a 40-character Git object ID")
    return source


def read_omp_source(path: Path) -> dict[str, str]:
    data = read_json(path)
    if not isinstance(data, dict):
        raise ValueError("OMP source descriptor must be a JSON object")
    return validate_omp_source(data)


def previous_preview_commit(path: Path) -> str | None:
    data = read_json(path)
    if not data:
        return None
    commit = data.get("commit")
    return commit if isinstance(commit, str) and commit.strip() else None


def hidden_subject(subject: str) -> bool:
    lowered = subject.strip().lower()
    return any(lowered.startswith(prefix) for prefix in HIDDEN_SUBJECTS)


def latest_publishable_commit(ref: str) -> str:
    output = run_git(["log", "--pretty=format:%H%x00%s", ref])
    for line in output.splitlines():
        commit, _, subject = line.partition("\x00")
        if commit and not hidden_subject(subject):
            return commit
    raise SystemExit(f"no publishable commit found in {ref}")


def commit_subjects(previous: str, commit: str) -> list[str]:
    output = run_git(["log", "--pretty=format:%s", f"{previous}..{commit}"])
    if not output:
        return []
    subjects = []
    for line in output.splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        if hidden_subject(stripped):
            continue
        subjects.append(stripped)
    return subjects


def preview_range_base(previous: str, commit: str) -> str:
    try:
        stable = latest_stable_tag(commit)
    except subprocess.CalledProcessError:
        return previous
    if git_is_ancestor(previous, stable) and git_is_ancestor(stable, commit):
        return stable
    return previous


def humanize_subject(subject: str) -> tuple[str, str]:
    match = COMMIT_RE.match(subject)
    if not match:
        return "Other", subject[0].upper() + subject[1:]
    kind = match.group("kind")
    body = match.group("body").strip()
    heading = TYPE_HEADINGS.get(kind, "Other")
    if body:
        body = body[0].upper() + body[1:]
    else:
        body = subject
    return heading, body


def build_notes(
    previous: str, commit: str, build_id: str, base_version: str, repo: str
) -> str:
    short = commit[:12]
    compare = f"https://github.com/{repo}/compare/{previous}...{commit}"
    lines = [
        f"Preview build {build_id}",
        "",
        f"Built from `{short}` on `master`.",
        f"Base stable: v{normalize_version(base_version)}",
        f"Compare: {compare}",
        "",
    ]
    grouped: dict[str, list[str]] = {heading: [] for heading in TYPE_ORDER}
    for subject in commit_subjects(previous, commit):
        heading, body = humanize_subject(subject)
        grouped.setdefault(heading, []).append(body)

    wrote = False
    for heading in TYPE_ORDER:
        items = grouped.get(heading, [])
        if not items:
            continue
        wrote = True
        lines.append(f"### {heading}")
        for item in items:
            lines.append(f"- {item}")
        lines.append("")

    if not wrote:
        lines.extend(
            ["### Changed", "- Rebuilt preview from the current master branch.", ""]
        )

    return "\n".join(lines).rstrip() + "\n"


def default_asset_urls(repo: str, tag: str) -> dict[str, str]:
    return {
        target: f"https://github.com/{repo}/releases/download/{tag}/{EXPECTED_ASSET_NAMES[target]}"
        for target in ASSET_TARGETS
    }


def default_omp_asset_urls(repo: str, tag: str) -> dict[str, str]:
    return {
        target: f"https://github.com/{repo}/releases/download/{tag}/{OMP_EXPECTED_ASSET_NAMES[target]}"
        for target in OMP_ASSET_TARGETS
    }


def read_sha_file(path: Path | None) -> dict[str, str]:
    if path is None:
        return {}
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict):
        raise SystemExit("sha file must be a JSON object")
    return {str(key): str(value) for key, value in data.items()}


def asset_objects(
    urls: dict[str, str], shas: dict[str, str]
) -> dict[str, dict[str, str]]:
    if set(shas) != set(ASSET_TARGETS):
        raise ValueError(
            f"Herdr SHA targets must be exactly: {', '.join(ASSET_TARGETS)}"
        )
    assets: dict[str, dict[str, str]] = {}
    for target in ASSET_TARGETS:
        sha = shas[target]
        if not re.fullmatch(r"[0-9a-fA-F]{64}", sha):
            raise ValueError(f"{target} requires a SHA-256 digest")
        entry = {"url": urls[target], "sha256": sha}
        if target.startswith("windows-"):
            entry["format"] = "zip"
        assets[target] = entry
    return assets


def omp_asset_objects(
    urls: dict[str, str], shas: dict[str, str]
) -> dict[str, dict[str, str]]:
    if set(shas) != set(OMP_ASSET_TARGETS):
        raise ValueError(
            f"OMP SHA targets must be exactly: {', '.join(OMP_ASSET_TARGETS)}"
        )
    assets: dict[str, dict[str, str]] = {}
    for target in OMP_ASSET_TARGETS:
        sha = shas[target]
        if not re.fullmatch(r"[0-9a-fA-F]{64}", sha):
            raise ValueError(f"OMP {target} requires a SHA-256 digest")
        assets[target] = {"url": urls[target], "sha256": sha}
    return assets


def omp_metadata(
    source: dict[str, str], repo: str, tag: str, shas: dict[str, str]
) -> dict[str, Any]:
    descriptor = validate_omp_source(source)
    return {
        "build_id": descriptor["build_id"],
        "commit": descriptor["commit"],
        "tree": descriptor["tree"],
        "version": descriptor["version"],
        "assets": omp_asset_objects(default_omp_asset_urls(repo, tag), shas),
    }


def valid_retained_asset(asset: Any) -> bool:
    return (
        isinstance(asset, dict)
        and isinstance(asset.get("url"), str)
        and bool(asset["url"].strip())
        and isinstance(asset.get("sha256"), str)
        and re.fullmatch(r"[0-9a-fA-F]{64}", asset["sha256"]) is not None
    )


def retained_build_has_verifiable_assets(build: Any) -> bool:
    if not isinstance(build, dict):
        return False
    assets = build.get("assets")
    return (
        isinstance(assets, dict)
        and set(assets) == set(ASSET_TARGETS)
        and all(valid_retained_asset(assets.get(target)) for target in ASSET_TARGETS)
    )


def retained_build_has_verifiable_omp_pair(build: Any) -> bool:
    if not retained_build_has_verifiable_assets(build):
        return False
    omp = build.get("omp")
    if not isinstance(omp, dict):
        return False
    for field in ("build_id", "version"):
        value = omp.get(field)
        if (
            not isinstance(value, str)
            or not value.strip()
            or "\n" in value
            or "\r" in value
        ):
            return False
    for field in ("commit", "tree"):
        value = omp.get(field)
        if (
            not isinstance(value, str)
            or re.fullmatch(r"[0-9a-fA-F]{40}", value) is None
        ):
            return False
    omp_assets = omp.get("assets")
    if not isinstance(omp_assets, dict) or set(omp_assets) != set(OMP_ASSET_TARGETS):
        return False
    return all(
        valid_retained_asset(omp_assets.get(target)) for target in OMP_ASSET_TARGETS
    )


def retained_build_is_verifiable(build: Any) -> bool:
    return retained_build_has_verifiable_assets(build) and (
        "omp" not in build or retained_build_has_verifiable_omp_pair(build)
    )


def validate_current_archive(
    manifest: dict[str, Any], expected_tag: str
) -> dict[str, Any]:
    manifest = _mapping(manifest, "preview manifest")
    build_id = _one_line(manifest.get("build_id"), "preview manifest build_id")
    expected_tag = _one_line(expected_tag, "preview manifest tag")
    builds = _mapping(manifest.get("builds"), "preview manifest builds")
    current = _mapping(
        builds.get(build_id), f"preview manifest current archive {build_id}"
    )
    fields = ("base_version", "commit", "built_at", "protocol", "assets")
    if any(field not in manifest for field in fields):
        raise ValueError("preview manifest is missing a current top-level field")
    expected = {field: manifest[field] for field in fields}
    expected["tag"] = expected_tag
    if "omp" in manifest:
        expected["omp"] = manifest["omp"]
    if current != expected:
        raise ValueError("preview manifest top-level/current archive mismatch")
    return manifest


CHANNEL_MANIFEST_KEYS = {
    "assets",
    "base_version",
    "build_id",
    "builds",
    "built_at",
    "channel",
    "commit",
    "notes",
    "omp",
    "protocol",
    "schema_version",
}
CHANNEL_BUILD_KEYS = {
    "assets",
    "base_version",
    "built_at",
    "commit",
    "omp",
    "protocol",
    "tag",
}
LEGACY_BUILD_ID_RE = re.compile(
    r"^(?P<day>\d{4}-\d{2}-\d{2})-(?P<commit>[0-9a-f]{12})$"
)


def _validate_channel_asset(
    value: Any, *, tag: str, name: str, zipped: bool = False
) -> dict[str, str]:
    asset = _mapping(value, f"channel asset {name}")
    expected_keys = {"sha256", "url"} | ({"format"} if zipped else set())
    _exact_keys(asset, expected_keys, f"channel asset {name}")
    expected_url = _release_asset_url(HERDR_REPOSITORY, tag, name)
    if asset.get("url") != expected_url:
        raise ValueError(f"channel asset {name} URL mismatch")
    _sha256(asset.get("sha256"), f"channel asset {name} sha256")
    if zipped and asset.get("format") != "zip":
        raise ValueError(f"channel asset {name} format mismatch")
    return asset


def _validate_channel_assets(
    value: Any, *, tag: str, omp: bool = False
) -> dict[str, Any]:
    assets = _mapping(value, "channel OMP assets" if omp else "channel Herdr assets")
    names = OMP_EXPECTED_ASSET_NAMES if omp else EXPECTED_ASSET_NAMES
    _exact_keys(
        assets, set(names), "channel OMP assets" if omp else "channel Herdr assets"
    )
    for platform, name in names.items():
        _validate_channel_asset(
            assets[platform],
            tag=tag,
            name=name,
            zipped=name.endswith(".zip"),
        )
    return assets


def _validate_channel_omp(
    value: Any, *, tag: str, expected_commit: str | None = None
) -> dict[str, Any]:
    omp = _mapping(value, "channel OMP identity")
    _exact_keys(
        omp, {"assets", "build_id", "commit", "tree", "version"}, "channel OMP identity"
    )
    _one_line(omp.get("build_id"), "channel OMP build_id")
    commit = _git_object(omp.get("commit"), "channel OMP commit")
    _git_object(omp.get("tree"), "channel OMP tree")
    _one_line(omp.get("version"), "channel OMP version")
    if expected_commit is not None and commit != expected_commit:
        raise ValueError("channel OMP commit does not match paired build ID")
    _validate_channel_assets(omp.get("assets"), tag=tag, omp=True)
    return omp


def validate_retained_channel_build(build_id: str, value: Any) -> dict[str, Any]:
    build_id = _one_line(build_id, "channel retained build ID")
    build = _mapping(value, f"channel retained build {build_id}")
    if set(build) not in (CHANNEL_BUILD_KEYS, CHANNEL_BUILD_KEYS - {"omp"}):
        raise ValueError(f"channel retained build {build_id} has unexpected fields")
    tag = f"smarty-preview-{build_id}"
    if build.get("tag") != tag:
        raise ValueError(f"channel retained build {build_id} tag mismatch")
    commit = _git_object(
        build.get("commit"), f"channel retained build {build_id} commit"
    )
    paired = PAIRED_BUILD_ID_RE.fullmatch(build_id)
    legacy = LEGACY_BUILD_ID_RE.fullmatch(build_id)
    if paired is None and legacy is None:
        raise ValueError(f"channel retained build {build_id} has an invalid ID")
    day = (paired or legacy).group("day")
    try:
        datetime.strptime(day, "%Y-%m-%d")
    except ValueError as error:
        raise ValueError(
            f"channel retained build {build_id} has an invalid date"
        ) from error
    if paired is not None:
        if commit != paired.group("herdr"):
            raise ValueError(f"channel retained build {build_id} commit mismatch")
        if "omp" not in build:
            raise ValueError(
                f"channel retained build {build_id} has no paired OMP identity"
            )
    elif commit[:12] != legacy.group("commit"):
        raise ValueError(f"channel retained build {build_id} legacy commit mismatch")
    _one_line(
        build.get("base_version"), f"channel retained build {build_id} base_version"
    )
    built_at_label = f"channel retained build {build_id} built_at"
    source_built_at = _one_line(build.get("built_at"), built_at_label)
    built_at = _timestamp(source_built_at, built_at_label)
    if legacy is not None and legacy.group("day") != source_built_at[:10]:
        raise ValueError(
            f"channel retained build {build_id} date does not match built_at"
        )
    if paired is not None:
        if source_built_at != built_at:
            raise ValueError(
                f"channel retained build {build_id} built_at is not canonical UTC Z"
            )
        if paired.group("day") != built_at[:10]:
            raise ValueError(
                f"channel retained build {build_id} date does not match built_at"
            )
    _protocol(build.get("protocol"))
    _validate_channel_assets(build.get("assets"), tag=tag)
    if "omp" in build:
        _validate_channel_omp(
            build["omp"],
            tag=tag,
            expected_commit=paired.group("omp") if paired is not None else None,
        )
    return build


def _validate_channel_manifest(value: Any) -> tuple[dict[str, Any], dict[str, Any]]:
    manifest = _mapping(value, "channel manifest")
    _exact_keys(manifest, CHANNEL_MANIFEST_KEYS, "channel manifest")
    if manifest.get("schema_version") != 1 or manifest.get("channel") != "preview":
        raise ValueError("channel manifest schema or channel mismatch")
    build_id = _one_line(manifest.get("build_id"), "channel manifest build_id")
    builds = _mapping(manifest.get("builds"), "channel manifest builds")
    current = validate_retained_channel_build(build_id, builds.get(build_id))
    expected = {
        "assets": manifest.get("assets"),
        "base_version": manifest.get("base_version"),
        "built_at": manifest.get("built_at"),
        "commit": manifest.get("commit"),
        "omp": manifest.get("omp"),
        "protocol": manifest.get("protocol"),
        "tag": f"smarty-preview-{build_id}",
    }
    if current != expected:
        raise ValueError("channel manifest top-level/current archive mismatch")
    if not isinstance(manifest.get("notes"), str):
        raise ValueError("channel manifest notes must be a string")
    for retained_id, retained in builds.items():
        validate_retained_channel_build(retained_id, retained)
    return manifest, current


def validate_channel_transition(
    previous: dict[str, Any] | None,
    candidate: dict[str, Any],
    *,
    expected_parent: str,
    expected_source: str,
    expected_omp: str,
    expected_omp_tree: str,
    expected_omp_version: str,
    expected_omp_build_id: str,
    expected_tag: str,
    expected_build_id: str,
    expected_built_at: str,
    expected_base_version: str,
    expected_protocol: int,
    expected_herdr_shas: dict[str, str],
    expected_omp_shas: dict[str, str],
    consumed: bool = False,
    retain: int = 30,
) -> dict[str, Any]:
    if retain < 1:
        raise ValueError("channel retention must be positive")
    expected_parent = _git_object(expected_parent, "expected channel parent")
    expected_source = _git_object(expected_source, "expected channel Herdr source")
    expected_omp = _git_object(expected_omp, "expected channel OMP source")
    expected_omp_tree = _git_object(expected_omp_tree, "expected channel OMP tree")
    expected_build_id = _validate_paired_build_id(
        expected_build_id,
        expected_parent,
        expected_source,
        expected_omp,
        expected_built_at,
    )
    expected_tag = _one_line(expected_tag, "expected channel tag")
    if expected_tag != f"smarty-preview-{expected_build_id}":
        raise ValueError("expected channel tag/build ID mismatch")
    expected_built_at = _timestamp(expected_built_at, "expected channel built_at")
    expected_base_version = normalize_version(
        _one_line(expected_base_version, "expected channel base_version")
    )
    expected_protocol = _protocol(expected_protocol)
    _exact_keys(
        expected_herdr_shas, set(ASSET_TARGETS), "expected Herdr channel digests"
    )
    _exact_keys(
        expected_omp_shas, set(OMP_ASSET_TARGETS), "expected OMP channel digests"
    )
    for platform, digest in expected_herdr_shas.items():
        _sha256(digest, f"expected Herdr channel digest {platform}")
    for platform, digest in expected_omp_shas.items():
        _sha256(digest, f"expected OMP channel digest {platform}")

    candidate, current = _validate_channel_manifest(candidate)
    if (
        candidate["build_id"] != expected_build_id
        or candidate["commit"] != expected_source
        or candidate["built_at"] != expected_built_at
        or candidate["base_version"] != expected_base_version
        or candidate["protocol"] != expected_protocol
        or current["tag"] != expected_tag
    ):
        raise ValueError("candidate channel current identity mismatch")
    omp = current["omp"]
    if (
        omp["commit"] != expected_omp
        or omp["tree"] != expected_omp_tree
        or omp["version"] != expected_omp_version
        or omp["build_id"] != expected_omp_build_id
    ):
        raise ValueError("candidate channel OMP identity mismatch")
    if {
        platform: current["assets"][platform]["sha256"] for platform in ASSET_TARGETS
    } != expected_herdr_shas:
        raise ValueError("candidate channel Herdr digests mismatch")
    if {
        platform: omp["assets"][platform]["sha256"] for platform in OMP_ASSET_TARGETS
    } != expected_omp_shas:
        raise ValueError("candidate channel OMP digests mismatch")

    candidate_builds = candidate["builds"]
    if previous is None:
        if list(candidate_builds) != [expected_build_id]:
            raise ValueError("initial channel candidate has unexpected history")
        if consumed:
            raise ValueError("an absent channel cannot have consumed authorization")
        return candidate

    previous, _ = _validate_channel_manifest(previous)
    if consumed:
        if candidate != previous:
            raise ValueError(
                "consumed channel candidate differs from authenticated snapshot"
            )
        return candidate
    previous_builds = previous["builds"]
    if expected_build_id in previous_builds:
        raise ValueError("new channel build already exists in authenticated history")
    ordered_previous = sorted(
        previous_builds,
        key=lambda key: (str(previous_builds[key]["built_at"]), key),
        reverse=True,
    )
    previous_current = previous["build_id"]
    try:
        keep = max(retain - 1, ordered_previous.index(previous_current) + 1)
    except ValueError as error:
        raise ValueError(
            "authenticated channel current build is absent from history"
        ) from error
    expected_history = ordered_previous[:keep]
    if list(candidate_builds) != [expected_build_id, *expected_history]:
        raise ValueError(
            "candidate channel history is not the deterministic retained prefix"
        )
    for retained_id in expected_history:
        before = json.dumps(
            previous_builds[retained_id], sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        after = json.dumps(
            candidate_builds[retained_id], sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
        if after != before:
            raise ValueError(
                f"candidate channel changed historical build {retained_id}"
            )
    return candidate


def build_manifest(
    output: Path,
    repo: str,
    tag: str,
    build_id: str,
    commit: str,
    built_at: str,
    base_version: str,
    protocol: int,
    notes: str,
    shas: dict[str, str],
    retain: int,
    omp_source: dict[str, str] | None = None,
    omp_shas: dict[str, str] | None = None,
) -> str:
    if retain < 1:
        raise ValueError("retain must be at least 1")
    built_at = _timestamp(built_at, "built_at")
    if (omp_source is None) != (omp_shas is None):
        raise ValueError(
            "OMP source descriptor and SHA targets must be provided together"
        )
    urls = default_asset_urls(repo, tag)
    assets = asset_objects(urls, shas)
    omp = None
    if omp_source is not None and omp_shas is not None:
        omp = omp_metadata(omp_source, repo, tag, omp_shas)
        build_id = _one_line(build_id, "build_id")
        paired = PAIRED_BUILD_ID_RE.fullmatch(build_id)
        if paired is not None:
            build_id = _validate_paired_build_id(
                build_id, paired.group("parent"), commit, omp["commit"], built_at
            )
            if tag != f"smarty-preview-{build_id}":
                raise ValueError("paired channel tag must namespace build_id")
    current = read_json(output) or {}
    existing_builds = current.get("builds", {})
    builds = (
        {
            key: build
            for key, build in existing_builds.items()
            if retained_build_is_verifiable(build)
        }
        if isinstance(existing_builds, dict)
        else {}
    )
    build: dict[str, Any] = {
        "base_version": normalize_version(base_version),
        "commit": commit,
        "built_at": built_at,
        "protocol": protocol,
        "tag": tag,
        "assets": assets,
    }
    if omp is not None:
        build["omp"] = omp
    builds[build_id] = build
    ordered = sorted(
        (key for key in builds if key != build_id),
        key=lambda key: (str(builds[key].get("built_at", "")), key),
        reverse=True,
    )
    keep = retain - 1
    previous_current = current.get("build_id")
    if isinstance(previous_current, str) and previous_current in ordered:
        keep = max(keep, ordered.index(previous_current) + 1)
    historical = ordered[:keep]
    ordered_builds = {build_id: build, **{key: builds[key] for key in historical}}
    manifest = {
        "schema_version": 1,
        "channel": "preview",
        "base_version": normalize_version(base_version),
        "build_id": build_id,
        "commit": commit,
        "built_at": built_at,
        "protocol": protocol,
        "notes": notes.strip(),
        "assets": assets,
    }
    if omp is not None:
        manifest["omp"] = omp
    manifest["builds"] = ordered_builds
    validate_current_archive(manifest, tag)
    return json.dumps(manifest, indent=2) + "\n"


def legacy_bootstrap_build_id(paired_build_id: str) -> str:
    paired_build_id = _one_line(paired_build_id, "paired bootstrap build_id")
    if PAIRED_BUILD_ID_RE.fullmatch(paired_build_id) is None:
        raise ValueError("paired bootstrap build_id must be a full P/R/O identity")
    return "bootstrap-" + hashlib.sha256(paired_build_id.encode("ascii")).hexdigest()


def build_legacy_bootstrap_manifest(paired_manifest: dict[str, Any]) -> str:
    paired, current = _validate_channel_manifest(paired_manifest)
    paired_build_id = paired["build_id"]
    alias = legacy_bootstrap_build_id(paired_build_id)
    tag = current["tag"]
    windows = current["assets"]["windows-x86_64"]
    manifest = {
        **{
            key: value
            for key, value in paired.items()
            if key not in {"assets", "build_id", "builds", "schema_version"}
        },
        "schema_version": 2,
        "build_id": alias,
        "canonical_build_id": paired_build_id,
        "assets": current["assets"],
        "bootstrap": {
            "schema": LEGACY_BOOTSTRAP_SCHEMA,
            "paired_build_id": paired_build_id,
            "paired_tag": tag,
            "paired_manifest": "preview.json",
            "windows_asset_sha256": windows["sha256"],
        },
        "builds": paired["builds"],
    }
    validate_legacy_bootstrap_manifest(manifest, paired)
    return json.dumps(manifest, indent=2) + "\n"


def validate_legacy_bootstrap_manifest(
    value: Any, paired_manifest: Any
) -> dict[str, Any]:
    legacy = _mapping(value, "legacy bootstrap manifest")
    paired, current = _validate_channel_manifest(paired_manifest)
    _exact_keys(
        legacy,
        CHANNEL_MANIFEST_KEYS | {"bootstrap", "canonical_build_id"},
        "legacy bootstrap manifest",
    )
    alias = _one_line(legacy.get("build_id"), "legacy bootstrap build_id")
    paired_build_id = paired["build_id"]
    if LEGACY_BOOTSTRAP_ID_RE.fullmatch(
        alias
    ) is None or alias != legacy_bootstrap_build_id(paired_build_id):
        raise ValueError("legacy bootstrap build_id does not bind the paired build")
    if legacy.get("schema_version") != 2 or legacy.get("channel") != "preview":
        raise ValueError("legacy bootstrap schema or channel mismatch")
    if legacy.get("canonical_build_id") != paired_build_id:
        raise ValueError("legacy bootstrap canonical build_id mismatch")
    binding = _mapping(legacy.get("bootstrap"), "legacy bootstrap binding")
    expected_binding = {
        "schema": LEGACY_BOOTSTRAP_SCHEMA,
        "paired_build_id": paired_build_id,
        "paired_tag": current["tag"],
        "paired_manifest": "preview.json",
        "windows_asset_sha256": current["assets"]["windows-x86_64"]["sha256"],
    }
    if binding != expected_binding:
        raise ValueError("legacy bootstrap binding does not match paired manifest")
    for field in CHANNEL_MANIFEST_KEYS - {
        "assets",
        "build_id",
        "builds",
        "schema_version",
    }:
        if legacy.get(field) != paired.get(field):
            raise ValueError(f"legacy bootstrap {field} differs from paired manifest")
    if legacy.get("assets") != current["assets"]:
        raise ValueError("legacy bootstrap top-level assets differ from paired manifest")
    builds = _mapping(legacy.get("builds"), "legacy bootstrap builds")
    if builds != paired["builds"]:
        raise ValueError("legacy bootstrap archive differs from paired manifest")
    return legacy


def canonical_manifest_from_legacy_bootstrap(value: Any) -> dict[str, Any]:
    legacy = _mapping(value, "legacy bootstrap manifest")
    canonical_build_id = _one_line(
        legacy.get("canonical_build_id"), "legacy bootstrap canonical build_id"
    )
    builds = _mapping(legacy.get("builds"), "legacy bootstrap builds")
    current = validate_retained_channel_build(
        canonical_build_id, builds.get(canonical_build_id)
    )
    canonical = {
        "assets": current["assets"],
        "base_version": current["base_version"],
        "build_id": canonical_build_id,
        "builds": builds,
        "built_at": current["built_at"],
        "channel": "preview",
        "commit": current["commit"],
        "notes": legacy.get("notes"),
        "omp": current["omp"],
        "protocol": current["protocol"],
        "schema_version": 1,
    }
    canonical, _ = _validate_channel_manifest(canonical)
    validate_legacy_bootstrap_manifest(legacy, canonical)
    return canonical


def validate_bootstrap_promotion(value: Any, candidate: Any) -> dict[str, Any]:
    expected = canonical_manifest_from_legacy_bootstrap(value)
    candidate, _ = _validate_channel_manifest(candidate)
    if candidate != expected:
        raise ValueError("bootstrap promotion changes canonical channel state")
    return candidate


def _mapping(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{label} must be a JSON object")
    return value


def _exact_keys(data: dict[str, Any], expected: set[str], label: str) -> None:
    if set(data) != expected:
        raise ValueError(f"{label} must contain exactly: {', '.join(sorted(expected))}")


def _one_line(value: Any, label: str) -> str:
    if (
        not isinstance(value, str)
        or not value
        or value != value.strip()
        or "\n" in value
        or "\r" in value
    ):
        raise ValueError(f"{label} must be a nonempty one-line string")
    return value


def _repository(value: Any, label: str) -> str:
    value = _one_line(value, label)
    if re.fullmatch(r"[^/\s]+/[^/\s]+", value) is None:
        raise ValueError(f"{label} must be owner/name")
    return value


def _git_object(value: Any, label: str) -> str:
    value = _one_line(value, label)
    if re.fullmatch(r"[0-9a-f]{40}", value) is None:
        raise ValueError(f"{label} must be a lowercase 40-character Git object ID")
    return value


def pair_id_for_sources(parent_commit: str, herdr_commit: str, omp_commit: str) -> str:
    commits = (
        _git_object(parent_commit, "parent_commit"),
        _git_object(herdr_commit, "herdr_commit"),
        _git_object(omp_commit, "omp_commit"),
    )
    material = PAIR_ID_DOMAIN + b"\0".join(commit.encode("ascii") for commit in commits)
    return hashlib.sha256(material).hexdigest()


def paired_build_id(
    built_at: str, parent_commit: str, herdr_commit: str, omp_commit: str
) -> str:
    day = _timestamp(built_at, "built_at")[:10]
    parent_commit = _git_object(parent_commit, "parent_commit")
    herdr_commit = _git_object(herdr_commit, "herdr_commit")
    omp_commit = _git_object(omp_commit, "omp_commit")
    return f"{day}-p{parent_commit}-r{herdr_commit}-o{omp_commit}"


def _validate_paired_build_id(
    build_id: Any,
    parent_commit: str,
    herdr_commit: str,
    omp_commit: str,
    built_at: Any | None = None,
) -> str:
    build_id = _one_line(build_id, "build_id")
    match = PAIRED_BUILD_ID_RE.fullmatch(build_id)
    if match is None:
        raise ValueError("build_id must encode the full exact P/R/O tuple")
    try:
        datetime.strptime(match.group("day"), "%Y-%m-%d")
    except ValueError as error:
        raise ValueError("build_id must contain a valid build date") from error
    expected = {
        "parent": _git_object(parent_commit, "parent_commit"),
        "herdr": _git_object(herdr_commit, "herdr_commit"),
        "omp": _git_object(omp_commit, "omp_commit"),
    }
    if any(match.group(name) != value for name, value in expected.items()):
        raise ValueError("build_id P/R/O identity mismatch")
    if built_at is not None and match.group("day") != _timestamp(
        built_at, "build timestamp"
    )[:10]:
        raise ValueError("build_id date must match canonical built_at date")
    return build_id


def _sha256(value: Any, label: str) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{64}", value) is None:
        raise ValueError(f"{label} must be a lowercase SHA-256 digest")
    return value


def _sha1(value: Any, label: str) -> str:
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{40}", value) is None:
        raise ValueError(f"{label} must be a lowercase SHA-1 digest")
    return value


def _length(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{label} must be a nonnegative byte length")
    return value


def _timestamp(value: Any, label: str) -> str:
    value = _one_line(value, label)
    candidate = f"{value[:-1]}+00:00" if value.endswith("Z") else value
    try:
        parsed = datetime.fromisoformat(candidate)
    except ValueError as error:
        raise ValueError(f"{label} must be ISO-8601") from error
    if parsed.tzinfo is None:
        raise ValueError(f"{label} must include a timezone")
    if parsed.microsecond:
        raise ValueError(f"{label} must use whole-second precision")
    return parsed.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _protocol(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        raise ValueError("protocol must be a positive integer")
    return value


def _regular_file(path: Path, label: str) -> Path:
    if not path.is_file() or path.is_symlink():
        raise ValueError(f"{label} must be a regular file")
    return path


def _file_digests(path: Path, label: str) -> dict[str, str]:
    path = _regular_file(path, label)
    sha1 = hashlib.sha1()
    sha256 = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            sha1.update(chunk)
            sha256.update(chunk)
    return {"sha1": sha1.hexdigest(), "sha256": sha256.hexdigest()}


def _file_record(path: Path, label: str) -> dict[str, Any]:
    path = _regular_file(path, label)
    return {
        "length": path.stat().st_size,
        "sha256": _file_digests(path, label)["sha256"],
    }


def _asset_file(asset_dir: Path, name: str) -> Path:
    if not asset_dir.is_dir():
        raise ValueError("asset directory must be a directory")
    return _regular_file(asset_dir / name, f"asset {name}")


def _asset_records(
    asset_dir: Path, names: tuple[str, ...]
) -> dict[str, dict[str, Any]]:
    return {
        name: _file_record(_asset_file(asset_dir, name), f"asset {name}")
        for name in names
    }


def _directory_file_records(
    directory: Path, names: tuple[str, ...], label: str
) -> dict[str, dict[str, Any]]:
    directory = Path(directory)
    if not directory.is_dir() or directory.is_symlink():
        raise ValueError(f"{label} must be a directory")
    entries = list(directory.iterdir())
    if {entry.name for entry in entries} != set(names) or len(entries) != len(names):
        raise ValueError(f"{label} file set mismatch")
    return {name: _file_record(directory / name, f"{label} {name}") for name in names}


def _semantic_verification_record(
    *,
    verifier_path: Path,
    source_archive_dir: Path,
    omp_bazel_graph: Path,
    cargo_metadata_dir: Path,
    omp_rules_rust_toolchains: Path,
    spdx_path: Path,
) -> dict[str, Any]:
    cargo_names = tuple(
        name
        for component in CARGO_METADATA_FILENAMES.values()
        for name in component.values()
    )
    return {
        "schema": SEMANTIC_VERIFICATION_SCHEMA,
        "verifier": _file_record(Path(verifier_path), "trusted release verifier"),
        "source_archives": _directory_file_records(
            Path(source_archive_dir), SOURCE_ARCHIVE_NAMES, "trusted source archives"
        ),
        "inputs": {
            "cargo_metadata": _directory_file_records(
                Path(cargo_metadata_dir), cargo_names, "Cargo metadata inputs"
            ),
            "native_graph": _file_record(
                Path(omp_bazel_graph), "OMP Bazel module graph input"
            ),
            "rules_rust_toolchains": _file_record(
                Path(omp_rules_rust_toolchains), "OMP rules_rust toolchain input"
            ),
        },
        "spdx": _file_record(Path(spdx_path), "verified SPDX document"),
    }


def _release_asset_url(repo: str, tag: str, name: str) -> str:
    return f"https://github.com/{repo}/releases/download/{tag}/{name}"


def _canonical_json(data: Any) -> str:
    return json.dumps(data, indent=2, sort_keys=True) + "\n"


def _validate_checksum_sidecars(
    asset_dir: Path, payload_records: dict[str, dict[str, Any]]
) -> None:
    for name in payload_records:
        sidecar = _asset_file(asset_dir, f"{name}.sha256")
        try:
            fields = sidecar.read_text(encoding="utf-8").split()
        except UnicodeDecodeError as error:
            raise ValueError(f"checksum sidecar is not UTF-8: {name}") from error
        if len(fields) != 2 or fields[1] != name:
            raise ValueError(f"checksum sidecar mismatch: {name}")
        if re.fullmatch(r"[0-9a-f]{64}", fields[0]) is None:
            raise ValueError(f"checksum sidecar digest is invalid: {name}")
        if fields[0] != payload_records[name]["sha256"]:
            raise ValueError(f"checksum sidecar digest mismatch: {name}")


def _toolchain_channel(path: Path, label: str) -> str:
    text = _regular_file(path, label).read_text(encoding="utf-8")
    matches = re.findall(r'^\s*channel\s*=\s*"([^"]+)"\s*$', text, re.MULTILINE)
    if len(matches) != 1:
        raise ValueError(f"{label} must declare exactly one Rust channel")
    return _one_line(matches[0], label)


def _one_line_file(path: Path, label: str) -> str:
    value = _regular_file(path, label).read_text(encoding="utf-8").strip()
    return _one_line(value, label)


def build_rules_rust_toolchain_report(log: str, platform: str) -> str:
    platform = _one_line(platform, "rules_rust report platform")
    if platform not in OMP_ASSET_TARGETS:
        raise ValueError("rules_rust report platform is not an OMP release target")
    pattern = re.compile(
        r"ToolchainResolution:\s+Target platform .*?:\s+Selected execution platform .*?,\s+"
        r"type\s+@@rules_rust\+//rust:toolchain_type\s+->\s+toolchain\s+"
        r"(\S+//:rust_toolchain)"
    )
    resolved = sorted(
        {
            match.group(1)
            for match in pattern.finditer(log)
            if match.group(1).startswith("@@rules_rust")
        }
    )
    if not resolved:
        raise ValueError("Bazel 9.2 did not report a selected rules_rust toolchain")
    return _canonical_json(
        {
            "platform": platform,
            "resolved": resolved,
            "schema": 1,
            "toolchain_type": "@@rules_rust+//rust:toolchain_type",
        }
    )


def _declaration_record(root: Path, path: str, label: str) -> dict[str, Any]:
    return {"path": path, **_file_record(root / path, label)}


def _rules_rust_declaration(root: Path) -> tuple[str, dict[str, Any]]:
    path = root / "MODULE.bazel"
    text = _regular_file(path, "OMP MODULE.bazel").read_text(encoding="utf-8")
    matches = re.findall(
        r'^\s*bazel_dep\(name\s*=\s*"rules_rust",\s*version\s*=\s*"([^"]+)"\)\s*$',
        text,
        re.MULTILINE,
    )
    if len(matches) != 1:
        raise ValueError("OMP MODULE.bazel must declare rules_rust exactly once")
    return _one_line(matches[0], "OMP rules_rust version"), _declaration_record(
        root, "MODULE.bazel", "OMP MODULE.bazel"
    )


def _read_rules_rust_toolchains(path: Path, omp_root: Path) -> dict[str, Any]:
    report = _mapping(
        _json_file(Path(path), "OMP rules_rust toolchains"), "OMP rules_rust toolchains"
    )
    _exact_keys(report, {"schema", "toolchains"}, "OMP rules_rust toolchains")
    if report.get("schema") != 1:
        raise ValueError("OMP rules_rust toolchains schema mismatch")
    toolchains = _mapping(report["toolchains"], "OMP rules_rust toolchains")
    _exact_keys(toolchains, set(OMP_ASSET_TARGETS), "OMP rules_rust toolchains")
    toolchain_type = "@@rules_rust+//rust:toolchain_type"
    resolved: dict[str, list[str]] = {}
    for platform in OMP_ASSET_TARGETS:
        record = _mapping(toolchains[platform], f"OMP rules_rust toolchain {platform}")
        _exact_keys(
            record,
            {"toolchain_type", "resolved"},
            f"OMP rules_rust toolchain {platform}",
        )
        if record.get("toolchain_type") != toolchain_type:
            raise ValueError(f"OMP rules_rust toolchain type mismatch: {platform}")
        labels = record.get("resolved")
        if (
            not isinstance(labels, list)
            or not labels
            or labels != sorted(set(labels))
            or any(
                not isinstance(label, str)
                or "\r" in label
                or "\n" in label
                or not label.startswith("@@rules_rust")
                or not label.endswith("//:rust_toolchain")
                for label in labels
            )
        ):
            raise ValueError(f"OMP rules_rust toolchain selection mismatch: {platform}")
        resolved[platform] = labels
    version, declaration = _rules_rust_declaration(omp_root)
    return {
        "version": version,
        "declaration": declaration,
        "toolchain_type": toolchain_type,
        "resolved": resolved,
    }


def _lock_input_records(herdr_root: Path, omp_root: Path) -> dict[str, dict[str, Any]]:
    roots = {"herdr": herdr_root, "omp": omp_root}
    records: dict[str, dict[str, Any]] = {}
    for component, relative_path in LOCK_INPUTS:
        record = _file_record(
            roots[component] / relative_path,
            f"{component} lock input {relative_path}",
        )
        records[f"{component}/{relative_path}"] = {
            "component": component,
            "path": relative_path,
            **record,
        }
    return records


def _platform_manifest() -> dict[str, dict[str, Any]]:
    return {
        target: {
            "os": data["os"],
            "architecture": data["architecture"],
            "abi": data["abi"],
            "runner": data["runner"],
            "payloads": dict(data["payloads"]),
        }
        for target, data in PLATFORM_MATRIX.items()
    }


def _release_artifact_records(
    asset_dir: Path, repo: str, tag: str
) -> dict[str, dict[str, Any]]:
    payload_records = _asset_records(asset_dir, RELEASE_PAYLOAD_ASSET_NAMES)
    _validate_checksum_sidecars(asset_dir, payload_records)
    records = _asset_records(asset_dir, RELEASE_ASSET_NAMES)
    artifacts: dict[str, dict[str, Any]] = {}
    for name in RELEASE_ASSET_NAMES:
        if name in PAYLOAD_METADATA_BY_NAME:
            artifacts[name] = {
                **PAYLOAD_METADATA_BY_NAME[name],
                **records[name],
                "url": _release_asset_url(repo, tag, name),
            }
        else:
            artifacts[name] = {
                "kind": "sha256",
                "payload": name.removesuffix(".sha256"),
                **records[name],
                "url": _release_asset_url(repo, tag, name),
            }
    return artifacts


def _evidence_records(
    asset_dir: Path, repo: str, tag: str
) -> dict[str, dict[str, Any]]:
    return {
        name: {
            **_file_record(_asset_file(asset_dir, name), f"evidence {name}"),
            "url": _release_asset_url(repo, tag, name),
        }
        for name in EVIDENCE_ASSET_NAMES
    }


def _spdx_namespace(parent_commit: str, herdr_commit: str, omp_commit: str) -> str:
    return (
        "https://smarty-pants-inc.github.io/spdx/smarty-pair/"
        f"{parent_commit}/{herdr_commit}/{omp_commit}"
    )


def _spdx_package_id(ecosystem: str, identity: str) -> str:
    digest = hashlib.sha256(identity.encode("utf-8")).hexdigest()
    return f"SPDXRef-Package-{ecosystem}-{digest}"


def _spdx_package(
    package_id: str,
    name: str,
    version: str,
    source_info: str,
    *,
    license_declared: str = "NOASSERTION",
    download_location: str = "NOASSERTION",
    checksums: list[dict[str, str]] | None = None,
    comment: str | None = None,
) -> dict[str, Any]:
    package = {
        "SPDXID": package_id,
        "copyrightText": "NOASSERTION",
        "downloadLocation": download_location,
        "filesAnalyzed": False,
        "licenseConcluded": "NOASSERTION",
        "licenseDeclared": license_declared,
        "name": _one_line(name, "SPDX package name"),
        "sourceInfo": _one_line(source_info, "SPDX package sourceInfo"),
        "versionInfo": _one_line(version, "SPDX package version"),
    }
    if checksums:
        package["checksums"] = checksums
    if comment:
        package["comment"] = _one_line(comment, "SPDX package comment")
    return package


def _add_spdx_package(
    packages: dict[str, dict[str, Any]], package: dict[str, Any]
) -> None:
    package_id = package["SPDXID"]
    previous = packages.get(package_id)
    if previous is not None and previous != package:
        raise ValueError(f"SPDX package identity collision: {package_id}")
    packages[package_id] = package


def _read_toml(path: Path, label: str) -> dict[str, Any]:
    try:
        value = tomllib.loads(_regular_file(path, label).read_text(encoding="utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
        raise ValueError(f"{label} must be valid TOML") from error
    return _mapping(value, label)


def _cargo_lock_packages(
    root: Path, component: str
) -> dict[tuple[str, str, str], dict[str, Any]]:
    lock = _read_toml(root / "Cargo.lock", f"{component} Cargo.lock")
    raw_packages = lock.get("package")
    if not isinstance(raw_packages, list):
        raise ValueError(f"{component} Cargo.lock has no package list")
    packages: dict[tuple[str, str, str], dict[str, Any]] = {}
    for raw in raw_packages:
        package = _mapping(raw, f"{component} Cargo.lock package")
        name = _one_line(package.get("name"), f"{component} Cargo package name")
        version = _one_line(
            package.get("version"), f"{component} Cargo package version"
        )
        source = package.get("source")
        if source is not None:
            source = _one_line(source, f"{component} Cargo package source")
        raw_dependencies = package.get("dependencies", [])
        if not isinstance(raw_dependencies, list):
            raise ValueError(f"{component} Cargo package dependencies must be a list")
        dependencies = [
            _one_line(value, f"{component} Cargo package dependency")
            for value in raw_dependencies
        ]
        if len(dependencies) != len(set(dependencies)):
            raise ValueError(f"{component} Cargo package repeats a dependency")
        checksum = package.get("checksum")
        if checksum is not None:
            checksum = _sha256(checksum, f"{component} Cargo package checksum")
        identity = (name, version, source or "")
        record = {"checksum": checksum, "dependencies": tuple(sorted(dependencies))}
        previous = packages.get(identity)
        if previous is not None and previous != record:
            raise ValueError(f"{component} Cargo.lock repeats package {name} {version}")
        packages[identity] = record
    return packages


def _cargo_lock_dependency(
    value: str,
    packages: dict[tuple[str, str, str], dict[str, Any]],
    component: str,
) -> tuple[str, str, str]:
    match = re.fullmatch(r"([^ ]+)(?: ([^ ()]+))?(?: \(([^\r\n]+)\))?", value)
    if match is None:
        raise ValueError(f"{component} Cargo.lock dependency is invalid: {value}")
    name, version, source = match.groups()
    candidates = [
        identity
        for identity in packages
        if identity[0] == name
        and (version is None or identity[1] == version)
        and (source is None or identity[2] == source)
    ]
    if len(candidates) != 1:
        raise ValueError(
            f"{component} Cargo.lock dependency is not uniquely resolved: {value}"
        )
    return candidates[0]


def _cargo_lock_dependency_graph(
    root: Path,
    component: str,
    root_name: str,
    root_spdx_id: str | None,
) -> tuple[
    list[dict[str, Any]],
    set[tuple[str, str, str]],
    str,
    str,
    dict[tuple[str, str, str], set[str]],
]:
    root = Path(root)
    packages = _cargo_lock_packages(root, component)
    roots = [
        identity
        for identity in packages
        if identity[0] == root_name and not identity[2]
    ]
    if len(roots) != 1:
        raise ValueError(
            f"{component} Cargo.lock must contain one path root {root_name}"
        )
    root_identity = roots[0]
    manifest_path = root / (
        "Cargo.toml" if component == "herdr" else "crates/pi-natives/Cargo.toml"
    )
    root_package = _mapping(
        _read_toml(manifest_path, f"{component} root Cargo.toml").get("package"),
        f"{component} root Cargo package",
    )
    if (
        root_package.get("name") != root_identity[0]
        or root_package.get("version") != root_identity[1]
    ):
        raise ValueError(
            f"{component} Cargo.lock root does not match its source manifest"
        )
    root_license = (
        _one_line(root_package["license"], f"{component} root Cargo license")
        if isinstance(root_package.get("license"), str)
        else "NOASSERTION"
    )

    ids: dict[tuple[str, str, str], str] = {}

    def package_id(identity: tuple[str, str, str]) -> str:
        if identity == root_identity and root_spdx_id is not None:
            return root_spdx_id
        if identity not in ids:
            name, version, source = identity
            material = f"cargo\0{name}\0{version}\0{source or f'path:{component}'}"
            ids[identity] = _spdx_package_id("cargo", material)
        return ids[identity]

    reachable: set[tuple[str, str, str]] = set()
    relationships: set[tuple[str, str, str]] = set()
    pending = [root_identity]
    while pending:
        identity = pending.pop()
        if identity in reachable:
            continue
        reachable.add(identity)
        for dependency in packages[identity]["dependencies"]:
            target = _cargo_lock_dependency(dependency, packages, component)
            relationships.add((package_id(identity), "DEPENDS_ON", package_id(target)))
            if target not in reachable:
                pending.append(target)

    spdx_packages: list[dict[str, Any]] = []
    for identity in sorted(reachable):
        if identity == root_identity and root_spdx_id is not None:
            continue
        name, version, source = identity
        checksum = packages[identity]["checksum"]
        spdx_packages.append(
            _spdx_package(
                package_id(identity),
                name,
                version,
                (
                    f"Cargo.lock package source={source}"
                    if source
                    else f"Cargo.lock path package in authenticated {component} source"
                ),
                license_declared=root_license
                if identity == root_identity
                else "NOASSERTION",
                checksums=(
                    [{"algorithm": "SHA256", "checksumValue": checksum}]
                    if checksum is not None
                    else None
                ),
            )
        )
    return (
        spdx_packages,
        relationships,
        package_id(root_identity),
        root_identity[1],
        {},
    )


def _cargo_metadata_package_identity(
    package: dict[str, Any], component: str
) -> tuple[str, str, str]:
    name = _one_line(package.get("name"), f"{component} Cargo metadata package name")
    version = _one_line(
        package.get("version"), f"{component} Cargo metadata package version"
    )
    source = package.get("source")
    if source is not None:
        source = _one_line(source, f"{component} Cargo metadata package source")
    return name, version, source or ""


def validate_cargo_metadata_directory(directory: Path) -> Path:
    directory = Path(directory)
    if not directory.is_dir() or directory.is_symlink():
        raise ValueError("Cargo metadata directory must be a directory")
    expected = {
        name
        for filenames in CARGO_METADATA_FILENAMES.values()
        for name in filenames.values()
    }
    entries = list(directory.iterdir())
    if {entry.name for entry in entries} != expected or len(entries) != len(expected):
        raise ValueError("Cargo metadata files must exactly match all release targets")
    for entry in entries:
        _regular_file(entry, f"Cargo metadata {entry.name}")
    return directory


def _cargo_metadata_graphs(
    directory: Path, component: str
) -> list[tuple[str, str, dict[str, Any]]]:
    graphs: list[tuple[str, str, dict[str, Any]]] = []
    for platform, target in CARGO_METADATA_TARGETS[component].items():
        name = CARGO_METADATA_FILENAMES[component][platform]
        graph = _mapping(
            _json_file(Path(directory) / name, f"{component} Cargo metadata {name}"),
            f"{component} Cargo metadata {name}",
        )
        if graph.get("version") != 1:
            raise ValueError(
                f"{component} Cargo metadata {name} has the wrong format version"
            )
        graphs.append((platform, target, graph))
    return graphs


def _cargo_metadata_relative_manifest(
    manifest_path: Any, workspace_root: Any, component: str
) -> str:
    manifest = _one_line(
        manifest_path, f"{component} Cargo metadata manifest path"
    ).replace("\\", "/")
    workspace = _one_line(
        workspace_root, f"{component} Cargo metadata workspace root"
    ).replace("\\", "/")
    workspace = workspace.rstrip("/") or "/"
    prefix = workspace if workspace.endswith("/") else f"{workspace}/"
    if not manifest.startswith(prefix):
        raise ValueError(
            f"{component} path package manifest is outside its metadata workspace"
        )
    relative = manifest[len(prefix) :]
    parts = relative.split("/")
    if (
        not parts
        or parts[-1] != "Cargo.toml"
        or any(part in ("", ".", "..") for part in parts)
    ):
        raise ValueError(f"{component} Cargo metadata manifest path is invalid")
    return relative


def _cargo_dependency_graph(
    root: Path,
    component: str,
    root_name: str,
    root_spdx_id: str | None,
    metadata_dir: Path,
) -> tuple[
    list[dict[str, Any]],
    set[tuple[str, str, str]],
    str,
    str,
    dict[tuple[str, str, str], set[str]],
]:
    lock_packages = _cargo_lock_packages(Path(root), component)
    package_records: dict[tuple[str, str, str, str], dict[str, Any]] = {}
    graph_nodes: list[
        tuple[
            str,
            str,
            dict[str, Any],
            dict[str, tuple[str, str, str, str]],
        ]
    ] = []
    root_identities: set[tuple[str, str, str, str]] = set()

    for platform, release_target, graph in _cargo_metadata_graphs(
        metadata_dir, component
    ):
        raw_packages = graph.get("packages")
        resolve = graph.get("resolve")
        if not isinstance(raw_packages, list) or not isinstance(resolve, dict):
            raise ValueError(
                f"{component} Cargo metadata {platform} has no resolve graph"
            )
        nodes = resolve.get("nodes")
        if not isinstance(nodes, list):
            raise ValueError(
                f"{component} Cargo metadata {platform} nodes must be a list"
            )
        workspace_root = graph.get("workspace_root")
        ids_for_graph: dict[str, tuple[str, str, str, str]] = {}
        for raw_package in raw_packages:
            package = _mapping(raw_package, f"{component} Cargo metadata package")
            metadata_id = _one_line(
                package.get("id"), f"{component} Cargo metadata package id"
            )
            if metadata_id in ids_for_graph:
                raise ValueError(
                    f"{component} Cargo metadata repeats package id {metadata_id}"
                )
            name, version, source = _cargo_metadata_package_identity(package, component)
            lock_identity = (name, version, source)
            if lock_identity not in lock_packages:
                raise ValueError(
                    f"{component} Cargo metadata package is absent from Cargo.lock: "
                    f"{name} {version}"
                )
            relative_manifest = None
            if source:
                identity = ("source", name, version, source)
            else:
                relative_manifest = _cargo_metadata_relative_manifest(
                    package.get("manifest_path"), workspace_root, component
                )
                identity = ("path", name, version, f"{component}/{relative_manifest}")
            license_value = package.get("license")
            license_declared = (
                "NOASSERTION"
                if license_value is None
                else _one_line(
                    license_value, f"{component} Cargo metadata package license"
                )
            )
            record = {
                "name": name,
                "version": version,
                "source": source,
                "manifest": relative_manifest,
                "license": license_declared,
                "checksum": lock_packages[lock_identity]["checksum"],
            }
            previous = package_records.get(identity)
            if previous is not None and previous != record:
                raise ValueError(
                    f"{component} Cargo metadata disagrees across release targets "
                    f"for {name} {version}"
                )
            package_records[identity] = record
            ids_for_graph[metadata_id] = identity

        root_candidates = {
            identity
            for identity in ids_for_graph.values()
            if identity[0] == "path" and identity[1] == root_name
        }
        if len(root_candidates) != 1:
            raise ValueError(
                f"{component} Cargo metadata {platform} must contain one path root "
                f"{root_name}"
            )
        root_identity = next(iter(root_candidates))
        resolve_root = resolve.get("root")
        if (
            resolve_root is not None
            and ids_for_graph.get(resolve_root) != root_identity
        ):
            raise ValueError(
                f"{component} Cargo metadata {platform} resolve root mismatch"
            )
        root_identities.add(root_identity)
        node_ids: set[str] = set()
        for raw_node in nodes:
            node = _mapping(raw_node, f"{component} Cargo metadata resolve node")
            node_id = _one_line(node.get("id"), f"{component} Cargo metadata node id")
            if node_id not in ids_for_graph:
                raise ValueError(f"{component} Cargo metadata node package is unknown")
            if node_id in node_ids:
                raise ValueError(
                    f"{component} Cargo metadata repeats resolve node {node_id}"
                )
            node_ids.add(node_id)
            graph_nodes.append((platform, release_target, node, ids_for_graph))

    if len(root_identities) != 1:
        raise ValueError(f"{component} Cargo metadata release targets disagree on root")
    root_identity = next(iter(root_identities))
    ids: dict[tuple[str, str, str, str], str] = {}

    def package_id(identity: tuple[str, str, str, str]) -> str:
        if identity == root_identity and root_spdx_id is not None:
            return root_spdx_id
        if identity not in ids:
            package = package_records[identity]
            if package["source"]:
                material = (
                    f"cargo\0{package['name']}\0{package['version']}\0"
                    f"{package['source']}"
                )
            else:
                material = (
                    f"cargo\0path\0{component}\0{package['name']}\0"
                    f"{package['version']}\0{package['manifest']}"
                )
            ids[identity] = _spdx_package_id("cargo", material)
        return ids[identity]

    edges: dict[
        tuple[str, str, str, str],
        dict[tuple[tuple[str, str, str, str], str], set[str]],
    ] = {}
    for platform, release_target, node, ids_for_graph in graph_nodes:
        node_id = _one_line(node.get("id"), f"{component} Cargo metadata node id")
        source_identity = ids_for_graph[node_id]
        raw_deps = node.get("deps")
        if not isinstance(raw_deps, list):
            raise ValueError(f"{component} Cargo metadata node deps must be a list")
        for raw_dep in raw_deps:
            dep = _mapping(raw_dep, f"{component} Cargo metadata dependency")
            alias = _one_line(dep.get("name"), f"{component} Cargo dependency alias")
            target_identity = ids_for_graph.get(dep.get("pkg"))
            if target_identity is None:
                raise ValueError(
                    f"{component} Cargo metadata dependency package is unknown"
                )
            raw_kinds = dep.get("dep_kinds")
            if not isinstance(raw_kinds, list) or not raw_kinds:
                raise ValueError(
                    f"{component} Cargo metadata dependency kinds are invalid"
                )
            for raw_kind in raw_kinds:
                kind = _mapping(raw_kind, f"{component} Cargo dependency kind")
                dependency_kind = kind.get("kind")
                if dependency_kind == "dev":
                    continue
                if dependency_kind not in (None, "build"):
                    raise ValueError(
                        f"{component} Cargo metadata has an unknown dependency kind"
                    )
                predicate = kind.get("target")
                if predicate is not None:
                    predicate = _one_line(
                        predicate, f"{component} Cargo dependency target"
                    )
                relationship = (
                    "DEPENDS_ON" if dependency_kind is None else "BUILD_DEPENDENCY_OF"
                )
                detail = (
                    f"platform={platform};filter={release_target};alias={alias};"
                    f"predicate={predicate or 'all'}"
                )
                edges.setdefault(source_identity, {}).setdefault(
                    (target_identity, relationship), set()
                ).add(detail)

    reachable: set[tuple[str, str, str, str]] = set()
    pending = [root_identity]
    relationships: set[tuple[str, str, str]] = set()
    comment_parts: dict[tuple[str, str, str], set[str]] = {}
    while pending:
        identity = pending.pop()
        if identity in reachable:
            continue
        reachable.add(identity)
        for (target, relationship), details in edges.get(identity, {}).items():
            if relationship == "BUILD_DEPENDENCY_OF":
                key = (package_id(target), relationship, package_id(identity))
            else:
                key = (package_id(identity), relationship, package_id(target))
            relationships.add(key)
            comment_parts.setdefault(key, set()).update(details)
            if target not in reachable:
                pending.append(target)

    spdx_packages: list[dict[str, Any]] = []
    for identity in sorted(reachable):
        if identity == root_identity and root_spdx_id is not None:
            continue
        package = package_records[identity]
        if package["source"]:
            source_info = (
                f"Cargo metadata source={package['source']}; Cargo.lock corroborated"
            )
        else:
            source_info = (
                f"{component} source tree {package['manifest']}; Cargo metadata resolve"
            )
        checksums = None
        if package["checksum"] is not None:
            checksums = [
                {
                    "algorithm": "SHA256",
                    "checksumValue": _sha256(
                        package["checksum"], f"{component} Cargo package checksum"
                    ),
                }
            ]
        spdx_packages.append(
            _spdx_package(
                package_id(identity),
                package["name"],
                package["version"],
                source_info,
                license_declared=package["license"],
                checksums=checksums,
            )
        )
    return (
        spdx_packages,
        relationships,
        package_id(root_identity),
        root_identity[2],
        comment_parts,
    )


def _jsonc_file(path: Path, label: str) -> dict[str, Any]:
    try:
        text = _regular_file(path, label).read_text(encoding="utf-8")
    except UnicodeDecodeError as error:
        raise ValueError(f"{label} must be UTF-8") from error
    without_comments: list[str] = []
    index = 0
    in_string = False
    escaped = False
    while index < len(text):
        char = text[index]
        if in_string:
            without_comments.append(char)
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
            index += 1
            continue
        if char == '"':
            in_string = True
            without_comments.append(char)
            index += 1
        elif text.startswith("//", index):
            index += 2
            while index < len(text) and text[index] not in "\r\n":
                index += 1
        elif text.startswith("/*", index):
            end = text.find("*/", index + 2)
            if end < 0:
                raise ValueError(f"{label} has an unterminated comment")
            without_comments.extend(
                "\n" for char in text[index : end + 2] if char == "\n"
            )
            index = end + 2
        else:
            without_comments.append(char)
            index += 1
    cleaned = "".join(without_comments)
    output: list[str] = []
    index = 0
    in_string = False
    escaped = False
    while index < len(cleaned):
        char = cleaned[index]
        if in_string:
            output.append(char)
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
            index += 1
            continue
        if char == '"':
            in_string = True
            output.append(char)
            index += 1
            continue
        if char == ",":
            next_index = index + 1
            while next_index < len(cleaned) and cleaned[next_index].isspace():
                next_index += 1
            if next_index < len(cleaned) and cleaned[next_index] in "]}":
                index += 1
                continue
        output.append(char)
        index += 1
    try:
        value = json.loads("".join(output))
    except json.JSONDecodeError as error:
        raise ValueError(f"{label} must be valid JSONC") from error
    return _mapping(value, label)


def _bun_resolution_name_version(resolution: str) -> tuple[str, str]:
    separator = resolution.rfind("@")
    if separator <= 0 or separator == len(resolution) - 1:
        raise ValueError(f"invalid bun.lock resolution: {resolution}")
    return resolution[:separator], resolution[separator + 1 :]


def _bun_package_key_parts(key: str) -> tuple[str, ...]:
    key = _one_line(key, "OMP bun.lock package key")
    raw_parts = key.split("/")
    if any(part in ("", ".", "..") for part in raw_parts):
        raise ValueError(f"OMP bun.lock package key is invalid: {key}")
    parts: list[str] = []
    index = 0
    while index < len(raw_parts):
        part = raw_parts[index]
        if part.startswith("@"):
            if (
                part == "@"
                or index + 1 == len(raw_parts)
                or raw_parts[index + 1].startswith("@")
            ):
                raise ValueError(f"OMP bun.lock package key is invalid: {key}")
            parts.append(f"{part}/{raw_parts[index + 1]}")
            index += 2
        else:
            parts.append(part)
            index += 1
    return tuple(parts)


def _bun_package_parent(key: str, package_name: str) -> str | None:
    parts = _bun_package_key_parts(key)
    if parts[-1] != package_name:
        raise ValueError(
            f"OMP bun.lock package key does not end in its package name: {key}"
        )
    return "/".join(parts[:-1]) or None


def _bun_package_key_name(key: str) -> str:
    return _bun_package_key_parts(key)[-1]


def _bun_resolve_dependency(
    package_data: dict[str, dict[str, Any]], parent_key: str, dependency_name: str
) -> str | None:
    ancestor: str | None = parent_key
    while True:
        matches = sorted(
            key
            for key, package in package_data.items()
            if package["parent"] == ancestor and package["key_name"] == dependency_name
        )
        if len(matches) > 1:
            raise ValueError(
                f"OMP bun.lock ambiguously resolves {dependency_name} from {parent_key}"
            )
        if matches:
            if matches[0] == parent_key:
                raise ValueError(
                    f"OMP bun.lock resolves {dependency_name} from {parent_key} to itself"
                )
            return matches[0]
        if ancestor is None:
            return None
        ancestor = package_data[ancestor]["parent"]


def _bun_platform_values(value: Any, label: str) -> tuple[str, ...]:
    if value is None:
        return ()
    raw_values = [value] if isinstance(value, str) else value
    if not isinstance(raw_values, list):
        raise ValueError(f"{label} must be a string or list")
    values = tuple(_one_line(item, label) for item in raw_values)
    if any(
        re.fullmatch(r"!?[A-Za-z0-9][A-Za-z0-9._-]*", item) is None for item in values
    ):
        raise ValueError(f"{label} has an invalid platform value")
    if len(set(values)) != len(values):
        raise ValueError(f"{label} repeats a platform value")
    return tuple(sorted(values))


def _bun_dimension_applies(
    values: tuple[str, ...], target: str, aliases: dict[str, str]
) -> bool:
    normalized = [
        (
            value.startswith("!"),
            aliases.get(
                value.removeprefix("!").lower(), value.removeprefix("!").lower()
            ),
        )
        for value in values
    ]
    allowed = {value for excluded, value in normalized if not excluded}
    excluded = {value for excluded, value in normalized if excluded}
    return (not allowed or target in allowed) and target not in excluded


def _bun_applicable_platforms(package: dict[str, Any]) -> frozenset[str]:
    return frozenset(
        platform
        for platform, (os_name, cpu_name) in BUN_PLATFORM_TARGETS.items()
        if _bun_dimension_applies(package["os"], os_name, BUN_OS_ALIASES)
        and _bun_dimension_applies(package["cpu"], cpu_name, BUN_CPU_ALIASES)
    )


def _bun_dependency_graph(
    root: Path, root_spdx_id: str
) -> tuple[
    list[dict[str, Any]],
    set[tuple[str, str, str]],
    str,
    dict[tuple[str, str, str], set[str]],
]:
    lock = _jsonc_file(root / "bun.lock", "OMP bun.lock")
    workspaces = _mapping(lock.get("workspaces"), "OMP bun.lock workspaces")
    raw_packages = _mapping(lock.get("packages"), "OMP bun.lock packages")
    if OMP_BUN_WORKSPACE not in workspaces:
        raise ValueError("OMP bun.lock has no coding-agent workspace")
    workspace_by_name: dict[str, tuple[str, dict[str, Any]]] = {}
    for path, raw in workspaces.items():
        workspace = _mapping(raw, f"OMP bun.lock workspace {path}")
        name = _one_line(workspace.get("name"), f"OMP workspace name {path}")
        if name in workspace_by_name:
            raise ValueError(f"OMP bun.lock repeats workspace {name}")
        workspace_by_name[name] = (path, workspace)
    root_workspace = _mapping(
        workspaces[OMP_BUN_WORKSPACE], "OMP coding-agent workspace"
    )
    root_name = _one_line(root_workspace.get("name"), "OMP coding-agent name")
    root_version = _one_line(root_workspace.get("version"), "OMP coding-agent version")
    if root_name not in raw_packages:
        raise ValueError("OMP bun.lock has no coding-agent package resolution")

    package_data: dict[str, dict[str, Any]] = {}
    for raw_key, raw in raw_packages.items():
        key = _one_line(raw_key, "OMP bun.lock package key")
        if not isinstance(raw, list) or not raw or not isinstance(raw[0], str):
            raise ValueError(f"OMP bun.lock package {key} is invalid")
        resolution = _one_line(raw[0], f"OMP bun.lock resolution {key}")
        info = next((value for value in raw[1:] if isinstance(value, dict)), {})
        integrity = next(
            (
                value
                for value in reversed(raw[1:])
                if isinstance(value, str)
                and re.fullmatch(r"sha(?:1|256|512)-.+", value)
            ),
            None,
        )
        if "@workspace:" in resolution:
            name, workspace_path = resolution.split("@workspace:", 1)
            workspace_record = workspace_by_name.get(name)
            if workspace_record is None or workspace_record[0] != workspace_path:
                raise ValueError(f"OMP bun.lock workspace resolution mismatch: {key}")
            workspace = workspace_record[1]
            version = _one_line(
                workspace.get("version", "0.0.0"), f"OMP workspace version {name}"
            )
            dependencies = workspace
            resolved_workspace = workspace_path
            integrity = None
        else:
            name, version = _bun_resolution_name_version(resolution)
            dependencies = info
            resolved_workspace = None
        optional_peers_raw = dependencies.get("optionalPeers", [])
        if not isinstance(optional_peers_raw, list):
            raise ValueError(f"OMP bun.lock {key} optionalPeers must be a list")
        optional_peers = tuple(
            _one_line(value, f"OMP bun.lock {key} optionalPeers")
            for value in optional_peers_raw
        )
        if len(set(optional_peers)) != len(optional_peers):
            raise ValueError(f"OMP bun.lock {key} optionalPeers repeats a dependency")
        package_data[key] = {
            "name": name,
            "key_name": _bun_package_key_name(key),
            "version": version,
            "resolution": resolution,
            "dependencies": dependencies,
            "workspace": resolved_workspace,
            "integrity": integrity,
            "parent": _bun_package_parent(key, name),
            "os": _bun_platform_values(
                dependencies.get("os"), f"OMP bun.lock {key} os"
            ),
            "cpu": _bun_platform_values(
                dependencies.get("cpu"), f"OMP bun.lock {key} cpu"
            ),
            "optional_peers": optional_peers,
        }

    for key, package in package_data.items():
        parent = package["parent"]
        if parent is not None and parent not in package_data:
            raise ValueError(f"OMP bun.lock package {key} has unknown parent {parent}")
        peer_dependencies = package["dependencies"].get("peerDependencies", {})
        if not isinstance(peer_dependencies, dict):
            raise ValueError(f"OMP bun.lock {key} peerDependencies must be an object")
        if not set(package["optional_peers"]).issubset(peer_dependencies):
            raise ValueError(
                f"OMP bun.lock {key} optionalPeers must name peer dependencies"
            )
    root_package = package_data[root_name]
    if (
        root_package["name"] != root_name
        or root_package["parent"] is not None
        or root_package["workspace"] != OMP_BUN_WORKSPACE
    ):
        raise ValueError("OMP bun.lock coding-agent package identity mismatch")
    for package in package_data.values():
        package["applicable_platforms"] = _bun_applicable_platforms(package)

    ids: dict[str, str] = {root_name: root_spdx_id}

    def package_id(key: str) -> str:
        if key not in ids:
            package = package_data[key]
            material = (
                f"bun\0{key}\0{package['name']}\0{package['version']}\0"
                f"{package['resolution']}"
            )
            ids[key] = _spdx_package_id("bun", material)
        return ids[key]

    patched = lock.get("patchedDependencies", {})
    if not isinstance(patched, dict):
        raise ValueError("OMP bun.lock patchedDependencies must be an object")
    reachable: set[str] = set()
    pending = [root_name]
    relationships: set[tuple[str, str, str]] = set()
    relationship_comments: dict[tuple[str, str, str], set[str]] = {}
    while pending:
        key = pending.pop()
        if key in reachable:
            continue
        reachable.add(key)
        package = package_data[key]
        dependency_sets = package["dependencies"]
        for field, base_relationship, base_reverse, required in (
            ("dependencies", "DEPENDS_ON", False, True),
            ("optionalDependencies", "OPTIONAL_DEPENDENCY_OF", True, False),
            ("peerDependencies", "PREREQUISITE_FOR", True, False),
        ):
            values = dependency_sets.get(field, {})
            if not isinstance(values, dict):
                raise ValueError(f"OMP bun.lock {key} {field} must be an object")
            for raw_dependency_name in sorted(values):
                dependency_name = _one_line(
                    raw_dependency_name, f"OMP bun.lock {key} {field} name"
                )
                dependency_key = _bun_resolve_dependency(
                    package_data, key, dependency_name
                )
                if dependency_key is None:
                    if required:
                        raise ValueError(
                            f"OMP bun.lock cannot resolve {dependency_name} from {key}"
                        )
                    continue
                dependency = package_data[dependency_key]
                optional_peer = (
                    field == "peerDependencies"
                    and dependency_name in package["optional_peers"]
                )
                platform_limited = not package["applicable_platforms"] or not package[
                    "applicable_platforms"
                ].issubset(dependency["applicable_platforms"])
                relationship = base_relationship
                reverse = base_reverse
                if field == "peerDependencies" and (optional_peer or platform_limited):
                    relationship = "OPTIONAL_DEPENDENCY_OF"
                    reverse = True
                elif field == "dependencies" and platform_limited:
                    relationship = "OPTIONAL_DEPENDENCY_OF"
                    reverse = True
                parent_id = package_id(key)
                child_id = package_id(dependency_key)
                edge = (
                    (child_id, relationship, parent_id)
                    if reverse
                    else (parent_id, relationship, child_id)
                )
                relationships.add(edge)
                applicability: list[str] = []
                if platform_limited:
                    for role, candidate in (
                        ("parent", package),
                        ("dependency", dependency),
                    ):
                        if candidate["os"]:
                            applicability.append(
                                f"bun.{role}.os={','.join(candidate['os'])}"
                            )
                        if candidate["cpu"]:
                            applicability.append(
                                f"bun.{role}.cpu={','.join(candidate['cpu'])}"
                            )
                if optional_peer:
                    applicability.append("bun.optional_peer=true")
                if applicability:
                    relationship_comments.setdefault(edge, set()).add(
                        ";".join(applicability)
                    )
                if dependency_key not in reachable:
                    pending.append(dependency_key)

    spdx_packages: list[dict[str, Any]] = []
    for key in sorted(reachable):
        if key == root_name:
            continue
        package = package_data[key]
        license_declared = "NOASSERTION"
        source_info = (
            f"OMP bun.lock package key={key};resolution={package['resolution']}"
        )
        if package["workspace"] is not None:
            package_json_path = root / package["workspace"] / "package.json"
            package_json = _mapping(
                _json_file(package_json_path, f"OMP workspace package.json {key}"),
                f"OMP workspace package.json {key}",
            )
            if (
                package_json.get("name") != package["name"]
                or package_json.get("version", "0.0.0") != package["version"]
            ):
                raise ValueError(f"OMP workspace package identity mismatch: {key}")
            if isinstance(package_json.get("license"), str):
                license_declared = _one_line(
                    package_json["license"], f"OMP workspace license {key}"
                )
            source_info = (
                f"OMP source tree {package['workspace']}/package.json and bun.lock"
            )
        applicability = []
        if package["os"]:
            applicability.append(f"os={','.join(package['os'])}")
        if package["cpu"]:
            applicability.append(f"cpu={','.join(package['cpu'])}")
        if package["optional_peers"]:
            applicability.append(f"optionalPeers={','.join(package['optional_peers'])}")
        if applicability:
            source_info = f"{source_info};{';'.join(applicability)}"
        checksums = None
        integrity = package["integrity"]
        if integrity is not None:
            algorithm, encoded = integrity.split("-", 1)
            algorithms = {"sha1": "SHA1", "sha256": "SHA256", "sha512": "SHA512"}
            expected_lengths = {"sha1": 20, "sha256": 32, "sha512": 64}
            try:
                checksum_bytes = base64.b64decode(encoded, validate=True)
            except binascii.Error as error:
                raise ValueError(f"OMP bun.lock integrity is invalid: {key}") from error
            if len(checksum_bytes) != expected_lengths[algorithm]:
                raise ValueError(f"OMP bun.lock integrity has the wrong length: {key}")
            checksum = checksum_bytes.hex()
            checksums = [
                {"algorithm": algorithms[algorithm], "checksumValue": checksum}
            ]
        patch_key = f"{package['name']}@{package['version']}"
        comments: list[str] = []
        if patch_key in patched:
            patch_path = _one_line(patched[patch_key], f"OMP patch path {patch_key}")
            patch = _file_record(root / patch_path, f"OMP patch {patch_path}")
            comments.append(
                f"bun.lock patch={patch_path};length={patch['length']};"
                f"sha256={patch['sha256']}"
            )
        spdx_packages.append(
            _spdx_package(
                package_id(key),
                package["name"],
                package["version"],
                source_info,
                license_declared=license_declared,
                checksums=checksums,
                comment=";".join(comments) if comments else None,
            )
        )
    return spdx_packages, relationships, root_version, relationship_comments


def _bazel_dependency_graph(
    path: Path, root_spdx_id: str
) -> tuple[list[dict[str, Any]], set[tuple[str, str, str]]]:
    graph = _mapping(
        _json_file(Path(path), "OMP Bazel module graph"), "OMP Bazel module graph"
    )
    packages: dict[str, dict[str, Any]] = {}
    nodes: dict[str, tuple[str, str]] = {}
    expanded: set[str] = set()
    unexpanded: set[str] = set()
    relationships: set[tuple[str, str, str]] = set()

    def identity(value: dict[str, Any]) -> tuple[str, str, str, str]:
        key = _one_line(value.get("key"), "OMP Bazel module graph key")
        name = _one_line(value.get("name"), f"OMP Bazel module {key} name")
        apparent_name = _one_line(
            value.get("apparentName"), f"OMP Bazel module {key} apparentName"
        )
        raw_version = value.get("version")
        if key == "<root>":
            if raw_version != "" or apparent_name != name:
                raise ValueError("OMP Bazel module graph root identity is invalid")
            version = ""
        else:
            version = _one_line(raw_version, f"OMP Bazel module {key} version")
        previous = nodes.get(key)
        if previous is not None and previous != (name, version):
            raise ValueError(f"OMP Bazel module graph disagrees for {key}")
        nodes[key] = (name, version)
        return key, name, version, apparent_name

    def package(key: str, name: str, version: str) -> str:
        if key == "<root>":
            return root_spdx_id
        package_id = _spdx_package_id("bazel", f"bazel\0{key}\0{name}\0{version}")
        _add_spdx_package(
            packages,
            _spdx_package(
                package_id,
                name,
                version,
                f"Bazel module graph key={key}",
            ),
        )
        return package_id

    def reference(
        value: Any,
        parent_id: str,
        *,
        label: str,
        active: dict[str, tuple[tuple[str, str], str]] | None = None,
    ) -> None:
        node = _mapping(value, label)
        required = {"key", "name", "version", "apparentName", "unexpanded"}
        if set(node) != required or node.get("unexpanded") is not True:
            raise ValueError(f"{label} must be one exact unexpanded module reference")
        key, name, version, _ = identity(node)
        if active is not None:
            ancestor = active.get(key)
            if ancestor is None or ancestor[0] != (name, version):
                raise ValueError(f"{label} does not identify an active ancestor")
            package_id = ancestor[1]
        else:
            if key == "<root>":
                raise ValueError(f"{label} unexpectedly references the root")
            package_id = package(key, name, version)
            unexpanded.add(key)
        relationships.add((package_id, "BUILD_DEPENDENCY_OF", parent_id))

    def visit(
        value: Any,
        parent_id: str | None,
        active: dict[str, tuple[tuple[str, str], str]],
    ) -> str:
        node = _mapping(value, "OMP Bazel module graph node")
        if node.get("unexpanded") is True:
            if parent_id is None:
                raise ValueError("OMP Bazel module graph root cannot be unexpanded")
            reference(node, parent_id, label="OMP Bazel module dependency")
            key, name, version, _ = identity(node)
            return package(key, name, version)

        required = {
            "key",
            "name",
            "version",
            "apparentName",
            "dependencies",
            "indirectDependencies",
            "cycles",
        }
        allowed = required | ({"root"} if parent_id is None else set())
        if set(node) != allowed:
            raise ValueError("OMP Bazel module graph node has unexpected fields")
        key, name, version, _ = identity(node)
        if parent_id is None:
            if node.get("root") is not True or key != "<root>":
                raise ValueError("OMP Bazel module graph root is invalid")
        elif key == "<root>":
            raise ValueError("OMP Bazel module graph contains a nested root")
        if key in expanded:
            raise ValueError(f"OMP Bazel module graph expands {key} more than once")
        if key in active:
            raise ValueError(
                f"OMP Bazel module graph contains an inline cycle at {key}"
            )
        package_id = package(key, name, version)
        expanded.add(key)
        if parent_id is not None:
            relationships.add((package_id, "BUILD_DEPENDENCY_OF", parent_id))
        dependencies = node["dependencies"]
        indirect = node["indirectDependencies"]
        cycles = node["cycles"]
        if not isinstance(dependencies, list):
            raise ValueError(f"OMP Bazel module dependencies must be a list: {key}")
        if not isinstance(indirect, list):
            raise ValueError(
                f"OMP Bazel module indirectDependencies must be a list: {key}"
            )
        if indirect:
            raise ValueError(
                "OMP Bazel module graph contains indirect dependencies; full graph required"
            )
        if not isinstance(cycles, list):
            raise ValueError(f"OMP Bazel module cycles must be a list: {key}")
        next_active = {**active, key: ((name, version), package_id)}
        for dependency in dependencies:
            visit(dependency, package_id, next_active)
        for cycle in cycles:
            reference(
                cycle,
                package_id,
                label=f"OMP Bazel module cycle from {key}",
                active=next_active,
            )
        return package_id

    visit(graph, None, {})
    missing = sorted(unexpanded - expanded)
    if missing:
        raise ValueError(
            "OMP Bazel module graph has unexpanded-only modules: " + ", ".join(missing)
        )
    return [packages[key] for key in sorted(packages)], relationships


def _zip_member_sha256(archive: zipfile.ZipFile, name: str) -> str:
    digest = hashlib.sha256()
    try:
        source = archive.open(name)
    except KeyError as error:
        raise ValueError(f"Windows payload is missing {name}") from error
    with source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _conpty_dependency(
    asset_dir: Path, herdr_root: Path, windows_file_id: str
) -> tuple[dict[str, Any], tuple[str, str, str]]:
    metadata = _mapping(
        _json_file(herdr_root / CONPTY_METADATA_PATH, "ConPTY metadata"),
        "ConPTY metadata",
    )
    _exact_keys(
        metadata, {"schema_version", "package", "bundles", "notices"}, "ConPTY metadata"
    )
    if metadata.get("schema_version") != 1:
        raise ValueError("ConPTY metadata schema mismatch")
    package = _mapping(metadata["package"], "ConPTY package")
    _exact_keys(
        package,
        {"id", "version", "release_tag", "url", "sha256", "license"},
        "ConPTY package",
    )
    package_name = _one_line(package.get("id"), "ConPTY package id")
    package_version = _one_line(package.get("version"), "ConPTY package version")
    package_url = _one_line(package.get("url"), "ConPTY package URL")
    package_sha = _sha256(package.get("sha256"), "ConPTY package SHA-256")
    license_declared = _one_line(package.get("license"), "ConPTY package license")
    bundles = _mapping(metadata["bundles"], "ConPTY bundles")
    _exact_keys(bundles, {"x86_64"}, "ConPTY bundles")
    bundle = _mapping(bundles["x86_64"], "ConPTY x86_64 bundle")
    _exact_keys(bundle, {"target_triple", "files"}, "ConPTY x86_64 bundle")
    if bundle.get("target_triple") != "x86_64-pc-windows-msvc":
        raise ValueError("ConPTY target triple mismatch")
    files = bundle.get("files")
    notices = metadata.get("notices")
    if not isinstance(files, list) or not isinstance(notices, list):
        raise ValueError("ConPTY bundle files and notices must be lists")
    expected_names = {"herdr.exe", "conpty/herdr-conpty.json"}
    embedded: list[dict[str, Any]] = []
    for label, values in (("file", files), ("notice", notices)):
        for raw in values:
            item = _mapping(raw, f"ConPTY {label}")
            destination = _one_line(
                item.get("destination"), f"ConPTY {label} destination"
            )
            digest = _sha256(item.get("sha256"), f"ConPTY {label} SHA-256")
            if destination in expected_names:
                raise ValueError(f"ConPTY bundle repeats {destination}")
            expected_names.add(destination)
            embedded.append({"kind": label, "path": destination, "sha256": digest})
            if label == "notice":
                source_path = _one_line(item.get("source"), "ConPTY notice source")
                if (
                    _file_record(
                        herdr_root / source_path, f"ConPTY notice source {source_path}"
                    )["sha256"]
                    != digest
                ):
                    raise ValueError(
                        f"ConPTY notice source digest mismatch: {source_path}"
                    )
    marker = {
        "schema_version": 1,
        "package": package_name,
        "version": package_version,
        "architecture": "x86_64",
        "files": {item["destination"]: item["sha256"] for item in files},
    }
    marker_bytes = (json.dumps(marker, indent=2, sort_keys=True) + "\n").encode()
    windows_payload = _asset_file(asset_dir, EXPECTED_ASSET_NAMES["windows-x86_64"])
    try:
        with zipfile.ZipFile(windows_payload) as archive:
            members = archive.infolist()
            names = [info.filename for info in members]
            if (
                len(members) != len(expected_names)
                or len(set(names)) != len(names)
                or set(names) != expected_names
                or any(
                    info.is_dir()
                    or info.filename.endswith("/")
                    or info.external_attr & 0x10
                    or stat.S_IFMT(info.external_attr >> 16)
                    not in (0, stat.S_IFREG)
                    for info in members
                )
            ):
                raise ValueError("Windows payload ConPTY bundle layout mismatch")
            if archive.read("conpty/herdr-conpty.json") != marker_bytes:
                raise ValueError("Windows payload ConPTY marker mismatch")
            for item in embedded:
                if _zip_member_sha256(archive, item["path"]) != item["sha256"]:
                    raise ValueError(
                        f"Windows payload ConPTY digest mismatch: {item['path']}"
                    )
    except zipfile.BadZipFile as error:
        raise ValueError("Windows payload must be a valid ConPTY zip") from error
    package_id = _spdx_package_id(
        "conpty", f"conpty\0{package_name}\0{package_version}\0{package_sha}"
    )
    comment = json.dumps(
        {
            "embedded": sorted(embedded, key=lambda item: item["path"]),
            "release_tag": _one_line(package.get("release_tag"), "ConPTY release tag"),
        },
        sort_keys=True,
        separators=(",", ":"),
    )
    spdx_package = _spdx_package(
        package_id,
        package_name,
        package_version,
        "Pinned Microsoft ConPTY package embedded in herdr-windows-x86_64.zip",
        license_declared=license_declared,
        download_location=package_url,
        checksums=[{"algorithm": "SHA256", "checksumValue": package_sha}],
        comment=comment,
    )
    return spdx_package, (package_id, "RUNTIME_DEPENDENCY_OF", windows_file_id)


def build_spdx(
    asset_dir: Path,
    built_at: str,
    parent_commit: str,
    herdr_commit: str,
    omp_commit: str,
    herdr_version: str,
    omp_version: str,
    herdr_root: Path,
    omp_root: Path,
    omp_bazel_graph: Path,
    cargo_metadata_dir: Path,
) -> str:
    asset_dir = Path(asset_dir)
    built_at = _timestamp(built_at, "built_at")
    parent_commit = _git_object(parent_commit, "parent_commit")
    herdr_commit = _git_object(herdr_commit, "herdr_commit")
    omp_commit = _git_object(omp_commit, "omp_commit")
    herdr_version = _one_line(herdr_version, "herdr_version")
    omp_version = _one_line(omp_version, "omp_version")
    herdr_root = Path(herdr_root)
    omp_root = Path(omp_root)
    created_at = built_at
    herdr_package = _mapping(
        _read_toml(herdr_root / "Cargo.toml", "Herdr Cargo.toml").get("package"),
        "Herdr Cargo package",
    )
    if herdr_package.get("name") != "herdr":
        raise ValueError("Herdr Cargo package has the wrong name")
    herdr_base_version = _one_line(
        herdr_package.get("version"), "Herdr Cargo package version"
    )
    herdr_license = (
        _one_line(herdr_package["license"], "Herdr Cargo package license")
        if isinstance(herdr_package.get("license"), str)
        else "NOASSERTION"
    )
    omp_package = _mapping(
        _json_file(
            omp_root / OMP_BUN_WORKSPACE / "package.json",
            "OMP coding-agent package.json",
        ),
        "OMP coding-agent package.json",
    )
    if omp_package.get("name") != "@oh-my-pi/pi-coding-agent":
        raise ValueError("OMP coding-agent package has the wrong name")
    if omp_package.get("version") != omp_version:
        raise ValueError("OMP coding-agent package version mismatch")
    omp_license = (
        _one_line(omp_package["license"], "OMP coding-agent package license")
        if isinstance(omp_package.get("license"), str)
        else "NOASSERTION"
    )
    records = _asset_records(asset_dir, RELEASE_PAYLOAD_ASSET_NAMES)
    packages: dict[str, dict[str, Any]] = {}
    relationships: set[tuple[str, str, str]] = {
        ("SPDXRef-DOCUMENT", "DESCRIBES", "SPDXRef-Package-herdr"),
        ("SPDXRef-DOCUMENT", "DESCRIBES", "SPDXRef-Package-omp"),
    }
    relationship_comments: dict[tuple[str, str, str], set[str]] = {}
    _add_spdx_package(
        packages,
        _spdx_package(
            "SPDXRef-Package-herdr",
            "herdr",
            herdr_version,
            f"git+https://github.com/Smarty-Pants-Inc/herdr@{herdr_commit}",
            license_declared=herdr_license,
        ),
    )
    _add_spdx_package(
        packages,
        _spdx_package(
            "SPDXRef-Package-omp",
            "omp",
            omp_version,
            f"git+https://github.com/Smarty-Pants-Inc/oh-my-pi@{omp_commit}",
            license_declared=omp_license,
        ),
    )
    metadata_dir = validate_cargo_metadata_directory(Path(cargo_metadata_dir))
    (
        cargo_packages,
        cargo_relationships,
        _,
        herdr_cargo_version,
        cargo_comments,
    ) = _cargo_dependency_graph(
        herdr_root,
        "herdr",
        "herdr",
        "SPDXRef-Package-herdr",
        metadata_dir,
    )
    if herdr_cargo_version != herdr_base_version or not herdr_version.startswith(
        f"{herdr_base_version}-preview."
    ):
        raise ValueError("Herdr SPDX version does not match Cargo source and lock")
    for package in cargo_packages:
        _add_spdx_package(packages, package)
    relationships.update(cargo_relationships)
    for relationship, details in cargo_comments.items():
        relationship_comments.setdefault(relationship, set()).update(details)
    (
        omp_cargo_packages,
        omp_cargo_relationships,
        omp_cargo_root,
        omp_cargo_version,
        cargo_comments,
    ) = _cargo_dependency_graph(omp_root, "omp", "pi-natives", None, metadata_dir)
    if omp_cargo_version != omp_version:
        raise ValueError("OMP native Cargo.lock version mismatch")
    for package in omp_cargo_packages:
        _add_spdx_package(packages, package)
    relationships.update(omp_cargo_relationships)
    for relationship, details in cargo_comments.items():
        relationship_comments.setdefault(relationship, set()).update(details)
    relationships.add(("SPDXRef-Package-omp", "DEPENDS_ON", omp_cargo_root))
    bun_packages, bun_relationships, bun_version, bun_comments = _bun_dependency_graph(
        omp_root, "SPDXRef-Package-omp"
    )
    if bun_version != omp_version:
        raise ValueError("OMP bun.lock coding-agent version mismatch")
    for package in bun_packages:
        _add_spdx_package(packages, package)
    relationships.update(bun_relationships)
    for relationship, details in bun_comments.items():
        relationship_comments.setdefault(relationship, set()).update(details)
    bazel_packages, bazel_relationships = _bazel_dependency_graph(
        Path(omp_bazel_graph), "SPDXRef-Package-omp"
    )
    for package in bazel_packages:
        _add_spdx_package(packages, package)
    rules_rust_version, _ = _rules_rust_declaration(omp_root)
    if [
        package["versionInfo"]
        for package in bazel_packages
        if package["name"] == "rules_rust"
    ] != [rules_rust_version]:
        raise ValueError("OMP Bazel graph rules_rust version mismatch")
    relationships.update(bazel_relationships)
    files = []
    file_ids: dict[str, str] = {}
    for name in RELEASE_PAYLOAD_ASSET_NAMES:
        file_id = f"SPDXRef-File-{re.sub(r'[^A-Za-z0-9.-]', '-', name)}"
        file_ids[name] = file_id
        component = PAYLOAD_METADATA_BY_NAME[name]["component"]
        package_id = (
            "SPDXRef-Package-omp"
            if component == "omp-native"
            else f"SPDXRef-Package-{component}"
        )
        digests = _file_digests(_asset_file(asset_dir, name), f"asset {name}")
        files.append(
            {
                "SPDXID": file_id,
                "checksums": [
                    {"algorithm": "SHA1", "checksumValue": digests["sha1"]},
                    {"algorithm": "SHA256", "checksumValue": records[name]["sha256"]},
                ],
                "copyrightText": "NOASSERTION",
                "fileName": name,
                "fileTypes": ["BINARY"],
                "licenseConcluded": "NOASSERTION",
            }
        )
        relationships.add((package_id, "CONTAINS", file_id))
    conpty_package, conpty_relationship = _conpty_dependency(
        asset_dir,
        herdr_root,
        file_ids[EXPECTED_ASSET_NAMES["windows-x86_64"]],
    )
    _add_spdx_package(packages, conpty_package)
    relationships.add(conpty_relationship)
    document = {
        "SPDXID": "SPDXRef-DOCUMENT",
        "comment": (
            f"parent_commit={parent_commit};herdr_commit={herdr_commit};"
            f"omp_commit={omp_commit}"
        ),
        "creationInfo": {
            "created": created_at,
            "creators": [SPDX_CREATOR],
        },
        "dataLicense": "CC0-1.0",
        "documentDescribes": ["SPDXRef-Package-herdr", "SPDXRef-Package-omp"],
        "documentNamespace": _spdx_namespace(parent_commit, herdr_commit, omp_commit),
        "files": files,
        "name": f"smarty-pair-{herdr_commit[:12]}",
        "packages": [packages[key] for key in sorted(packages)],
        "relationships": [
            {
                "spdxElementId": source,
                "relationshipType": relationship,
                "relatedSpdxElement": target,
                **(
                    {"comment": ";".join(sorted(relationship_comments[key]))}
                    if (key := (source, relationship, target)) in relationship_comments
                    else {}
                ),
            }
            for source, relationship, target in sorted(relationships)
        ],
        "spdxVersion": "SPDX-2.3",
    }
    return _canonical_json(document)


def build_pair_manifest(
    asset_dir: Path,
    repo: str,
    tag: str,
    build_id: str,
    built_at: str,
    parent_repo: str,
    parent_commit: str,
    parent_tree: str,
    herdr_commit: str,
    herdr_tree: str,
    base_version: str,
    protocol: int,
    omp_source: dict[str, Any],
    herdr_root: Path,
    omp_root: Path,
    omp_rules_rust_toolchains: Path,
    trusted_verifier: Path,
    source_archive_dir: Path,
    omp_bazel_graph: Path,
    cargo_metadata_dir: Path,
    bun_version: str,
    zig_version: str,
) -> str:
    asset_dir = Path(asset_dir)
    repo = _repository(repo, "repo")
    if repo != HERDR_REPOSITORY:
        raise ValueError(f"repo must be {HERDR_REPOSITORY}")
    tag = _one_line(tag, "tag")
    build_id = _one_line(build_id, "build_id")
    built_at = _timestamp(built_at, "built_at")
    parent_repo = _repository(parent_repo, "parent_repo")
    if parent_repo != PARENT_REPOSITORY:
        raise ValueError(f"parent_repo must be {PARENT_REPOSITORY}")
    parent_commit = _git_object(parent_commit, "parent_commit")
    parent_tree = _git_object(parent_tree, "parent_tree")
    herdr_commit = _git_object(herdr_commit, "herdr_commit")
    herdr_tree = _git_object(herdr_tree, "herdr_tree")
    base_version = normalize_version(_one_line(base_version, "base_version"))
    base_version = _one_line(base_version, "base_version")
    protocol = _protocol(protocol)
    omp = validate_omp_source(omp_source)
    for field in ("commit", "tree"):
        _git_object(omp[field], f"OMP source {field}")
    if omp["repository"] != OMP_REPOSITORY:
        raise ValueError(f"OMP source repository must be {OMP_REPOSITORY}")
    build_id = _validate_paired_build_id(
        build_id, parent_commit, herdr_commit, omp["commit"], built_at
    )
    if tag != f"smarty-preview-{build_id}":
        raise ValueError("tag must exactly namespace the paired build_id")
    bun_version = _one_line(bun_version, "bun_version")
    zig_version = _one_line(zig_version, "zig_version")
    if bun_version != TRUSTED_BUN_VERSION or zig_version != TRUSTED_ZIG_VERSION:
        raise ValueError(
            "pair manifest toolchain versions do not match the trusted workflow"
        )
    herdr_root = Path(herdr_root)
    omp_root = Path(omp_root)
    rules_rust = _read_rules_rust_toolchains(Path(omp_rules_rust_toolchains), omp_root)
    document = {
        "pair_id": pair_id_for_sources(parent_commit, herdr_commit, omp["commit"]),
        "schema": PAIR_MANIFEST_SCHEMA,
        "release": {
            "repository": repo,
            "tag": tag,
            "build_id": build_id,
            "built_at": built_at,
            "immutable": True,
        },
        "sources": {
            "parent": {
                "repository": parent_repo,
                "commit": parent_commit,
                "tree": parent_tree,
            },
            "herdr": {
                "repository": repo,
                "commit": herdr_commit,
                "tree": herdr_tree,
                "version": base_version,
                "build_id": build_id,
                "protocol": protocol,
            },
            "omp": omp,
        },
        "toolchains": {
            "herdr": {
                "rust": _toolchain_channel(
                    herdr_root / "rust-toolchain.toml", "Herdr rust-toolchain.toml"
                ),
                "zig": zig_version,
                "declarations": {
                    "rust": _declaration_record(
                        herdr_root,
                        "rust-toolchain.toml",
                        "Herdr rust-toolchain.toml",
                    )
                },
            },
            "omp": {
                "bun": bun_version,
                "bazel": _one_line_file(
                    omp_root / ".bazelversion", "OMP .bazelversion"
                ),
                "rust": _toolchain_channel(
                    omp_root / "rust-toolchain.toml", "OMP rust-toolchain.toml"
                ),
                "declarations": {
                    "bazel": _declaration_record(
                        omp_root, ".bazelversion", "OMP .bazelversion"
                    ),
                    "rust": _declaration_record(
                        omp_root,
                        "rust-toolchain.toml",
                        "OMP rust-toolchain.toml",
                    ),
                },
                "rules_rust": rules_rust,
            },
        },
        "lock_inputs": _lock_input_records(herdr_root, omp_root),
        "platforms": _platform_manifest(),
        "artifacts": _release_artifact_records(asset_dir, repo, tag),
        "evidence": _evidence_records(asset_dir, repo, tag),
        "verification": _semantic_verification_record(
            verifier_path=Path(trusted_verifier),
            source_archive_dir=Path(source_archive_dir),
            omp_bazel_graph=Path(omp_bazel_graph),
            cargo_metadata_dir=Path(cargo_metadata_dir),
            omp_rules_rust_toolchains=Path(omp_rules_rust_toolchains),
            spdx_path=asset_dir / SPDX_ASSET_NAME,
        ),
    }
    return _canonical_json(document)


def _json_file(path: Path, label: str) -> Any:
    try:
        return json.loads(_regular_file(path, label).read_text(encoding="utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"{label} must be valid JSON") from error


def _json_objects(path: Path, label: str) -> list[Any]:
    try:
        text = _regular_file(path, label).read_text(encoding="utf-8")
    except UnicodeDecodeError as error:
        raise ValueError(f"{label} must be UTF-8 JSON") from error
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        values = []
        for line_number, line in enumerate(text.splitlines(), start=1):
            if not line.strip():
                continue
            try:
                values.append(json.loads(line))
            except json.JSONDecodeError as error:
                raise ValueError(f"{label} line {line_number} must be JSON") from error
        if not values:
            raise ValueError(f"{label} must contain a JSON bundle")
        return values
    return value if isinstance(value, list) else [value]


def _dsse_envelopes(values: list[Any]) -> list[dict[str, Any]]:
    envelopes: list[dict[str, Any]] = []
    seen: set[int] = set()

    def add(envelope: dict[str, Any]) -> None:
        if id(envelope) not in seen:
            seen.add(id(envelope))
            envelopes.append(envelope)

    def visit(value: Any) -> None:
        if isinstance(value, dict):
            envelope = value.get("dsseEnvelope")
            if isinstance(envelope, dict):
                add(envelope)
            if isinstance(value.get("payload"), str) and (
                "payloadType" in value or "signatures" in value
            ):
                add(value)
            for key, child in value.items():
                if key != "dsseEnvelope":
                    visit(child)
        elif isinstance(value, list):
            for child in value:
                visit(child)

    for value in values:
        visit(value)
    return envelopes


def decode_attestation_subjects(path: Path) -> dict[str, str]:
    subjects: dict[str, str] = {}
    envelopes = _dsse_envelopes(_json_objects(path, f"attestation {path.name}"))
    if not envelopes:
        raise ValueError(f"attestation {path.name} has no DSSE envelope")
    for envelope in envelopes:
        payload = envelope.get("payload")
        if not isinstance(payload, str):
            raise ValueError(f"attestation {path.name} DSSE payload is missing")
        try:
            statement = json.loads(
                base64.b64decode(payload, validate=True).decode("utf-8")
            )
        except (binascii.Error, UnicodeDecodeError, json.JSONDecodeError) as error:
            raise ValueError(
                f"attestation {path.name} DSSE payload is invalid"
            ) from error
        statement = _mapping(statement, f"attestation {path.name} statement")
        raw_subjects = statement.get("subject")
        if not isinstance(raw_subjects, list) or not raw_subjects:
            raise ValueError(f"attestation {path.name} statement has no subjects")
        for subject in raw_subjects:
            subject = _mapping(subject, f"attestation {path.name} subject")
            name = _one_line(
                subject.get("name"), f"attestation {path.name} subject name"
            )
            if (
                name != Path(name).name
                or "/" in name
                or "\\" in name
                or name not in FULL_RELEASE_ASSET_NAMES
            ):
                raise ValueError(
                    f"attestation {path.name} subject is not a release basename"
                )
            digest = _mapping(
                subject.get("digest"), f"attestation {path.name} subject digest"
            )
            value = digest.get("sha256")
            if (
                not isinstance(value, str)
                or re.fullmatch(r"[0-9a-f]{64}", value) is None
            ):
                raise ValueError(f"attestation {path.name} subject digest is invalid")
            previous = subjects.get(name)
            if previous is not None and previous != value:
                raise ValueError(
                    f"attestation {path.name} repeats {name} with another digest"
                )
            subjects[name] = value
    return subjects


def _require_subjects(path: Path, expected: dict[str, str], label: str) -> None:
    if decode_attestation_subjects(path) != expected:
        raise ValueError(f"{label} subjects mismatch")


def _validate_file_record(
    value: Any, expected_keys: set[str], expected_url: str, label: str
) -> dict[str, Any]:
    record = _mapping(value, label)
    _exact_keys(record, expected_keys, label)
    _length(record.get("length"), f"{label} length")
    _sha256(record.get("sha256"), f"{label} sha256")
    if record.get("url") != expected_url:
        raise ValueError(f"{label} URL mismatch")
    return record


def _validate_digest_record(value: Any, label: str) -> dict[str, Any]:
    record = _mapping(value, label)
    _exact_keys(record, {"length", "sha256"}, label)
    _length(record.get("length"), f"{label} length")
    _sha256(record.get("sha256"), f"{label} sha256")
    return record


def _validate_named_digest_records(
    value: Any, names: tuple[str, ...], label: str
) -> dict[str, Any]:
    records = _mapping(value, label)
    _exact_keys(records, set(names), label)
    for name in names:
        _validate_digest_record(records[name], f"{label} {name}")
    return records


def _validate_semantic_verification_record(value: Any) -> dict[str, Any]:
    record = _mapping(value, "pair manifest semantic verification")
    _exact_keys(
        record,
        {"schema", "verifier", "source_archives", "inputs", "spdx"},
        "pair manifest semantic verification",
    )
    if record.get("schema") != SEMANTIC_VERIFICATION_SCHEMA:
        raise ValueError("pair manifest semantic verification schema mismatch")
    _validate_digest_record(record["verifier"], "pair manifest semantic verifier")
    _validate_named_digest_records(
        record["source_archives"],
        SOURCE_ARCHIVE_NAMES,
        "pair manifest semantic source archives",
    )
    inputs = _mapping(record["inputs"], "pair manifest semantic inputs")
    _exact_keys(
        inputs,
        {"cargo_metadata", "native_graph", "rules_rust_toolchains"},
        "pair manifest semantic inputs",
    )
    cargo_names = tuple(
        name
        for component in CARGO_METADATA_FILENAMES.values()
        for name in component.values()
    )
    _validate_named_digest_records(
        inputs["cargo_metadata"],
        cargo_names,
        "pair manifest semantic Cargo metadata",
    )
    _validate_digest_record(
        inputs["native_graph"], "pair manifest semantic native graph"
    )
    _validate_digest_record(
        inputs["rules_rust_toolchains"],
        "pair manifest semantic rules_rust toolchains",
    )
    _validate_digest_record(record["spdx"], "pair manifest semantic SPDX")
    return record


def _validate_declaration(value: Any, path: str, label: str) -> None:
    record = _mapping(value, label)
    _exact_keys(record, {"path", "length", "sha256"}, label)
    if record.get("path") != path:
        raise ValueError(f"{label} path mismatch")
    _length(record.get("length"), f"{label} length")
    _sha256(record.get("sha256"), f"{label} sha256")


def _validate_rules_rust_toolchains(value: Any, label: str) -> dict[str, Any]:
    record = _mapping(value, label)
    _exact_keys(
        record,
        {"version", "declaration", "toolchain_type", "resolved"},
        label,
    )
    _one_line(record.get("version"), f"{label} version")
    _validate_declaration(record["declaration"], "MODULE.bazel", f"{label} declaration")
    if record.get("toolchain_type") != "@@rules_rust+//rust:toolchain_type":
        raise ValueError(f"{label} toolchain type mismatch")
    resolved = _mapping(record.get("resolved"), f"{label} resolved toolchains")
    _exact_keys(resolved, set(OMP_ASSET_TARGETS), f"{label} resolved toolchains")
    for platform, labels in resolved.items():
        if (
            not isinstance(labels, list)
            or not labels
            or labels != sorted(set(labels))
            or any(
                not isinstance(item, str)
                or "\r" in item
                or "\n" in item
                or not item.startswith("@@rules_rust")
                or not item.endswith("//:rust_toolchain")
                for item in labels
            )
        ):
            raise ValueError(f"{label} resolved toolchains are invalid: {platform}")
    return record


def _validate_pair_manifest(value: Any) -> dict[str, Any]:
    document = _mapping(value, "pair manifest")
    _exact_keys(
        document,
        {
            "schema",
            "pair_id",
            "release",
            "sources",
            "toolchains",
            "lock_inputs",
            "platforms",
            "artifacts",
            "evidence",
            "verification",
        },
        "pair manifest",
    )
    if document.get("schema") != PAIR_MANIFEST_SCHEMA:
        raise ValueError("pair manifest schema mismatch")

    release = _mapping(document["release"], "pair manifest release")
    _exact_keys(
        release,
        {"repository", "tag", "build_id", "built_at", "immutable"},
        "pair manifest release",
    )
    repo = _repository(release.get("repository"), "pair manifest repository")
    if repo != HERDR_REPOSITORY:
        raise ValueError("pair manifest repository mismatch")
    tag = _one_line(release.get("tag"), "pair manifest tag")
    build_id = _one_line(release.get("build_id"), "pair manifest build_id")
    built_at = _timestamp(release.get("built_at"), "pair manifest built_at")
    if release.get("built_at") != built_at:
        raise ValueError("pair manifest built_at must use canonical UTC Z")
    if release.get("immutable") is not True:
        raise ValueError("pair manifest must require an immutable release")

    sources = _mapping(document["sources"], "pair manifest sources")
    _exact_keys(sources, {"parent", "herdr", "omp"}, "pair manifest sources")
    parent = _mapping(sources["parent"], "pair manifest parent source")
    _exact_keys(parent, {"repository", "commit", "tree"}, "pair manifest parent source")
    if (
        _repository(parent.get("repository"), "pair manifest parent repository")
        != PARENT_REPOSITORY
    ):
        raise ValueError("pair manifest parent repository mismatch")
    parent_commit = _git_object(parent.get("commit"), "pair manifest parent commit")
    _git_object(parent.get("tree"), "pair manifest parent tree")
    herdr = _mapping(sources["herdr"], "pair manifest Herdr source")
    _exact_keys(
        herdr,
        {"repository", "commit", "tree", "version", "build_id", "protocol"},
        "pair manifest Herdr source",
    )
    if (
        _repository(herdr.get("repository"), "pair manifest Herdr repository")
        != HERDR_REPOSITORY
    ):
        raise ValueError("pair manifest Herdr repository mismatch")
    herdr_commit = _git_object(herdr.get("commit"), "pair manifest Herdr commit")
    _git_object(herdr.get("tree"), "pair manifest Herdr tree")
    _one_line(herdr.get("version"), "pair manifest Herdr version")
    if (
        _one_line(herdr.get("build_id"), "pair manifest Herdr build_id")
        != release["build_id"]
    ):
        raise ValueError("pair manifest Herdr build_id mismatch")
    _protocol(herdr.get("protocol"))
    omp = _mapping(sources["omp"], "pair manifest OMP source")
    _exact_keys(omp, set(OMP_SOURCE_FIELDS), "pair manifest OMP source")
    if (
        _repository(omp.get("repository"), "pair manifest OMP repository")
        != OMP_REPOSITORY
    ):
        raise ValueError("pair manifest OMP repository mismatch")
    omp_commit = _git_object(omp.get("commit"), "pair manifest OMP commit")
    _git_object(omp.get("tree"), "pair manifest OMP tree")
    _one_line(omp.get("version"), "pair manifest OMP version")
    _one_line(omp.get("build_id"), "pair manifest OMP build_id")
    pair_id = _sha256(document.get("pair_id"), "pair manifest pair_id")
    if pair_id != pair_id_for_sources(parent_commit, herdr_commit, omp_commit):
        raise ValueError("pair manifest pair_id mismatch")
    _validate_paired_build_id(
        build_id, parent_commit, herdr_commit, omp_commit, built_at
    )
    if tag != f"smarty-preview-{build_id}":
        raise ValueError("pair manifest tag/build_id mismatch")

    toolchains = _mapping(document["toolchains"], "pair manifest toolchains")
    _exact_keys(toolchains, {"herdr", "omp"}, "pair manifest toolchains")
    herdr_tools = _mapping(toolchains["herdr"], "pair manifest Herdr toolchains")
    _exact_keys(
        herdr_tools, {"rust", "zig", "declarations"}, "pair manifest Herdr toolchains"
    )
    _one_line(herdr_tools.get("rust"), "pair manifest Herdr Rust")
    _one_line(herdr_tools.get("zig"), "pair manifest Zig")
    herdr_declarations = _mapping(
        herdr_tools["declarations"], "pair manifest Herdr declarations"
    )
    _exact_keys(herdr_declarations, {"rust"}, "pair manifest Herdr declarations")
    _validate_declaration(
        herdr_declarations["rust"],
        "rust-toolchain.toml",
        "pair manifest Herdr Rust declaration",
    )
    omp_tools = _mapping(toolchains["omp"], "pair manifest OMP toolchains")
    _exact_keys(
        omp_tools,
        {"bun", "bazel", "rust", "rules_rust", "declarations"},
        "pair manifest OMP toolchains",
    )
    for name in ("bun", "bazel", "rust"):
        _one_line(omp_tools.get(name), f"pair manifest OMP {name}")
    _validate_rules_rust_toolchains(
        omp_tools["rules_rust"], "pair manifest OMP rules_rust"
    )
    omp_declarations = _mapping(
        omp_tools["declarations"], "pair manifest OMP declarations"
    )
    _exact_keys(omp_declarations, {"bazel", "rust"}, "pair manifest OMP declarations")
    _validate_declaration(
        omp_declarations["bazel"],
        ".bazelversion",
        "pair manifest OMP Bazel declaration",
    )
    _validate_declaration(
        omp_declarations["rust"],
        "rust-toolchain.toml",
        "pair manifest OMP Rust declaration",
    )

    locks = _mapping(document["lock_inputs"], "pair manifest lock inputs")
    expected_locks = {f"{component}/{path}" for component, path in LOCK_INPUTS}
    _exact_keys(locks, expected_locks, "pair manifest lock inputs")
    for component, path in LOCK_INPUTS:
        label = f"pair manifest lock input {component}/{path}"
        record = _mapping(locks[f"{component}/{path}"], label)
        _exact_keys(record, {"component", "path", "length", "sha256"}, label)
        if record.get("component") != component or record.get("path") != path:
            raise ValueError(f"{label} identity mismatch")
        _length(record.get("length"), f"{label} length")
        _sha256(record.get("sha256"), f"{label} sha256")

    platforms = _mapping(document["platforms"], "pair manifest platforms")
    _exact_keys(platforms, set(PLATFORM_MATRIX), "pair manifest platforms")
    for target, expected in PLATFORM_MATRIX.items():
        label = f"pair manifest platform {target}"
        record = _mapping(platforms[target], label)
        _exact_keys(record, {"os", "architecture", "abi", "runner", "payloads"}, label)
        if any(
            record.get(field) != expected[field]
            for field in ("os", "architecture", "abi", "runner")
        ):
            raise ValueError(f"{label} identity mismatch")
        if record.get("payloads") != expected["payloads"]:
            raise ValueError(f"{label} payloads mismatch")

    artifacts = _mapping(document["artifacts"], "pair manifest artifacts")
    _exact_keys(artifacts, set(RELEASE_ASSET_NAMES), "pair manifest artifacts")
    for name in RELEASE_ASSET_NAMES:
        label = f"pair manifest artifact {name}"
        if name in PAYLOAD_METADATA_BY_NAME:
            expected = PAYLOAD_METADATA_BY_NAME[name]
            record = _validate_file_record(
                artifacts[name],
                set(expected) | {"length", "sha256", "url"},
                _release_asset_url(repo, tag, name),
                label,
            )
            if any(record.get(field) != expected[field] for field in expected):
                raise ValueError(f"{label} metadata mismatch")
        else:
            record = _validate_file_record(
                artifacts[name],
                {"kind", "payload", "length", "sha256", "url"},
                _release_asset_url(repo, tag, name),
                label,
            )
            if record.get("kind") != "sha256" or record.get(
                "payload"
            ) != name.removesuffix(".sha256"):
                raise ValueError(f"{label} metadata mismatch")

    evidence = _mapping(document["evidence"], "pair manifest evidence")
    _exact_keys(evidence, set(EVIDENCE_ASSET_NAMES), "pair manifest evidence")
    for name in EVIDENCE_ASSET_NAMES:
        _validate_file_record(
            evidence[name],
            {"length", "sha256", "url"},
            _release_asset_url(repo, tag, name),
            f"pair manifest evidence {name}",
        )
    _validate_semantic_verification_record(document["verification"])
    return document


def _release_asset_directory(asset_dir: Path) -> Path:
    asset_dir = Path(asset_dir)
    if not asset_dir.is_dir():
        raise ValueError("asset directory must be a directory")
    names = {path.name for path in asset_dir.iterdir()}
    if names != set(FULL_RELEASE_ASSET_NAMES):
        raise ValueError("release assets must exactly match the 37-name allow-list")
    for name in FULL_RELEASE_ASSET_NAMES:
        _asset_file(asset_dir, name)
    return asset_dir


def _spdx_file_checksums(document: dict[str, Any]) -> dict[str, dict[str, str]]:
    files = document.get("files")
    if not isinstance(files, list):
        raise ValueError("SPDX files must be a list")
    subjects: dict[str, dict[str, str]] = {}
    for file in files:
        file = _mapping(file, "SPDX file")
        name = _one_line(file.get("fileName"), "SPDX file name")
        checksums = file.get("checksums")
        if not isinstance(checksums, list):
            raise ValueError(f"SPDX file {name} checksums must be a list")
        values: dict[str, str] = {}
        for checksum in checksums:
            checksum = _mapping(checksum, f"SPDX file {name} checksum")
            algorithm = checksum.get("algorithm")
            if algorithm == "SHA1":
                value = _sha1(checksum.get("checksumValue"), f"SPDX file {name} SHA-1")
            elif algorithm == "SHA256":
                value = _sha256(
                    checksum.get("checksumValue"), f"SPDX file {name} SHA-256"
                )
            else:
                raise ValueError(f"SPDX file {name} has an unsupported checksum")
            if algorithm in values:
                raise ValueError(f"SPDX file {name} has multiple {algorithm} checksums")
            values[algorithm] = value
        if set(values) != {"SHA1", "SHA256"}:
            raise ValueError(f"SPDX file {name} must have SHA-1 and SHA-256 checksums")
        if name in subjects:
            raise ValueError(f"SPDX repeats file {name}")
        subjects[name] = values
    return subjects


def _spdx_subjects(document: dict[str, Any]) -> dict[str, str]:
    return {
        name: values["SHA256"]
        for name, values in _spdx_file_checksums(document).items()
    }


def _verify_spdx(
    path: Path,
    asset_dir: Path,
    built_at: str,
    parent_commit: str,
    herdr_commit: str,
    omp_commit: str,
    herdr_version: str,
    omp_version: str,
    herdr_root: Path,
    omp_root: Path,
    omp_bazel_graph: Path,
    cargo_metadata_dir: Path,
    expected_subjects: dict[str, str],
) -> None:
    document = _mapping(_json_file(path, "SPDX document"), "SPDX document")
    file_checksums = _spdx_file_checksums(document)
    if {
        name: values["SHA256"] for name, values in file_checksums.items()
    } != expected_subjects:
        raise ValueError("SPDX file subjects mismatch")
    for name in RELEASE_PAYLOAD_ASSET_NAMES:
        actual = _file_digests(_asset_file(asset_dir, name), f"asset {name}")
        if file_checksums[name]["SHA1"] != actual["sha1"]:
            raise ValueError(f"SPDX file SHA-1 mismatch: {name}")
        if file_checksums[name]["SHA256"] != actual["sha256"]:
            raise ValueError(f"SPDX file SHA-256 mismatch: {name}")
    expected = json.loads(
        build_spdx(
            asset_dir=asset_dir,
            built_at=built_at,
            parent_commit=parent_commit,
            herdr_commit=herdr_commit,
            omp_commit=omp_commit,
            herdr_version=herdr_version,
            omp_version=omp_version,
            herdr_root=herdr_root,
            omp_root=omp_root,
            omp_bazel_graph=omp_bazel_graph,
            cargo_metadata_dir=cargo_metadata_dir,
        )
    )
    if document != expected:
        raise ValueError("SPDX dependency inventory mismatch")


def _verify_declared_file(
    path: Path, record: dict[str, Any], label: str
) -> dict[str, Any]:
    actual = _file_record(path, label)
    if actual["length"] != record["length"]:
        raise ValueError(f"{label} length mismatch")
    if actual["sha256"] != record["sha256"]:
        raise ValueError(f"{label} digest mismatch")
    return actual


def verify_pair(
    asset_dir: Path,
    expected_parent: str,
    expected_source: str,
    expected_omp: str,
    expected_parent_tree: str,
    expected_source_tree: str,
    expected_omp_tree: str,
    expected_tag: str,
    expected_build_id: str,
    herdr_root: Path,
    omp_root: Path,
    trusted_verifier: Path,
    source_archive_dir: Path,
    omp_bazel_graph: Path | None,
    cargo_metadata_dir: Path | None,
    omp_rules_rust_toolchains: Path | None,
    trust_attested_verification: bool = False,
) -> dict[str, Any]:
    asset_dir = _release_asset_directory(Path(asset_dir))
    expected_parent = _git_object(expected_parent, "expected_parent")
    expected_source = _git_object(expected_source, "expected_source")
    expected_omp = _git_object(expected_omp, "expected_omp")
    expected_parent_tree = _git_object(expected_parent_tree, "expected_parent_tree")
    expected_source_tree = _git_object(expected_source_tree, "expected_source_tree")
    expected_omp_tree = _git_object(expected_omp_tree, "expected_omp_tree")
    expected_build_id = _validate_paired_build_id(
        expected_build_id, expected_parent, expected_source, expected_omp
    )
    expected_tag = _one_line(expected_tag, "expected_tag")
    if expected_tag != f"smarty-preview-{expected_build_id}":
        raise ValueError("expected tag/build_id mismatch")
    document = _validate_pair_manifest(
        _json_file(asset_dir / PAIR_MANIFEST_ASSET_NAME, "pair manifest")
    )
    release = document["release"]
    sources = document["sources"]
    expected_values = (
        (release["tag"], expected_tag, "release tag"),
        (release["build_id"], expected_build_id, "release build_id"),
        (sources["parent"]["commit"], expected_parent, "parent commit"),
        (sources["parent"]["tree"], expected_parent_tree, "parent tree"),
        (sources["herdr"]["commit"], expected_source, "Herdr commit"),
        (sources["herdr"]["tree"], expected_source_tree, "Herdr tree"),
        (sources["omp"]["commit"], expected_omp, "OMP commit"),
        (sources["omp"]["tree"], expected_omp_tree, "OMP tree"),
    )
    for actual, expected, label in expected_values:
        if actual != expected:
            raise ValueError(f"pair manifest {label} mismatch")
    herdr_root = Path(herdr_root)
    omp_root = Path(omp_root)
    expected_locks = _lock_input_records(herdr_root, omp_root)
    if document["lock_inputs"] != expected_locks:
        raise ValueError("pair manifest lock inputs do not match exact source trees")
    toolchains = document["toolchains"]
    expected_declarations = {
        "herdr": {
            "rust": _declaration_record(
                herdr_root, "rust-toolchain.toml", "Herdr rust-toolchain.toml"
            )
        },
        "omp": {
            "bazel": _declaration_record(
                omp_root, ".bazelversion", "OMP .bazelversion"
            ),
            "rust": _declaration_record(
                omp_root, "rust-toolchain.toml", "OMP rust-toolchain.toml"
            ),
        },
    }
    if toolchains["herdr"]["declarations"] != expected_declarations["herdr"]:
        raise ValueError("pair manifest Herdr declarations do not match source")
    if toolchains["omp"]["declarations"] != expected_declarations["omp"]:
        raise ValueError("pair manifest OMP declarations do not match source")
    rules_rust_version, rules_rust_declaration = _rules_rust_declaration(omp_root)
    rules_rust = toolchains["omp"]["rules_rust"]
    if (
        rules_rust["version"] != rules_rust_version
        or rules_rust["declaration"] != rules_rust_declaration
    ):
        raise ValueError("pair manifest OMP rules_rust declaration mismatch")
    if toolchains["herdr"]["rust"] != _toolchain_channel(
        herdr_root / "rust-toolchain.toml", "Herdr rust-toolchain.toml"
    ):
        raise ValueError("pair manifest Herdr Rust toolchain mismatch")
    if toolchains["herdr"]["zig"] != TRUSTED_ZIG_VERSION:
        raise ValueError("pair manifest Zig toolchain mismatch")
    if toolchains["omp"]["bazel"] != _one_line_file(
        omp_root / ".bazelversion", "OMP .bazelversion"
    ) or toolchains["omp"]["rust"] != _toolchain_channel(
        omp_root / "rust-toolchain.toml", "OMP rust-toolchain.toml"
    ):
        raise ValueError("pair manifest OMP toolchain mismatch")
    if toolchains["omp"]["bun"] != TRUSTED_BUN_VERSION:
        raise ValueError("pair manifest Bun toolchain mismatch")

    artifacts = document["artifacts"]
    payload_records: dict[str, dict[str, Any]] = {}
    for name in RELEASE_ASSET_NAMES:
        actual = _verify_declared_file(
            _asset_file(asset_dir, name), artifacts[name], f"asset {name}"
        )
        if name in PAYLOAD_METADATA_BY_NAME:
            payload_records[name] = actual
    _validate_checksum_sidecars(asset_dir, payload_records)
    evidence = document["evidence"]
    for name in EVIDENCE_ASSET_NAMES:
        _verify_declared_file(
            _asset_file(asset_dir, name), evidence[name], f"evidence {name}"
        )
    verification = document["verification"]
    if verification["verifier"] != _file_record(
        Path(trusted_verifier), "trusted release verifier"
    ):
        raise ValueError("pair manifest trusted verifier record mismatch")
    if verification["source_archives"] != _directory_file_records(
        Path(source_archive_dir), SOURCE_ARCHIVE_NAMES, "trusted source archives"
    ):
        raise ValueError("pair manifest trusted source archive records mismatch")
    if verification["spdx"] != _file_record(
        asset_dir / SPDX_ASSET_NAME, "verified SPDX document"
    ):
        raise ValueError("pair manifest verified SPDX record mismatch")
    if not trust_attested_verification:
        if (
            omp_bazel_graph is None
            or cargo_metadata_dir is None
            or omp_rules_rust_toolchains is None
        ):
            raise ValueError("complete semantic verification inputs are required")
        expected_document = json.loads(
            build_pair_manifest(
                asset_dir=asset_dir,
                repo=HERDR_REPOSITORY,
                tag=expected_tag,
                build_id=expected_build_id,
                built_at=release["built_at"],
                parent_repo=PARENT_REPOSITORY,
                parent_commit=expected_parent,
                parent_tree=expected_parent_tree,
                herdr_commit=expected_source,
                herdr_tree=expected_source_tree,
                base_version=sources["herdr"]["version"],
                protocol=sources["herdr"]["protocol"],
                omp_source=sources["omp"],
                herdr_root=herdr_root,
                omp_root=omp_root,
                omp_rules_rust_toolchains=Path(omp_rules_rust_toolchains),
                trusted_verifier=Path(trusted_verifier),
                source_archive_dir=Path(source_archive_dir),
                omp_bazel_graph=Path(omp_bazel_graph),
                cargo_metadata_dir=Path(cargo_metadata_dir),
                bun_version=TRUSTED_BUN_VERSION,
                zig_version=TRUSTED_ZIG_VERSION,
            )
        )
        if document != expected_document:
            raise ValueError(
                "pair manifest semantic claims do not match trusted reconstruction"
            )

    payload_digests = {
        name: payload_records[name]["sha256"] for name in RELEASE_PAYLOAD_ASSET_NAMES
    }
    if not trust_attested_verification:
        assert omp_bazel_graph is not None
        assert cargo_metadata_dir is not None
        _verify_spdx(
            asset_dir / SPDX_ASSET_NAME,
            asset_dir,
            release["built_at"],
            sources["parent"]["commit"],
            sources["herdr"]["commit"],
            sources["omp"]["commit"],
            f"{sources['herdr']['version']}-preview.{release['build_id']}",
            sources["omp"]["version"],
            herdr_root,
            omp_root,
            Path(omp_bazel_graph),
            Path(cargo_metadata_dir),
            payload_digests,
        )
    for target, name in PLATFORM_PROVENANCE_ASSET_NAMES.items():
        _require_subjects(
            asset_dir / name,
            {
                payload: payload_digests[payload]
                for payload in PLATFORM_PAYLOAD_ASSET_NAMES[target]
            },
            f"platform provenance {target}",
        )
    _require_subjects(
        asset_dir / SPDX_PROVENANCE_ASSET_NAME,
        {
            **payload_digests,
            SPDX_ASSET_NAME: _file_record(
                _asset_file(asset_dir, SPDX_ASSET_NAME), "SPDX document"
            )["sha256"],
        },
        "SBOM attestation",
    )
    _require_subjects(
        asset_dir / PAIR_PROVENANCE_ASSET_NAME,
        {
            PAIR_MANIFEST_ASSET_NAME: _file_record(
                _asset_file(asset_dir, PAIR_MANIFEST_ASSET_NAME), "pair manifest"
            )["sha256"]
        },
        "pair provenance",
    )
    return document


def cmd_notes(args: argparse.Namespace) -> int:
    previous = (
        args.previous
        or previous_preview_commit(Path(args.manifest))
        or latest_stable_tag()
    )
    notes = build_notes(
        previous, args.commit, args.build_id, args.base_version, args.repo
    )
    Path(args.output).write_text(notes, encoding="utf-8")
    return 0


def cmd_manifest(args: argparse.Namespace) -> int:
    if (args.omp_source is None) != (args.omp_sha_file is None):
        raise SystemExit("--omp-source and --omp-sha-file must be provided together")
    notes = Path(args.notes).read_text(encoding="utf-8")
    shas = read_sha_file(Path(args.sha_file) if args.sha_file else None)
    omp_source = read_omp_source(Path(args.omp_source)) if args.omp_source else None
    omp_shas = read_sha_file(Path(args.omp_sha_file)) if args.omp_sha_file else None
    content = build_manifest(
        output=Path(args.output),
        repo=args.repo,
        tag=args.tag,
        build_id=args.build_id,
        commit=args.commit,
        built_at=args.built_at,
        base_version=args.base_version,
        protocol=args.protocol,
        notes=notes,
        shas=shas,
        retain=args.retain,
        omp_source=omp_source,
        omp_shas=omp_shas,
    )
    Path(args.output).write_text(content, encoding="utf-8")
    return 0


def cmd_legacy_bootstrap_manifest(args: argparse.Namespace) -> int:
    paired = _json_file(Path(args.paired), "paired preview manifest")
    content = build_legacy_bootstrap_manifest(
        _mapping(paired, "paired preview manifest")
    )
    Path(args.output).write_text(content, encoding="utf-8")
    return 0


def cmd_promote_bootstrap_manifest(args: argparse.Namespace) -> int:
    bridge = _json_file(Path(args.bridge), "legacy bootstrap manifest")
    content = (
        json.dumps(canonical_manifest_from_legacy_bootstrap(bridge), indent=2) + "\n"
    )
    Path(args.output).write_text(content, encoding="utf-8")
    return 0


def cmd_rules_rust_report(args: argparse.Namespace) -> int:
    log = _regular_file(Path(args.log), "Bazel rules_rust resolution log").read_text(
        encoding="utf-8"
    )
    Path(args.output).write_text(
        build_rules_rust_toolchain_report(log, args.platform), encoding="utf-8"
    )
    return 0


def cmd_spdx(args: argparse.Namespace) -> int:
    content = build_spdx(
        asset_dir=Path(args.asset_dir),
        built_at=args.built_at,
        parent_commit=args.parent_commit,
        herdr_commit=args.herdr_commit,
        omp_commit=args.omp_commit,
        herdr_version=args.herdr_version,
        omp_version=args.omp_version,
        herdr_root=Path(args.herdr_root),
        omp_root=Path(args.omp_root),
        omp_bazel_graph=Path(args.omp_bazel_graph),
        cargo_metadata_dir=Path(args.cargo_metadata_dir),
    )
    Path(args.output).write_text(content, encoding="utf-8")
    return 0


def cmd_pair_manifest(args: argparse.Namespace) -> int:
    content = build_pair_manifest(
        asset_dir=Path(args.asset_dir),
        repo=args.repo,
        tag=args.tag,
        build_id=args.build_id,
        built_at=args.built_at,
        parent_repo=args.parent_repo,
        parent_commit=args.parent_commit,
        parent_tree=args.parent_tree,
        herdr_commit=args.herdr_commit,
        herdr_tree=args.herdr_tree,
        base_version=args.base_version,
        protocol=args.protocol,
        omp_source=read_omp_source(Path(args.omp_source)),
        herdr_root=Path(args.herdr_root),
        omp_root=Path(args.omp_root),
        omp_rules_rust_toolchains=Path(args.omp_rules_rust_toolchains),
        trusted_verifier=Path(args.trusted_verifier),
        source_archive_dir=Path(args.source_archive_dir),
        omp_bazel_graph=Path(args.omp_bazel_graph),
        cargo_metadata_dir=Path(args.cargo_metadata_dir),
        bun_version=args.bun_version,
        zig_version=args.zig_version,
    )
    Path(args.output).write_text(content, encoding="utf-8")
    return 0


def cmd_verify_pair(args: argparse.Namespace) -> int:
    try:
        verify_pair(
            asset_dir=Path(args.asset_dir),
            expected_parent=args.expected_parent,
            expected_source=args.expected_source,
            expected_omp=args.expected_omp,
            expected_parent_tree=args.expected_parent_tree,
            expected_source_tree=args.expected_source_tree,
            expected_omp_tree=args.expected_omp_tree,
            expected_tag=args.expected_tag,
            expected_build_id=args.expected_build_id,
            herdr_root=Path(args.herdr_root),
            omp_root=Path(args.omp_root),
            trusted_verifier=Path(args.trusted_verifier),
            source_archive_dir=Path(args.source_archive_dir),
            omp_bazel_graph=(
                Path(args.omp_bazel_graph) if args.omp_bazel_graph is not None else None
            ),
            cargo_metadata_dir=(
                Path(args.cargo_metadata_dir)
                if args.cargo_metadata_dir is not None
                else None
            ),
            omp_rules_rust_toolchains=(
                Path(args.omp_rules_rust_toolchains)
                if args.omp_rules_rust_toolchains is not None
                else None
            ),
            trust_attested_verification=args.trust_attested_verification,
        )
    except ValueError as error:
        raise SystemExit(str(error)) from error
    return 0


def cmd_current_commit(args: argparse.Namespace) -> int:
    commit = previous_preview_commit(Path(args.manifest))
    if commit:
        print(commit)
    return 0


def cmd_select_commit(args: argparse.Namespace) -> int:
    print(latest_publishable_commit(args.ref))
    return 0


def cmd_range_base(args: argparse.Namespace) -> int:
    print(preview_range_base(args.previous, args.commit))
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Preview channel release helpers")
    sub = parser.add_subparsers(required=True)

    notes = sub.add_parser("notes")
    notes.add_argument("--manifest", default="website/preview.json")
    notes.add_argument("--previous")
    notes.add_argument("--commit", required=True)
    notes.add_argument("--build-id", required=True)
    notes.add_argument("--base-version", required=True)
    notes.add_argument("--repo", default="herdrdev/herdr")
    notes.add_argument("--output", required=True)
    notes.set_defaults(func=cmd_notes)

    manifest = sub.add_parser("manifest")
    manifest.add_argument("--output", default="website/preview.json")
    manifest.add_argument("--repo", default="herdrdev/herdr")
    manifest.add_argument("--tag", required=True)
    manifest.add_argument("--build-id", required=True)
    manifest.add_argument("--commit", required=True)
    manifest.add_argument("--built-at", required=True)
    manifest.add_argument("--base-version", required=True)
    manifest.add_argument("--protocol", required=True, type=int)
    manifest.add_argument("--notes", required=True)
    manifest.add_argument("--sha-file")
    manifest.add_argument("--omp-source")
    manifest.add_argument("--omp-sha-file")
    manifest.add_argument("--retain", type=int, default=30)
    manifest.set_defaults(func=cmd_manifest)

    bootstrap = sub.add_parser("legacy-bootstrap-manifest")
    bootstrap.add_argument("--paired", required=True)
    bootstrap.add_argument("--output", required=True)
    bootstrap.set_defaults(func=cmd_legacy_bootstrap_manifest)

    promotion = sub.add_parser("promote-bootstrap-manifest")
    promotion.add_argument("--bridge", required=True)
    promotion.add_argument("--output", required=True)
    promotion.set_defaults(func=cmd_promote_bootstrap_manifest)

    rules_rust_report = sub.add_parser("rules-rust-report")
    rules_rust_report.add_argument("--log", required=True)
    rules_rust_report.add_argument("--platform", required=True)
    rules_rust_report.add_argument("--output", required=True)
    rules_rust_report.set_defaults(func=cmd_rules_rust_report)

    spdx = sub.add_parser("spdx")
    spdx.add_argument("--output", required=True)
    spdx.add_argument("--asset-dir", required=True)
    spdx.add_argument("--built-at", required=True)
    spdx.add_argument("--parent-commit", required=True)
    spdx.add_argument("--herdr-commit", required=True)
    spdx.add_argument("--omp-commit", required=True)
    spdx.add_argument("--herdr-version", required=True)
    spdx.add_argument("--omp-version", required=True)
    spdx.add_argument("--herdr-root", required=True)
    spdx.add_argument("--omp-root", required=True)
    spdx.add_argument(
        "--omp-bazel-graph", "--native-graph", dest="omp_bazel_graph", required=True
    )
    spdx.add_argument(
        "--cargo-metadata-dir",
        "--dependency-metadata",
        dest="cargo_metadata_dir",
        required=True,
    )
    spdx.set_defaults(func=cmd_spdx)

    pair_manifest = sub.add_parser("pair-manifest")
    pair_manifest.add_argument("--output", required=True)
    pair_manifest.add_argument("--asset-dir", required=True)
    pair_manifest.add_argument("--repo", default="Smarty-Pants-Inc/herdr")
    pair_manifest.add_argument("--tag", required=True)
    pair_manifest.add_argument("--build-id", required=True)
    pair_manifest.add_argument("--built-at", required=True)
    pair_manifest.add_argument("--parent-repo", default="Smarty-Pants-Inc/smarty-dev")
    pair_manifest.add_argument("--parent-commit", required=True)
    pair_manifest.add_argument("--parent-tree", required=True)
    pair_manifest.add_argument("--herdr-commit", required=True)
    pair_manifest.add_argument("--herdr-tree", required=True)
    pair_manifest.add_argument("--base-version", required=True)
    pair_manifest.add_argument("--protocol", required=True, type=int)
    pair_manifest.add_argument(
        "--omp-source", "--omp-descriptor", dest="omp_source", required=True
    )
    pair_manifest.add_argument("--herdr-root", required=True)
    pair_manifest.add_argument("--omp-root", required=True)
    pair_manifest.add_argument("--omp-rules-rust-toolchains", required=True)
    pair_manifest.add_argument("--trusted-verifier", required=True)
    pair_manifest.add_argument("--source-archive-dir", required=True)
    pair_manifest.add_argument(
        "--omp-bazel-graph", "--native-graph", dest="omp_bazel_graph", required=True
    )
    pair_manifest.add_argument(
        "--cargo-metadata-dir",
        "--dependency-metadata",
        dest="cargo_metadata_dir",
        required=True,
    )
    pair_manifest.add_argument("--bun-version", default=TRUSTED_BUN_VERSION)
    pair_manifest.add_argument("--zig-version", default=TRUSTED_ZIG_VERSION)
    pair_manifest.set_defaults(func=cmd_pair_manifest)

    def add_verify_arguments(command: argparse.ArgumentParser) -> None:
        command.add_argument("--asset-dir", required=True)
        command.add_argument("--expected-parent", required=True)
        command.add_argument("--expected-source", required=True)
        command.add_argument("--expected-omp", required=True)
        command.add_argument("--expected-parent-tree", required=True)
        command.add_argument("--expected-source-tree", required=True)
        command.add_argument("--expected-omp-tree", required=True)
        command.add_argument("--expected-tag", required=True)
        command.add_argument("--expected-build-id", required=True)
        command.add_argument("--herdr-root", required=True)
        command.add_argument("--omp-root", required=True)
        command.add_argument("--trusted-verifier", required=True)
        command.add_argument("--source-archive-dir", required=True)

    verify = sub.add_parser("verify-pair")
    add_verify_arguments(verify)
    verify.add_argument(
        "--omp-bazel-graph", "--native-graph", dest="omp_bazel_graph", required=True
    )
    verify.add_argument(
        "--cargo-metadata-dir",
        "--dependency-metadata",
        dest="cargo_metadata_dir",
        required=True,
    )
    verify.add_argument("--omp-rules-rust-toolchains", required=True)
    verify.set_defaults(func=cmd_verify_pair, trust_attested_verification=False)

    attested = sub.add_parser("verify-attested-pair")
    add_verify_arguments(attested)
    attested.set_defaults(
        func=cmd_verify_pair,
        trust_attested_verification=True,
        omp_bazel_graph=None,
        cargo_metadata_dir=None,
        omp_rules_rust_toolchains=None,
    )

    current = sub.add_parser("current-commit")
    current.add_argument("--manifest", default="website/preview.json")
    current.set_defaults(func=cmd_current_commit)

    select = sub.add_parser("select-commit")
    select.add_argument("--ref", default="origin/master")
    select.set_defaults(func=cmd_select_commit)

    range_base = sub.add_parser("range-base")
    range_base.add_argument("--previous", required=True)
    range_base.add_argument("--commit", required=True)
    range_base.set_defaults(func=cmd_range_base)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
