#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import re
import subprocess
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
OMP_ASSET_TARGETS = tuple(OMP_EXPECTED_ASSET_NAMES)
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
    return isinstance(assets, dict) and set(assets) == set(ASSET_TARGETS) and all(
        valid_retained_asset(assets.get(target)) for target in ASSET_TARGETS
    )


def retained_build_has_verifiable_omp_pair(build: Any) -> bool:
    if not retained_build_has_verifiable_assets(build):
        return False
    omp = build.get("omp")
    if not isinstance(omp, dict):
        return False
    for field in ("build_id", "version"):
        value = omp.get(field)
        if not isinstance(value, str) or not value.strip() or "\n" in value or "\r" in value:
            return False
    for field in ("commit", "tree"):
        value = omp.get(field)
        if not isinstance(value, str) or re.fullmatch(r"[0-9a-fA-F]{40}", value) is None:
            return False
    omp_assets = omp.get("assets")
    if not isinstance(omp_assets, dict) or set(omp_assets) != set(OMP_ASSET_TARGETS):
        return False
    return all(valid_retained_asset(omp_assets.get(target)) for target in OMP_ASSET_TARGETS)


def retained_build_is_verifiable(build: Any) -> bool:
    return retained_build_has_verifiable_assets(build) and (
        "omp" not in build or retained_build_has_verifiable_omp_pair(build)
    )


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
    if (omp_source is None) != (omp_shas is None):
        raise ValueError(
            "OMP source descriptor and SHA targets must be provided together"
        )
    urls = default_asset_urls(repo, tag)
    assets = asset_objects(urls, shas)
    omp = None
    if omp_source is not None and omp_shas is not None:
        omp = omp_metadata(omp_source, repo, tag, omp_shas)
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
    ordered_builds = {
        key: builds[key]
        for key in sorted(
            builds,
            key=lambda key: str(builds[key].get("built_at", "")),
            reverse=True,
        )[:retain]
    }
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
    return json.dumps(manifest, indent=2) + "\n"


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
