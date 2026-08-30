#!/usr/bin/env python3
"""Default-branch trusted checks for the Smarty Preview publisher.

This module imports no candidate code and never executes artifact-provided
Python. Candidate artifacts are data until the trusted workflow has validated
run identity, source closure, and exact bytes.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timezone
import io
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path, PurePosixPath, PureWindowsPath
from typing import Any
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

REPOSITORY = "Smarty-Pants-Inc/herdr"
WORKFLOW_NAME = "Smarty Preview"
WORKFLOW_PATH = ".github/workflows/smarty-preview.yml"
PUBLISH_WORKFLOW_NAME = "Smarty Preview Trusted Publisher"
PUBLISH_WORKFLOW_PATH = ".github/workflows/smarty-preview-publish.yml"
PARENT_REPOSITORY = "Smarty-Pants-Inc/smarty-dev"
OMP_REPOSITORY = "Smarty-Pants-Inc/oh-my-pi"
HEX40 = re.compile(r"[0-9a-f]{40}")
HEX64 = re.compile(r"[0-9a-f]{64}")
PAIRED_TAG = re.compile(
    r"^smarty-preview-(?P<day>\d{4}-\d{2}-\d{2})-p(?P<parent>[0-9a-f]{40})-"
    r"r(?P<source>[0-9a-f]{40})-o(?P<omp>[0-9a-f]{40})$"
)
LEGACY_BUILD_ID = re.compile(r"^(?P<day>\d{4}-\d{2}-\d{2})-[0-9a-f]{12}$")
HERDR_ASSETS = {
    "linux-x86_64": "herdr-linux-x86_64",
    "linux-aarch64": "herdr-linux-aarch64",
    "macos-x86_64": "herdr-macos-x86_64",
    "macos-aarch64": "herdr-macos-aarch64",
    "windows-x86_64": "herdr-windows-x86_64.zip",
}
OMP_ASSETS = {
    "linux-x86_64": "omp-linux-x86_64",
    "linux-aarch64": "omp-linux-aarch64",
    "macos-x86_64": "omp-macos-x86_64",
    "macos-aarch64": "omp-macos-aarch64",
}
NATIVE_ASSETS = (
    "pi_natives.linux-x64-baseline.node",
    "pi_natives.linux-x64-modern.node",
    "pi_natives.linux-arm64.node",
    "pi_natives.darwin-x64-baseline.node",
    "pi_natives.darwin-arm64.node",
)
PAYLOAD_ASSETS = tuple(HERDR_ASSETS.values()) + tuple(OMP_ASSETS.values()) + NATIVE_ASSETS
SIDECAR_ASSETS = tuple(f"{name}.sha256" for name in PAYLOAD_ASSETS)
PROVENANCE_ASSETS = tuple(f"smarty-provenance-{platform}.sigstore.json" for platform in HERDR_ASSETS)
FULL_RELEASE_ASSETS = (
    *PAYLOAD_ASSETS, *SIDECAR_ASSETS, "smarty-pair.json", "smarty-pair.spdx.json",
    *PROVENANCE_ASSETS, "smarty-pair.provenance.sigstore.json", "smarty-pair.spdx.sigstore.json",
)
SEALED_INPUT_ASSETS = (*PAYLOAD_ASSETS, *SIDECAR_ASSETS, "smarty-pair.spdx.json")
PRODUCER_ARTIFACT_KINDS = (
    "smarty-release-plan", "smarty-candidate-sources", "candidate-linux-x86_64",
    "candidate-linux-aarch64", "candidate-macos-x86_64", "candidate-macos-aarch64",
    "candidate-windows-x86_64", "smarty-candidate-handoff",
)
MAX_JSON_BYTES = 16 * 1024 * 1024
MAX_SIDECAR_BYTES = 256
ARTIFACT_NAME = re.compile(
    rf"^(?:{'|'.join(re.escape(kind) for kind in PRODUCER_ARTIFACT_KINDS)})-(?P<attempt>[1-9][0-9]*)$"
)
MAX_TAR_MEMBERS = 200_000
MAX_ZIP_MEMBERS = 200_000
MAX_ZIP_EXPANDED_BYTES = 4 * 1024 * 1024 * 1024
MAX_TAR_EXPANDED_BYTES = 4 * 1024 * 1024 * 1024
MAX_TOTAL_EXTRACTED_BYTES = 16 * 1024 * 1024 * 1024
MAX_PROTOCOL_SOURCE_BYTES = 1 * 1024 * 1024
MAX_FILE_BYTES = 4 * 1024 * 1024 * 1024
MAX_TOTAL_DOWNLOAD_BYTES = 16 * 1024 * 1024 * 1024


def _fail(message: str) -> None:
    raise ValueError(message)


def _mapping(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail(f"{label} must be an object")
    return value


def _scalar(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value or value != value.strip() or "\n" in value or "\r" in value:
        _fail(f"{label} must be one nonempty line")
    return value


def _positive_int(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        _fail(f"{label} must be a positive integer")
    return value


def _hex(value: Any, label: str, pattern: re.Pattern[str] = HEX40) -> str:
    value = _scalar(value, label)
    if pattern.fullmatch(value) is None:
        _fail(f"{label} must be lowercase hexadecimal")
    return value


def managed_omp_build_id(tree: Any) -> str:
    return f"managed-omp-{_hex(tree, 'OMP tree')}"


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(",", ":")).encode("utf-8") + b"\n"


def write_canonical(path: Path, value: Any) -> dict[str, int | str]:
    data = canonical_bytes(value)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)
    return {"length": len(data), "sha256": hashlib.sha256(data).hexdigest()}


def normalize_built_at(raw: str) -> str:
    raw = _scalar(raw, "source commit timestamp")
    try:
        parsed = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError as error:
        raise ValueError("source commit timestamp is invalid") from error
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        _fail("source commit timestamp must include a timezone")
    return parsed.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _api_timestamp(value: Any, label: str) -> datetime:
    raw = _scalar(value, label)
    try:
        parsed = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    except ValueError as error:
        raise ValueError(f"{label} is invalid") from error
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        _fail(f"{label} must include a timezone")
    return parsed.astimezone(timezone.utc)


def paired_identity(tag: str, *, source_sha: str | None = None, built_at: str | None = None) -> dict[str, str]:
    tag = _scalar(tag, "tag")
    match = PAIRED_TAG.fullmatch(tag)
    if match is None:
        _fail("tag is not an exact smarty-preview P/R/O identity")
    try:
        datetime.strptime(match.group("day"), "%Y-%m-%d")
    except ValueError as error:
        raise ValueError("tag contains an invalid calendar day") from error
    if source_sha is not None and source_sha != match.group("source"):
        _fail("tag source R does not match workflow head SHA")
    if built_at is not None and match.group("day") != normalize_built_at(built_at)[:10]:
        _fail("tag day does not match normalized source commit time")
    return {"tag": tag, "build_id": tag.removeprefix("smarty-preview-"), "day": match.group("day"),
            "parent": match.group("parent"), "source": match.group("source"), "omp": match.group("omp")}


def validate_tag_object(tag_object: dict[str, Any], tag: str, source_sha: str) -> dict[str, str | int]:
    tag_object = _mapping(tag_object, "tag API response")
    identity = paired_identity(tag, source_sha=source_sha)
    if tag_object.get("ref") != f"refs/tags/{identity['tag']}":
        _fail("tag API ref is not the exact requested tag")
    target = _mapping(tag_object.get("object"), "tag API target")
    if target.get("type") != "commit" or target.get("sha") != identity["source"]:
        _fail("preview tag is not one lightweight ref directly at R")
    return {"schema": 1, **identity, "ref": tag_object["ref"], "object": {"type": "commit", "sha": target["sha"]}}


def validate_legacy_day(build_id: str, built_at: str) -> None:
    match = LEGACY_BUILD_ID.fullmatch(_scalar(build_id, "legacy build ID"))
    if match is None:
        _fail("legacy build ID is malformed")
    if match.group("day") != _scalar(built_at, "built_at")[:10]:
        _fail("legacy build day does not equal literal built_at date")


def file_record(path: Path) -> dict[str, int | str]:
    if not path.is_file() or path.is_symlink():
        _fail(f"not a regular file: {path}")
    size = path.stat().st_size
    if size < 0 or size > MAX_FILE_BYTES:
        _fail(f"file exceeds the bounded size limit: {path}")
    digest = hashlib.sha256()
    total = 0
    with path.open("rb") as source:
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if total > MAX_FILE_BYTES:
                _fail(f"file exceeds the bounded size limit: {path}")
            digest.update(chunk)
    if total != size:
        _fail(f"file changed while hashing: {path}")
    return {"length": total, "sha256": digest.hexdigest()}


def _artifact_names(attempt: int) -> tuple[str, ...]:
    return tuple(f"{kind}-{_positive_int(attempt, 'run attempt')}" for kind in PRODUCER_ARTIFACT_KINDS)


def _artifact_url(repository: str, artifact_id: int) -> str:
    return f"https://api.github.com/repos/{repository}/actions/artifacts/{artifact_id}/zip"


def validate_publisher_identity(workflow: dict[str, Any], content: dict[str, Any], *, default_branch: str,
                                checked_out_commit: str, checked_out_blob: str,
                                branch: dict[str, Any] | None = None,
                                revision: dict[str, Any] | None = None) -> dict[str, Any]:
    workflow = _mapping(workflow, "publisher workflow")
    content = _mapping(content, "publisher workflow content")
    workflow_id = _positive_int(workflow.get("id"), "publisher workflow ID")
    if workflow.get("name") != PUBLISH_WORKFLOW_NAME or workflow.get("path") != PUBLISH_WORKFLOW_PATH or workflow.get("state") != "active":
        _fail("publisher workflow API identity mismatch")
    default_branch = _scalar(default_branch, "default branch")
    checked_out_commit = _hex(checked_out_commit, "checked-out publisher commit")
    checked_out_blob = _hex(checked_out_blob, "checked-out publisher workflow blob")
    if content.get("type") != "file" or content.get("path") != PUBLISH_WORKFLOW_PATH:
        _fail("publisher workflow content path/type mismatch")
    if _hex(content.get("sha"), "publisher workflow content blob") != checked_out_blob:
        _fail("publisher workflow API blob differs from checked-out definition")
    if content.get("commit_sha") is not None and content.get("commit_sha") != checked_out_commit:
        _fail("publisher workflow content commit differs from checked-out revision")
    if branch is not None:
        branch = _mapping(branch, "default branch API response")
        branch_commit = _mapping(branch.get("commit"), "default branch commit")
        if branch.get("name") != default_branch or branch_commit.get("sha") != checked_out_commit:
            _fail("publisher workflow revision is not the default-branch revision")
    if revision is not None:
        revision = _mapping(revision, "publisher workflow revision")
        if revision.get("sha") != checked_out_commit:
            _fail("publisher workflow API revision differs from checked-out revision")
        revision_path = revision.get("path")
        if revision_path is not None and revision_path != PUBLISH_WORKFLOW_PATH:
            _fail("publisher workflow API revision path mismatch")
        revision_blob = revision.get("blob_sha")
        if revision_blob is not None and revision_blob != checked_out_blob:
            _fail("publisher workflow API revision blob mismatch")
    return {"id": workflow_id, "name": PUBLISH_WORKFLOW_NAME, "path": PUBLISH_WORKFLOW_PATH,
            "state": "active", "default_branch": default_branch, "commit": checked_out_commit, "blob": checked_out_blob}





def validate_workflow_run(run: dict[str, Any], workflow: dict[str, Any], artifacts: dict[str, Any], *,
                          repository: str = REPOSITORY, run_id: int | None = None,
                          event_run_attempt: int | None = None,
                          publisher: dict[str, Any] | None = None) -> dict[str, Any]:
    run = _mapping(run, "workflow run")
    workflow = _mapping(workflow, "producer workflow")
    artifacts = _mapping(artifacts, "artifact response")
    repository = _scalar(repository, "repository")
    actual_run_id = _positive_int(run.get("id"), "workflow run ID")
    if run_id is None or actual_run_id != _positive_int(run_id, "requested run ID"):
        _fail("workflow run ID mismatch")
    run_repository = _mapping(run.get("repository"), "workflow run repository")
    if run_repository.get("full_name") != repository:
        _fail("workflow run repository mismatch")
    if run.get("event") != "push" or run.get("status") != "completed" or run.get("conclusion") != "success":
        _fail("producer run must be one successful completed push")
    if run.get("name") != WORKFLOW_NAME or run.get("path") != WORKFLOW_PATH:
        _fail("producer workflow name/path mismatch")
    workflow_id = _positive_int(workflow.get("id"), "producer workflow ID")
    if run.get("workflow_id") != workflow_id or workflow.get("name") != WORKFLOW_NAME or workflow.get("path") != WORKFLOW_PATH:
        _fail("producer workflow API identity mismatch")
    attempt = _positive_int(run.get("run_attempt"), "producer run attempt")
    if event_run_attempt is not None and attempt != _positive_int(event_run_attempt, "event workflow run attempt"):
        _fail("workflow run attempt does not match workflow_run event")
    head_repository = _mapping(run.get("head_repository"), "producer head repository")
    if head_repository.get("full_name") != repository:
        _fail("producer head repository mismatch")
    source_sha = _hex(run.get("head_sha"), "producer head SHA")
    head_commit = _mapping(run.get("head_commit"), "producer head commit")
    if head_commit.get("id") != source_sha:
        _fail("producer head commit mismatch")
    built_at = normalize_built_at(_scalar(head_commit.get("timestamp"), "producer head commit timestamp"))
    run_started_at = _api_timestamp(run.get("run_started_at"), "workflow run start timestamp")
    tag = _scalar(run.get("head_branch"), "producer head ref")
    identity = paired_identity(tag, source_sha=source_sha, built_at=built_at)
    # The workflow-run API exposes head_branch; tag-object validation below binds it to the exact ref.
    values = artifacts.get("artifacts")
    if not isinstance(values, list) or artifacts.get("total_count") != len(values):
        _fail("artifact API response count is invalid")
    expected_names = _artifact_names(attempt)
    by_name: dict[str, dict[str, Any]] = {}
    for value in values:
        item = _mapping(value, "artifact record")
        name = _scalar(item.get("name"), "artifact name")
        match = ARTIFACT_NAME.fullmatch(name)
        if match is None:
            _fail(f"producer artifact is outside the recognized attempt namespace: {name}")
        artifact_attempt = int(match.group("attempt"))
        if artifact_attempt > attempt:
            _fail(f"producer artifact belongs to a future run attempt: {name}")
        if name in by_name:
            _fail(f"duplicate producer artifact: {name}")
        by_name[name] = item
    current = {name: item for name, item in by_name.items() if name in expected_names}
    if set(current) != set(expected_names):
        _fail("producer artifact allow-list mismatch")
    by_name = current
    records: dict[str, dict[str, Any]] = {}
    total_artifact_bytes = 0
    for name in expected_names:
        item = by_name[name]
        artifact_id = _positive_int(item.get("id"), f"artifact ID {name}")
        if item.get("expired") is not False:
            _fail(f"producer artifact is expired or lacks explicit expiry: {name}")
        size = _positive_int(item.get("size_in_bytes"), f"artifact size {name}")
        if size > MAX_FILE_BYTES or total_artifact_bytes > MAX_TOTAL_DOWNLOAD_BYTES - size:
            _fail("producer artifacts exceed the bounded download limit")
        total_artifact_bytes += size
        digest = _scalar(item.get("digest"), f"artifact digest {name}")
        if not digest.startswith("sha256:") or HEX64.fullmatch(digest[7:]) is None:
            _fail(f"artifact digest is not sha256:<64-hex>: {name}")
        if item.get("archive_download_url") != _artifact_url(repository, artifact_id):
            _fail(f"artifact download URL is not canonical: {name}")
        owner = _mapping(item.get("workflow_run"), f"artifact workflow run {name}")
        if owner.get("id") != actual_run_id:
            _fail(f"artifact workflow identity mismatch: {name}")
        owner_attempt = owner.get("run_attempt")
        if owner_attempt is not None and owner_attempt != attempt:
            _fail(f"artifact workflow attempt mismatch: {name}")
        created_at = _api_timestamp(item.get("created_at"), f"artifact creation timestamp {name}")
        if created_at < run_started_at:
            _fail(f"artifact predates the current workflow attempt: {name}")
        updated_at = item.get("updated_at")
        if updated_at is not None and _api_timestamp(updated_at, f"artifact update timestamp {name}") < run_started_at:
            _fail(f"artifact update predates the current workflow attempt: {name}")
        records[name] = {"id": artifact_id, "name": name, "size_in_bytes": size, "digest": digest,
                         "archive_download_url": item["archive_download_url"], "run_id": actual_run_id, "run_attempt": attempt}
    result = {"schema": 1, "repository": repository, "run_id": actual_run_id, "run_attempt": attempt,
              "event_run_attempt": attempt, "workflow_id": workflow_id, "workflow_name": WORKFLOW_NAME, "workflow_path": WORKFLOW_PATH,
              "event": "push", "conclusion": "success", "built_at": built_at, **identity, "artifacts": records}
    if publisher is not None:
        result["publisher"] = validate_publisher_identity(**publisher)
    return result




def _read_bounded_bytes(path: Path, label: str, limit: int) -> bytes:
    if path.is_symlink() or not path.is_file():
        _fail(f"{label} is not a regular file")
    try:
        if path.stat().st_size > limit:
            _fail(f"{label} exceeds the bounded read limit")
        with path.open("rb") as source:
            data = source.read(limit + 1)
    except OSError as error:
        raise ValueError(f"{label} cannot be read") from error
    if len(data) > limit:
        _fail(f"{label} exceeds the bounded read limit")
    return data


def _load_json(path: Path, label: str) -> Any:
    try:
        return json.loads(_read_bounded_bytes(path, label, MAX_JSON_BYTES).decode("utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"{label} is not valid UTF-8 JSON") from error


class _HttpsArtifactRedirectHandler(HTTPRedirectHandler):
    def redirect_request(self, req: Request, fp: Any, code: int, msg: str,
                         headers: Any, newurl: str) -> Request | None:
        target = urlsplit(newurl)
        if target.scheme != "https" or not target.hostname or target.username is not None or target.password is not None:
            _fail("artifact redirect target must be an authenticated-free HTTPS URL")
        redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
        if redirected is not None and urlsplit(req.full_url).netloc != target.netloc:
            redirected.remove_header("Authorization")
        return redirected


def download_artifacts(root: Path, identity: dict[str, Any], token: str) -> None:
    if not token:
        _fail("GH_TOKEN is required to download producer artifacts")
    artifacts = _mapping(identity.get("artifacts"), "producer artifacts")
    records: dict[str, dict[str, Any]] = {}
    declared_total = 0
    for name, value in artifacts.items():
        if not isinstance(name, str) or ARTIFACT_NAME.fullmatch(name) is None:
            _fail(f"producer artifact is outside the recognized attempt namespace: {name}")
        record = _mapping(value, f"artifact record {name}")
        size = _positive_int(record.get("size_in_bytes"), f"artifact size {name}")
        artifact_id = _positive_int(record.get("id"), f"artifact ID {name}")
        if record.get("archive_download_url") != _artifact_url(REPOSITORY, artifact_id):
            _fail(f"artifact download URL is not canonical: {name}")
        if size > MAX_FILE_BYTES or declared_total > MAX_TOTAL_DOWNLOAD_BYTES - size:
            _fail("producer artifacts exceed the bounded download limit")
        declared_total += size
        records[name] = record
    if declared_total < 1:
        _fail("producer artifact aggregate is empty")
    root.mkdir(parents=True, exist_ok=True)
    opener = build_opener(_HttpsArtifactRedirectHandler())
    for name, record in records.items():
        expected_size = record["size_in_bytes"]
        request = Request(record["archive_download_url"], headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github+json",
        })
        path = root / f"{name}.zip"
        partial = path.with_suffix(".zip.part")
        total = 0
        try:
            with opener.open(request, timeout=120) as response, partial.open("wb") as output:
                while True:
                    chunk = response.read(min(1024 * 1024, expected_size - total + 1))
                    if not chunk:
                        break
                    total += len(chunk)
                    if total > expected_size:
                        _fail(f"artifact download exceeds declared size: {name}")
                    output.write(chunk)
            if total != expected_size:
                _fail(f"artifact download is truncated: {name}")
            partial.replace(path)
        except BaseException:
            partial.unlink(missing_ok=True)
            raise


def extract_producer_artifacts(root: Path, output: Path, identity: dict[str, Any]) -> None:
    artifacts = _mapping(identity.get("artifacts"), "producer artifacts")
    for name in artifacts:
        match = ARTIFACT_NAME.fullmatch(name) if isinstance(name, str) else None
        if match is None:
            _fail(f"producer artifact is outside the recognized attempt namespace: {name}")
        kind = name[: -(len(match.group("attempt")) + 1)]
        archive = root / f"{name}.zip"
        if kind == "smarty-candidate-sources":
            destination = output / "source-archives"
        elif kind in {"smarty-release-plan", "smarty-candidate-handoff"}:
            destination = output
        elif kind.startswith("candidate-"):
            continue
        else:
            _fail(f"producer artifact has no trusted extraction route: {name}")
        extract_zip(archive, destination)


def verify_downloads(root: Path, identity: dict[str, Any]) -> dict[str, Any]:
    records = {}
    for name, expected in _mapping(identity.get("artifacts"), "producer artifacts").items():
        actual = file_record(root / f"{name}.zip")
        if actual["length"] != expected["size_in_bytes"] or f"sha256:{actual['sha256']}" != expected["digest"]:
            _fail(f"downloaded artifact bytes mismatch: {name}")
        records[name] = actual
    return {"schema": 1, "downloads": records}


def _safe_member_name(name: str, label: str) -> Path:
    if not isinstance(name, str) or not name or "\x00" in name:
        _fail(f"unsafe archive member in {label}: {name!r}")
    normalized = name.replace("\\", "/")
    windows_path = PureWindowsPath(normalized)
    if normalized.startswith("/") or windows_path.drive or windows_path.root:
        _fail(f"unsafe archive member in {label}: {name}")
    parts = normalized.rstrip("/").split("/")
    if not parts or any(part in ("", ".", "..") for part in parts):
        _fail(f"unsafe archive member in {label}: {name}")
    path = Path(*parts)
    if path.is_absolute() or "\\" in name:
        _fail(f"unsafe archive member in {label}: {name}")
    return path
def _safe_link_target(member_name: str, target_name: str, label: str) -> str:
    if not isinstance(target_name, str) or not target_name or "\x00" in target_name:
        _fail(f"unsafe archive link target in {label}: {member_name}")
    if "\\" in target_name or PureWindowsPath(target_name).drive or PureWindowsPath(target_name).root:
        _fail(f"unsafe archive link target in {label}: {member_name}")
    base = PurePosixPath(member_name).parent
    resolved = PurePosixPath(target_name) if target_name.startswith("/") else base / target_name
    parts: list[str] = []
    for part in resolved.parts:
        if part in ("", "."):
            continue
        if part == "..":
            if not parts:
                _fail(f"archive link target escapes root in {label}: {member_name}")
            parts.pop()
        else:
            parts.append(part)
    return "/".join(parts)


def _existing_regular_bytes(root: Path) -> int:
    if root.is_symlink():
        _fail(f"extraction root is a symlink: {root}")
    if not root.exists():
        return 0
    total = 0
    for path in root.rglob("*"):
        if path.is_symlink() or not (path.is_dir() or path.is_file()):
            _fail(f"extraction tree contains an unsupported entry: {path}")
        if path.is_file():
            size = path.stat().st_size
            if size < 0 or total > MAX_TOTAL_EXTRACTED_BYTES - size:
                _fail("extraction tree exceeds the aggregate size limit")
            total += size
    return total


def extract_zip(archive: Path, output: Path) -> None:
    if archive.is_symlink() or not archive.is_file():
        _fail(f"artifact archive is not a regular file: {archive}")
    output.mkdir(parents=True, exist_ok=True)
    seen: set[str] = set()
    archive_expanded = 0
    total_expanded = _existing_regular_bytes(output)
    with zipfile.ZipFile(archive) as bundle:
        infos = bundle.infolist()
        if len(infos) > MAX_ZIP_MEMBERS:
            _fail(f"{archive.name} has too many members")
        for info in infos:
            relative = _safe_member_name(info.filename, archive.name)
            if info.filename in seen:
                _fail(f"artifact archive repeats a member: {info.filename}")
            seen.add(info.filename)
            size = info.file_size
            if (size < 0 or archive_expanded > MAX_ZIP_EXPANDED_BYTES - size
                    or total_expanded > MAX_TOTAL_EXTRACTED_BYTES - size):
                _fail(f"{archive.name} exceeds the expanded-size limit")
            target = output / relative
            if target.exists():
                _fail(f"artifact archive members collide: {info.filename}")
            if info.is_dir():
                target.mkdir(parents=True, exist_ok=False)
                continue
            archive_expanded += size
            total_expanded += size
            if ((info.external_attr >> 16) & 0o170000) == 0o120000:
                _fail(f"artifact archive contains a symlink: {info.filename}")
            target.parent.mkdir(parents=True, exist_ok=True)
            with bundle.open(info) as source, target.open("xb") as destination:
                copied = 0
                while chunk := source.read(min(1024 * 1024, size - copied + 1)):
                    copied += len(chunk)
                    if copied > size:
                        _fail(f"artifact archive member expands beyond its declared size: {info.filename}")
                    destination.write(chunk)
                if copied != size:
                    _fail(f"artifact archive member size mismatch: {info.filename}")

def _safe_tar_members(path: Path, label: str) -> list[tarfile.TarInfo]:
    if path.is_symlink() or not path.is_file():
        _fail(f"{label} is not a regular file")
    members: list[tarfile.TarInfo] = []
    seen: set[str] = set()
    links: dict[str, str] = {}
    expanded = 0
    with tarfile.open(path, mode="r:*") as archive:
        for member in archive:
            if len(members) >= MAX_TAR_MEMBERS:
                _fail(f"{label} has too many members")
            _safe_member_name(member.name, label)
            if member.name in seen:
                _fail(f"{label} repeats a member: {member.name}")
            seen.add(member.name)
            if member.islnk():
                _fail(f"{label} contains an unsupported hard link: {member.name}")
            if member.issym():
                links[member.name] = _safe_link_target(member.name, member.linkname, label)
            elif not (member.isfile() or member.isdir()):
                _fail(f"{label} contains an unsupported member: {member.name}")
            if member.isfile():
                if member.size < 0 or expanded > MAX_TAR_EXPANDED_BYTES - member.size:
                    _fail(f"{label} exceeds the expanded-size limit")
                expanded += member.size
            members.append(member)
    by_name = {member.name: member for member in members}
    for name, target in links.items():
        target_member = by_name.get(target)
        if target_member is None or not target_member.isfile():
            _fail(f"{label} link target is not an archive regular file: {name}")
        prefix = name.rstrip("/") + "/"
        if any(member.name.startswith(prefix) for member in members):
            _fail(f"{label} symlink is used as a directory: {name}")
    return members


def _safe_tar(path: Path, label: str) -> list[str]:
    return [member.name for member in _safe_tar_members(path, label)]


def extract_tar(archive_path: Path, output: Path) -> None:
    members = _safe_tar_members(archive_path, archive_path.name)
    output.mkdir(parents=True, exist_ok=True)
    total_extracted = _existing_regular_bytes(output)
    with tarfile.open(archive_path, mode="r:*") as archive:
        for member in members:
            relative = _safe_member_name(member.name, archive_path.name)
            target = output / relative
            if target.exists() or target.is_symlink():
                _fail(f"tar archive members collide: {member.name}")
            if member.isdir():
                target.mkdir(parents=True, exist_ok=False)
                continue
            target.parent.mkdir(parents=True, exist_ok=True)
            if member.issym():
                _safe_link_target(member.name, member.linkname, archive_path.name)
                target.symlink_to(member.linkname)
                continue
            if member.size > MAX_TOTAL_EXTRACTED_BYTES - total_extracted:
                _fail(f"{archive_path.name} exceeds the aggregate expanded-size limit")
            source = archive.extractfile(member)
            if source is None:
                _fail(f"tar archive member cannot be read: {member.name}")
            copied = 0
            with source, target.open("xb") as destination:
                while chunk := source.read(min(1024 * 1024, member.size - copied + 1)):
                    copied += len(chunk)
                    if copied > member.size or copied > MAX_TOTAL_EXTRACTED_BYTES - total_extracted:
                        _fail(f"tar archive member expands beyond its declared size: {member.name}")
                    destination.write(chunk)
            if copied != member.size:
                _fail(f"tar archive member size mismatch: {member.name}")
            target.chmod(member.mode & 0o7777)
            total_extracted += copied
def herdr_protocol_from_archive(path: Path) -> int:
    members = [member for member in _safe_tar_members(path, "Herdr source archive") if member.name == "src/protocol/wire.rs"]
    if len(members) != 1:
        _fail("Herdr source archive must contain one exact protocol declaration")
    member = members[0]
    if member.size > MAX_PROTOCOL_SOURCE_BYTES:
        _fail("Herdr protocol source exceeds the bounded read limit")
    with tarfile.open(path, mode="r:*") as archive:
        source = archive.extractfile(member)
        if source is None:
            _fail("Herdr protocol source cannot be read")
        with source:
            content = source.read(MAX_PROTOCOL_SOURCE_BYTES + 1)
    if len(content) != member.size or len(content) > MAX_PROTOCOL_SOURCE_BYTES:
        _fail("Herdr protocol source read is invalid")
    matches = re.findall(rb"(?m)^pub const PROTOCOL_VERSION: u32 = ([0-9]+);$", content)
    if len(matches) != 1:
        _fail("Herdr protocol declaration is not exact")
    protocol = int(matches[0])
    if protocol < 1:
        _fail("Herdr protocol version must be positive")
    return protocol
def _load_tar_json(path: Path, member_name: str, label: str) -> Any:
    members = [member for member in _safe_tar_members(path, label) if member.name == member_name]
    if len(members) != 1 or not members[0].isfile():
        _fail(f"{label} must contain one regular JSON member: {member_name}")
    member = members[0]
    if member.size > MAX_JSON_BYTES:
        _fail(f"{label} JSON member exceeds the bounded read limit: {member_name}")
    with tarfile.open(path, mode="r:*") as archive:
        source = archive.extractfile(member)
        if source is None:
            _fail(f"{label} JSON member cannot be read: {member_name}")
        with source:
            data = source.read(MAX_JSON_BYTES + 1)
    if len(data) != member.size or len(data) > MAX_JSON_BYTES:
        _fail(f"{label} JSON member read is invalid: {member_name}")
    try:
        return json.loads(data.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"{label} JSON member is not valid UTF-8 JSON: {member_name}") from error


def validate_git_archive(archive_path: Path, repository: Path) -> None:
    members = _safe_tar_members(archive_path, "Herdr source archive")
    files = {member.name: member for member in members if member.isfile() or member.issym()}
    raw = subprocess.check_output(
        ["git", "-C", str(repository), "ls-tree", "-r", "--full-tree", "-z", "HEAD"]
    )
    blobs: dict[str, tuple[int, str]] = {}
    for record in raw.split(b"\0"):
        if not record:
            continue
        header, name = record.split(b"\t", 1)
        mode, kind, object_id = header.split()
        if kind != b"blob":
            _fail(f"Herdr Git tree contains unsupported non-blob entry: {name!r}")
        path = name.decode("utf-8")
        blobs[path] = (int(mode, 8), object_id.decode("ascii"))
        if path not in files:
            _fail(f"Herdr source archive omits Git tree file: {path}")
    if set(files) != set(blobs):
        _fail("Herdr source archive file set does not equal the Git tree")
    expected_directories = {
        "/".join(path.split("/")[:index])
        for path in blobs
        for index in range(1, len(path.split("/")))
    }
    actual_directories = {member.name.rstrip("/") for member in members if member.isdir()}
    if not actual_directories <= expected_directories:
        _fail("Herdr source archive contains a directory outside the Git tree")
    with tarfile.open(archive_path, mode="r:*") as bundle:
        for path, (git_mode, object_id) in blobs.items():
            member = files[path]
            git_type = git_mode & 0o170000
            if git_type not in (0o100000, 0o120000):
                _fail(f"Herdr Git tree has an unsupported file mode: {path}")
            if git_type == 0o120000:
                if not member.issym():
                    _fail(f"Herdr source archive does not preserve Git symlink semantics: {path}")
                expected_permissions = 0o777
                source = io.BytesIO(member.linkname.encode("utf-8"))
                expected_size = len(member.linkname.encode("utf-8"))
            else:
                if not member.isfile():
                    _fail(f"Herdr source archive does not preserve Git regular-file semantics: {path}")
                expected_permissions = 0o775 if git_mode & 0o111 else 0o664
                source = bundle.extractfile(member)
                if source is None:
                    _fail(f"Herdr source archive member cannot be read: {path}")
                expected_size = member.size
            if (member.mode & 0o7777) != expected_permissions:
                _fail(f"Herdr source archive mode differs from the Git tree: {path}")
            process = subprocess.Popen(
                ["git", "-C", str(repository), "cat-file", "blob", object_id],
                stdout=subprocess.PIPE,
            )
            if process.stdout is None:
                process.kill()
                process.wait()
                _fail("Git blob stream is unavailable")
            total = 0
            try:
                with source, process.stdout as expected:
                    while True:
                        actual_chunk = source.read(1024 * 1024)
                        expected_chunk = expected.read(1024 * 1024)
                        if actual_chunk != expected_chunk:
                            _fail(f"Herdr source archive content differs from the Git tree: {path}")
                        if not actual_chunk:
                            break
                        total += len(actual_chunk)
                        if total > MAX_FILE_BYTES:
                            _fail(f"Herdr source archive member exceeds the bounded size limit: {path}")
            except BaseException:
                if process.poll() is None:
                    process.kill()
                process.wait()
                raise
            if process.wait() != 0:
                _fail(f"Git blob could not be read: {path}")
            if total != expected_size:
                _fail(f"Herdr source archive member size differs from the Git tree: {path}")


def validate_producer_tree(root: Path, identity: dict[str, Any]) -> dict[str, Any]:
    identity = _mapping(identity, "producer identity")
    expected = paired_identity(str(identity.get("tag")), source_sha=str(identity.get("source")))
    for key in ("tag", "build_id", "parent", "source", "omp"):
        if identity.get(key) != expected[key]:
            _fail(f"producer identity field mismatch: {key}")
    plan = _mapping(_load_json(root / "producer-plan.json", "producer plan"), "producer plan")
    for key in ("tag", "build_id", "parent", "source", "omp"):
        if plan.get(key) != expected[key]:
            _fail(f"producer plan identity mismatch: {key}")
    expected_plan_fields = {"schema", "repository", "workflow_name", "workflow_path", "run_id", "run_attempt", "tag", "build_id", "parent", "source", "omp", "built_at", "base_version", "protocol", "legacy_day_binding", "source_archives"}
    if set(plan) != expected_plan_fields or plan.get("schema") != 1:
        _fail("producer plan fields are not exact")
    if plan.get("repository") != REPOSITORY or plan.get("workflow_name") != WORKFLOW_NAME or plan.get("workflow_path") != WORKFLOW_PATH:
        _fail("producer plan workflow identity mismatch")
    if plan.get("run_id") != identity.get("run_id") or plan.get("run_attempt") != identity.get("run_attempt"):
        _fail("producer plan run identity mismatch")
    for key in ("tag", "build_id", "parent", "source", "omp"):
        if plan.get(key) != expected[key]:
            _fail(f"producer plan identity mismatch: {key}")
    built_at = normalize_built_at(str(plan.get("built_at")))
    if built_at != identity.get("built_at"):
        _fail("producer plan timestamp does not match workflow commit timestamp")
    paired_identity(expected["tag"], source_sha=expected["source"], built_at=built_at)
    if plan.get("legacy_day_binding") != "literal-built_at-prefix":
        _fail("producer plan timestamp policy mismatch")
    source_archive = root / "source-archives" / "herdr-source.tar"
    protocol = herdr_protocol_from_archive(source_archive)
    if plan.get("protocol") != protocol:
        _fail("producer plan protocol does not match the Herdr source")
    names = _safe_tar(source_archive, "Herdr source archive")
    if ".github/smarty-omp-source.json" not in names:
        _fail("Herdr source archive does not contain the OMP descriptor")
    source_record = file_record(source_archive)
    if plan.get("source_archives") != {"herdr-source.tar": source_record}:
        _fail("producer plan source archive record mismatch")
    descriptor = _mapping(_load_json(root / "source-archives/omp-descriptor.json", "OMP descriptor"), "OMP descriptor")
    archived_descriptor = _mapping(
        _load_tar_json(source_archive, ".github/smarty-omp-source.json", "Herdr source archive"),
        "archived OMP descriptor",
    )
    if archived_descriptor != descriptor:
        _fail("Herdr source archive OMP descriptor differs from the supplied descriptor")
    if set(descriptor) != {"repository", "commit", "tree", "version", "build_id"}:
        _fail("producer OMP descriptor fields are not exact")
    if descriptor.get("repository") != OMP_REPOSITORY or descriptor.get("commit") != expected["omp"]:
        _fail("producer OMP descriptor does not bind O")
    descriptor_tree = _hex(descriptor.get("tree"), "producer OMP tree")
    descriptor_build_id = _scalar(descriptor.get("build_id"), "producer OMP build ID")
    if descriptor_build_id != managed_omp_build_id(descriptor_tree):
        _fail("producer OMP build ID does not match OMP tree")
    candidate_root = root / "candidate-artifacts"
    expected_files = set(HERDR_ASSETS.values()) | {f"{name}.sha256" for name in HERDR_ASSETS.values()}
    paths = list(candidate_root.iterdir()) if candidate_root.is_dir() else []
    if {path.name for path in paths} != expected_files or any(path.is_symlink() or not path.is_file() for path in paths):
        _fail("producer Herdr artifact set is not exact")
    candidate_files = {}
    for name in HERDR_ASSETS.values():
        digest = file_record(candidate_root / name)["sha256"]
        try:
            sidecar = _read_bounded_bytes(
                candidate_root / f"{name}.sha256",
                f"producer checksum sidecar {name}",
                MAX_SIDECAR_BYTES,
            ).decode("ascii")
        except UnicodeDecodeError as error:
            raise ValueError(f"producer checksum sidecar is not ASCII: {name}") from error
        if sidecar.split() != [digest, name]:
            _fail(f"producer checksum mismatch: {name}")
        candidate_files[name] = file_record(candidate_root / name)
        candidate_files[f"{name}.sha256"] = file_record(candidate_root / f"{name}.sha256")
    handoff = _mapping(_load_json(root / "producer-handoff.json", "producer handoff"), "producer handoff")
    if handoff.get("schema") != 1 or handoff.get("identity") != {key: expected[key] for key in ("tag", "build_id", "parent", "source", "omp")} or handoff.get("files") != candidate_files:
        _fail("producer handoff records mismatch")
    return {"schema": 1, "identity": expected, "producer_plan": plan, "descriptor": descriptor,
            "candidate_files": candidate_files, "source_archive": file_record(source_archive)}


def validate_trusted_sources(identity: dict[str, Any], plan: dict[str, Any], parent_root: Path, omp_root: Path,
                             descriptor: dict[str, Any], output: Path) -> dict[str, Any]:
    identity = _mapping(identity, "source identity")
    plan = _mapping(plan, "source plan")
    expected = paired_identity(str(identity.get("tag")), source_sha=str(identity.get("source")))
    for key in ("tag", "build_id", "parent", "source", "omp"):
        if plan.get(key) != expected[key]:
            _fail(f"trusted source plan identity mismatch: {key}")

    def git(root: Path, *args: str) -> str:
        try:
            return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()
        except subprocess.CalledProcessError as error:
            raise ValueError(f"git source query failed: {root} {' '.join(args)}") from error

    if git(parent_root, "rev-parse", "HEAD") != expected["parent"] or git(omp_root, "rev-parse", "HEAD") != expected["omp"]:
        _fail("private source checkout commit mismatch")
    parent_tree = _hex(git(parent_root, "rev-parse", "HEAD^{tree}"), "parent tree")
    omp_tree = _hex(git(omp_root, "rev-parse", "HEAD^{tree}"), "OMP tree")
    descriptor = _mapping(descriptor, "trusted OMP descriptor")
    if set(descriptor) != {"repository", "commit", "tree", "version", "build_id"}:
        _fail("trusted OMP descriptor fields are not exact")
    if descriptor.get("repository") != OMP_REPOSITORY or descriptor.get("commit") != expected["omp"] or descriptor.get("tree") != omp_tree:
        _fail("trusted OMP descriptor mismatch")
    descriptor_tree = _hex(descriptor.get("tree"), "trusted OMP descriptor tree")
    descriptor_build_id = _scalar(descriptor.get("build_id"), "trusted OMP build ID")
    if descriptor_build_id != managed_omp_build_id(descriptor_tree):
        _fail("trusted OMP build ID does not match OMP tree")
    package = _mapping(_load_json(omp_root / "packages/coding-agent/package.json", "OMP package"), "OMP package")
    version = _scalar(package.get("version"), "OMP version")
    if descriptor.get("version") != version:
        _fail("trusted OMP package identity mismatch")
    release = _mapping(_load_json(parent_root / "integrations/omp/release.json", "parent OMP release"), "parent OMP release")
    if set(release) != {"schema", "sourceRevision", "sourceTree", "version", "buildId"} or release.get("schema") != 1:
        _fail("parent OMP release fields are not exact")
    expected_build_id = managed_omp_build_id(omp_tree)
    if (release.get("sourceRevision") != expected["omp"] or release.get("sourceTree") != omp_tree
            or release.get("version") != version or release.get("buildId") != expected_build_id
            or descriptor_build_id != expected_build_id):
        _fail("parent OMP release does not independently bind the trusted descriptor")
    def gitlink(path: str, expected_commit: str, label: str) -> None:
        fields = git(parent_root, "ls-tree", "HEAD", path).split()
        if len(fields) != 4 or fields[0] != "160000" or fields[1] != "commit" or fields[2] != expected_commit or fields[3] != path:
            _fail(f"parent {label} gitlink mismatch")
    gitlink("repos/herdr", expected["source"], "Herdr")
    gitlink("repos/omp", expected["omp"], "OMP")
    result = {"schema": 1, "identity": expected, "parent": {"commit": expected["parent"], "tree": parent_tree},
              "omp": {"repository": OMP_REPOSITORY, "commit": expected["omp"], "tree": descriptor_tree,
                      "version": version, "build_id": descriptor_build_id}}
    write_canonical(output, result)
    return result


def _validate_sidecars(asset_dir: Path) -> None:
    for name in PAYLOAD_ASSETS:
        digest = file_record(asset_dir / name)["sha256"]
        try:
            sidecar = _read_bounded_bytes(
                asset_dir / f"{name}.sha256",
                f"release checksum sidecar {name}",
                MAX_SIDECAR_BYTES,
            ).decode("ascii")
        except UnicodeDecodeError as error:
            raise ValueError(f"release checksum sidecar is not ASCII: {name}") from error
        if sidecar.split() != [digest, name]:
            _fail(f"release checksum sidecar mismatch: {name}")


def seal_release(asset_dir: Path, identity: dict[str, Any], output_dir: Path) -> dict[str, Any]:
    identity = _mapping(identity, "seal identity")
    parsed = paired_identity(str(identity.get("tag")), source_sha=str(identity.get("source")))
    for key in ("build_id", "parent", "source", "omp"):
        if identity.get(key) != parsed[key]:
            _fail(f"seal {key} mismatch")
    built_at = identity.get("built_at")
    if built_at is not None:
        built_at = normalize_built_at(str(built_at))
        paired_identity(parsed["tag"], source_sha=parsed["source"], built_at=built_at)
    if not asset_dir.is_dir():
        _fail("release asset directory is missing")
    paths = list(asset_dir.iterdir())
    if {path.name for path in paths} != set(FULL_RELEASE_ASSETS) or any(path.is_symlink() or not path.is_file() for path in paths):
        _fail("sealed release does not match the exact 37-file allow-list")
    _validate_sidecars(asset_dir)
    pair = _mapping(_load_json(asset_dir / "smarty-pair.json", "pair manifest"), "pair manifest")
    release = _mapping(pair.get("release"), "pair release")
    sources = _mapping(pair.get("sources"), "pair sources")
    if (release.get("repository") != REPOSITORY or release.get("tag") != parsed["tag"] or
            release.get("build_id") != parsed["build_id"] or release.get("immutable") is not True or
            (built_at is not None and release.get("built_at") != built_at)):
        _fail("pair manifest does not bind sealed release identity")
    for source_name, identity_key in (("parent", "parent"), ("herdr", "source"), ("omp", "omp")):
        source = _mapping(sources.get(source_name), f"pair {source_name} source")
        if source.get("commit") != parsed[identity_key]:
            _fail("pair manifest does not bind sealed source identity")
    files: dict[str, dict[str, int | str]] = {}
    total = 0
    for name in sorted(FULL_RELEASE_ASSETS):
        record = file_record(asset_dir / name)
        size = int(record["length"])
        if total > MAX_TOTAL_DOWNLOAD_BYTES - size:
            _fail("sealed release exceeds the aggregate size limit")
        total += size
        files[name] = record
    output_dir.mkdir(parents=True, exist_ok=True)
    release_record = write_canonical(output_dir / "release-files.json", {"schema": 1, "files": files})
    handoff = {"schema": 1, "repository": REPOSITORY, "workflow": PUBLISH_WORKFLOW_PATH, "identity": parsed,
               "files": files, "release_files": release_record}
    handoff_record = write_canonical(output_dir / "sealed-handoff.json", handoff)
    return {**handoff, "handoff": handoff_record}


def seal_inputs(asset_dir: Path, identity: dict[str, Any], output_dir: Path) -> dict[str, Any]:
    identity = _mapping(identity, "input seal identity")
    parsed = paired_identity(str(identity.get("tag")), source_sha=str(identity.get("source")))
    if identity.get("build_id") != parsed["build_id"]:
        _fail("input seal build ID mismatch")
    built_at = identity.get("built_at")
    if built_at is not None:
        built_at = normalize_built_at(str(built_at))
        paired_identity(parsed["tag"], source_sha=parsed["source"], built_at=built_at)
    if not asset_dir.is_dir():
        _fail("input seal asset directory is missing")
    paths = list(asset_dir.iterdir())
    if {path.name for path in paths} != set(SEALED_INPUT_ASSETS) or any(path.is_symlink() or not path.is_file() for path in paths):
        _fail("sealed trusted inputs do not match the exact allow-list")
    files: dict[str, dict[str, int | str]] = {}
    total = 0
    for name in sorted(SEALED_INPUT_ASSETS):
        record = file_record(asset_dir / name)
        size = int(record["length"])
        if total > MAX_TOTAL_DOWNLOAD_BYTES - size:
            _fail("sealed trusted inputs exceed the aggregate size limit")
        total += size
        files[name] = record
    output_dir.mkdir(parents=True, exist_ok=True)
    release_record = write_canonical(output_dir / "sealed-input-files.json", {"schema": 1, "files": files})
    handoff = {
        "schema": 1,
        "repository": REPOSITORY,
        "workflow": PUBLISH_WORKFLOW_PATH,
        "identity": parsed,
        "files": files,
        "expected_release_assets": list(FULL_RELEASE_ASSETS),
        "sealed_inputs": release_record,
    }
    handoff_record = write_canonical(output_dir / "sealed-input-handoff.json", handoff)
    return {**handoff, "handoff": handoff_record}


def cmd_validate_git_archive(args: argparse.Namespace) -> int:
    validate_git_archive(Path(args.archive), Path(args.repository))
    return 0
def _load(path: str) -> Any:
    return _load_json(Path(path), path)


def _write_output(path: str | None, value: Any) -> int:
    if path:
        write_canonical(Path(path), value)
    else:
        sys.stdout.buffer.write(canonical_bytes(value))
    return 0




def cmd_identity(args: argparse.Namespace) -> int:
    return _write_output(args.output, paired_identity(args.tag, source_sha=args.source_sha, built_at=args.built_at))

def cmd_extract_tar(args: argparse.Namespace) -> int:
    extract_tar(Path(args.archive), Path(args.output))
    return 0


def cmd_validate_tag(args: argparse.Namespace) -> int:
    return _write_output(args.output, validate_tag_object(_load(args.tag_json), args.tag, args.source_sha))


def cmd_validate_run(args: argparse.Namespace) -> int:
    publisher = {
        "workflow": _load(args.publisher_workflow_json),
        "content": _load(args.publisher_content_json),
        "default_branch": args.default_branch,
        "checked_out_commit": args.checked_out_commit,
        "checked_out_blob": args.checked_out_blob,
        "branch": _load(args.publisher_branch_json),
        "revision": _load(args.publisher_revision_json),
    }
    result = validate_workflow_run(
        _load(args.run_json), _load(args.workflow_json), _load(args.artifacts_json),
        repository=args.repository, run_id=args.run_id, event_run_attempt=args.event_run_attempt, publisher=publisher,
    )
    return _write_output(args.output, result)


def cmd_download_artifacts(args: argparse.Namespace) -> int:
    download_artifacts(Path(args.root), _load(args.identity), os.environ.get("GH_TOKEN", ""))
    return 0


def cmd_extract_producer_artifacts(args: argparse.Namespace) -> int:
    extract_producer_artifacts(Path(args.root), Path(args.output), _load(args.identity))
    return 0


def cmd_verify_downloads(args: argparse.Namespace) -> int:
    return _write_output(args.output, verify_downloads(Path(args.root), _load(args.identity)))


def cmd_extract_zip(args: argparse.Namespace) -> int:
    extract_zip(Path(args.archive), Path(args.output))
    return 0


def cmd_validate_producer(args: argparse.Namespace) -> int:
    return _write_output(args.output, validate_producer_tree(Path(args.root), _load(args.identity)))


def cmd_validate_sources(args: argparse.Namespace) -> int:
    validate_trusted_sources(_load(args.identity), _load(args.plan), Path(args.parent_root), Path(args.omp_root),
                             _load(args.descriptor), Path(args.output))
    return 0


def cmd_legacy_day(args: argparse.Namespace) -> int:
    validate_legacy_day(args.build_id, args.built_at)
    return 0


def cmd_seal(args: argparse.Namespace) -> int:
    seal_release(Path(args.asset_dir), _load(args.identity), Path(args.output_dir))
    return 0

def cmd_seal_inputs(args: argparse.Namespace) -> int:
    seal_inputs(Path(args.asset_dir), _load(args.identity), Path(args.output_dir))
    return 0
def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(required=True)

    identity = sub.add_parser("identity")
    identity.add_argument("--tag", required=True)
    identity.add_argument("--source-sha")
    identity.add_argument("--built-at")
    identity.add_argument("--output")
    identity.set_defaults(func=cmd_identity)

    tag = sub.add_parser("validate-tag")
    tag.add_argument("--tag-json", required=True)
    tag.add_argument("--tag", required=True)
    tag.add_argument("--source-sha", required=True)
    tag.add_argument("--output", required=True)
    tag.set_defaults(func=cmd_validate_tag)

    run = sub.add_parser("validate-run")
    run.add_argument("--run-json", required=True)
    run.add_argument("--workflow-json", required=True)
    run.add_argument("--artifacts-json", required=True)
    run.add_argument("--publisher-workflow-json", required=True)
    run.add_argument("--publisher-content-json", required=True)
    run.add_argument("--publisher-branch-json", required=True)
    run.add_argument("--publisher-revision-json", required=True)
    run.add_argument("--default-branch", required=True)
    run.add_argument("--checked-out-commit", required=True)
    run.add_argument("--checked-out-blob", required=True)
    run.add_argument("--repository", default=REPOSITORY)
    run.add_argument("--run-id", type=int, required=True)
    run.add_argument("--event-run-attempt", type=int, required=True)
    run.add_argument("--output", required=True)
    run.set_defaults(func=cmd_validate_run)

    download = sub.add_parser("download-artifacts")
    download.add_argument("--root", required=True)
    download.add_argument("--identity", required=True)
    download.set_defaults(func=cmd_download_artifacts)

    producer_extract = sub.add_parser("extract-producer-artifacts")
    producer_extract.add_argument("--root", required=True)
    producer_extract.add_argument("--output", required=True)
    producer_extract.add_argument("--identity", required=True)
    producer_extract.set_defaults(func=cmd_extract_producer_artifacts)

    downloads = sub.add_parser("verify-downloads")
    archive_validate = sub.add_parser("validate-git-archive")
    archive_validate.add_argument("--archive", required=True)
    archive_validate.add_argument("--repository", required=True)
    archive_validate.set_defaults(func=cmd_validate_git_archive)

    downloads.add_argument("--root", required=True)
    downloads.add_argument("--identity", required=True)
    downloads.add_argument("--output", required=True)
    downloads.set_defaults(func=cmd_verify_downloads)

    extract = sub.add_parser("extract-zip")
    extract.add_argument("--archive", required=True)
    extract.add_argument("--output", required=True)
    extract.set_defaults(func=cmd_extract_zip)

    tar_extract = sub.add_parser("extract-tar")
    tar_extract.add_argument("--archive", required=True)
    tar_extract.add_argument("--output", required=True)
    tar_extract.set_defaults(func=cmd_extract_tar)

    producer = sub.add_parser("validate-producer")
    producer.add_argument("--root", required=True)
    producer.add_argument("--identity", required=True)
    producer.add_argument("--output", required=True)
    producer.set_defaults(func=cmd_validate_producer)

    sources = sub.add_parser("validate-sources")
    sources.add_argument("--identity", required=True)
    sources.add_argument("--plan", required=True)
    sources.add_argument("--parent-root", required=True)
    sources.add_argument("--omp-root", required=True)
    sources.add_argument("--descriptor", required=True)
    sources.add_argument("--output", required=True)
    sources.set_defaults(func=cmd_validate_sources)

    legacy = sub.add_parser("legacy-day")
    legacy.add_argument("--build-id", required=True)
    legacy.add_argument("--built-at", required=True)
    legacy.set_defaults(func=cmd_legacy_day)

    seal = sub.add_parser("seal")
    seal.add_argument("--asset-dir", required=True)
    seal.add_argument("--identity", required=True)
    seal.add_argument("--output-dir", required=True)
    seal.set_defaults(func=cmd_seal)

    seal_inputs_parser = sub.add_parser("seal-inputs")
    seal_inputs_parser.add_argument("--asset-dir", required=True)
    seal_inputs_parser.add_argument("--identity", required=True)
    seal_inputs_parser.add_argument("--output-dir", required=True)
    seal_inputs_parser.set_defaults(func=cmd_seal_inputs)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return int(args.func(args))
    except (OSError, ValueError, json.JSONDecodeError, tarfile.TarError, zipfile.BadZipFile) as error:
        print(f"trusted preview validation failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
