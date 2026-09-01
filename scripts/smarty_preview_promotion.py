#!/usr/bin/env python3
"""Trusted internal Smarty preview channel promotion.

This owns the bridge/canonical transaction used by the protected publisher.
It deliberately does not read or invoke the public stable TUF path.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Iterator

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from scripts import smarty_preview_release as release
from scripts import smarty_preview_trusted as trusted

REPOSITORY = trusted.REPOSITORY
CHANNEL_REF = "refs/heads/smarty-channel"
AUTHORIZATION_REF = "refs/heads/smarty-preview-authorization"
PROMOTION_REF = "refs/heads/smarty-preview-promotion-authorization"
SHA1 = re.compile(r"[0-9a-f]{40}")
SHA256 = re.compile(r"[0-9a-f]{64}")
CHANNEL_RETAIN = 30


def _fail(message: str) -> None:
    raise ValueError(message)


def _sha1(value: Any, label: str) -> str:
    if not isinstance(value, str) or SHA1.fullmatch(value) is None:
        _fail(f"{label} must be a lowercase Git commit")
    return value


def _sha256(value: Any, label: str) -> str:
    if not isinstance(value, str) or SHA256.fullmatch(value) is None:
        _fail(f"{label} must be a lowercase SHA-256 digest")
    return value


def _positive(value: Any, label: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 1:
        _fail(f"{label} must be a positive integer")
    return value


def _one_line(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value or value != value.strip() or "\n" in value or "\r" in value:
        _fail(f"{label} must be one nonempty line")
    return value


def _json(path: Path, label: str) -> Any:
    if path.is_symlink() or not path.is_file():
        _fail(f"{label} must be a regular file")
    data = path.read_bytes()
    if len(data) > trusted.MAX_JSON_BYTES:
        _fail(f"{label} exceeds the JSON size limit")
    try:
        return json.loads(data)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"{label} must be valid UTF-8 JSON") from error


def _mapping(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        _fail(f"{label} must be an object")
    return value


def _write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n", encoding="ascii")




def _git_bytes(repo: Path, *args: str, input: bytes | None = None, env: dict[str, str] | None = None) -> bytes:
    return subprocess.check_output(["git", "-C", str(repo), *args], input=input, env=env)
def _git(repo: Path, *args: str, input: str | None = None, env: dict[str, str] | None = None) -> str:
    return subprocess.check_output(["git", "-C", str(repo), *args], input=input, text=True, env=env).strip()


def _git_init(repo: Path) -> None:
    repo.mkdir(parents=True, exist_ok=True)
    subprocess.check_call(["git", "init", "-q", str(repo)])


def _remote(repository: str) -> str:
    if repository != REPOSITORY:
        _fail("preview promotion repository mismatch")
    return f"https://github.com/{repository}.git"


@contextmanager
def _git_auth(token: str) -> Iterator[dict[str, str]]:
    if not token:
        _fail("GH_TOKEN is required for channel Git access")
    directory = os.environ.get("RUNNER_TEMP")
    with tempfile.NamedTemporaryFile("w", dir=directory, prefix="smarty-preview-askpass-", delete=False) as file:
        file.write("#!/bin/sh\ncase \"$1\" in\n  *Username*) printf '%s\\n' x-access-token ;;\n  *Password*) printf '%s\\n' \"$GH_TOKEN\" ;;\n  *) exit 1 ;;\nesac\n")
        askpass = file.name
    try:
        os.chmod(askpass, stat.S_IRWXU)
        env = os.environ.copy()
        env.update({
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_ASKPASS": askpass,
            "GIT_TERMINAL_PROMPT": "0",
            "GH_TOKEN": token,
        })
        yield env
    finally:
        Path(askpass).unlink(missing_ok=True)


def _observe_ref(remote: str, ref: str, env: dict[str, str]) -> str:
    line = subprocess.check_output(
        ["git", "-c", "credential.helper=", "-c", f"core.askPass={env['GIT_ASKPASS']}", "ls-remote", remote, ref],
        text=True,
        env=env,
    ).strip()
    value, separator, observed = line.partition("\t")
    if separator != "\t" or observed != ref:
        _fail(f"remote ref {ref} is not one exact commit")
    return _sha1(value, f"remote ref {ref}")


def observe_refs(remote: str, env: dict[str, str]) -> dict[str, str]:
    return {
        "channel": _observe_ref(remote, CHANNEL_REF, env),
        "authorization": _observe_ref(remote, AUTHORIZATION_REF, env),
        "promotion": _observe_ref(remote, PROMOTION_REF, env),
    }


def _fetch(repo: Path, remote: str, tag: str, env: dict[str, str]) -> None:
    _git_init(repo)
    _git(
        repo, "-c", "credential.helper=", "-c", f"core.askPass={env['GIT_ASKPASS']}", "fetch", "--no-tags", remote,
        f"+{CHANNEL_REF}:refs/remotes/smarty/channel",
        f"+{AUTHORIZATION_REF}:refs/remotes/smarty/authorization",
        f"+{PROMOTION_REF}:refs/remotes/smarty/promotion",
        f"+refs/tags/{tag}:refs/smarty/release-source",
        env=env,
    )


def _author_env(built_at: str, *, lease: bool = False) -> dict[str, str]:
    env = os.environ.copy()
    if lease:
        name, email, date = "smarty-preview", "smarty-preview@users.noreply.github.com", "2000-01-01T00:00:00Z"
    else:
        name, email, date = "github-actions[bot]", "41898282+github-actions[bot]@users.noreply.github.com", built_at
    env.update({
        "GIT_AUTHOR_NAME": name, "GIT_AUTHOR_EMAIL": email, "GIT_AUTHOR_DATE": date,
        "GIT_COMMITTER_NAME": name, "GIT_COMMITTER_EMAIL": email, "GIT_COMMITTER_DATE": date,
    })
    return env


def _json_line(value: dict[str, Any]) -> str:
    return json.dumps(value, separators=(",", ":")) + "\n"


def _tree(repo: Path, files: list[tuple[str, str, str]]) -> str:
    payload = "".join(f"{mode} blob {blob}\t{name}\n" for mode, blob, name in files)
    return _git(repo, "mktree", input=payload)


def _commit(repo: Path, tree: str, parent: str, message: str, built_at: str) -> str:
    return _git(repo, "commit-tree", tree, "-p", parent, "-m", message, env=_author_env(built_at))


def _lease_commit(repo: Path, filename: str, value: dict[str, Any], message: str) -> str:
    blob = _git(repo, "hash-object", "-w", "--stdin", input=_json_line(value))
    tree = _tree(repo, [("100644", blob, filename)])
    return _git(repo, "commit-tree", tree, "-m", message, env=_author_env("", lease=True))


def _channel_commits(repo: Path, channel: Path, previous: str, build_id: str, built_at: str) -> tuple[str, str]:
    channel = channel.resolve()
    install = _git(repo, "hash-object", "-w", "--", str(channel / "install.sh"))
    bridge_blob = _git(repo, "hash-object", "-w", "--", str(channel / "preview.json"))
    bridge = _commit(
        repo, _tree(repo, [("100755", install, "install.sh"), ("100644", bridge_blob, "preview.json")]),
        previous, f"Publish Smarty preview {build_id}", built_at,
    )
    canonical_blob = _git(repo, "hash-object", "-w", "--", str(channel / "canonical-preview.json"))
    canonical = _commit(
        repo, _tree(repo, [("100755", install, "install.sh"), ("100644", canonical_blob, "preview.json")]),
        bridge, f"Promote Smarty preview {build_id} to canonical channel", built_at,
    )
    return bridge, canonical


def _authorization_lease(repo: Path, parent: str, source: str, omp: str) -> str:
    return _lease_commit(
        repo, ".smarty-preview-authorization.json",
        {"schema": 2, "action": "windows-bootstrap-bridge", "parent": parent, "source": source, "omp": omp},
        "Authorize Smarty Preview P/R/O tuple",
    )


def _promotion_lease(repo: Path, parent: str, source: str, omp: str, tag: str, build_id: str, bridge: str) -> str:
    return _lease_commit(
        repo, ".smarty-preview-promotion-authorization.json",
        {"schema": 1, "action": "promote-windows-bootstrap-to-canonical", "parent": parent,
         "source": source, "omp": omp, "tag": tag, "build_id": build_id,
         "bridge_commit": bridge, "workflow": trusted.PUBLISH_WORKFLOW_PATH},
        "Authorize Smarty Preview canonical promotion",
    )


def _pair_inputs(release_assets: Path, seal_path: Path, *, tag: str, build_id: str, built_at: str,
                 parent: str, source: str, omp: str, omp_tree: str, omp_version: str,
                 omp_build_id: str) -> tuple[dict[str, str], dict[str, str], dict[str, Any]]:
    pair_path = release_assets / "smarty-pair.json"
    if pair_path.is_symlink() or not pair_path.is_file():
        _fail("sealed pair manifest must be a regular file")
    pair_bytes = pair_path.read_bytes()
    seal = _mapping(_json(seal_path, "sealed release handoff"), "sealed release handoff")
    record = _mapping(_mapping(seal.get("files"), "sealed release files").get("smarty-pair.json"), "sealed pair record")
    if record != {"length": len(pair_bytes), "sha256": hashlib.sha256(pair_bytes).hexdigest()}:
        _fail("sealed pair manifest bytes mismatch")
    pair = _mapping(_json(pair_path, "pair manifest"), "pair manifest")
    if pair.get("release") != {"repository": REPOSITORY, "tag": tag, "build_id": build_id, "built_at": built_at, "immutable": True}:
        _fail("pair release identity mismatch")
    sources = _mapping(pair.get("sources"), "pair sources")
    if _mapping(sources.get("parent"), "pair parent").get("commit") != parent:
        _fail("pair parent identity mismatch")
    herdr = _mapping(sources.get("herdr"), "pair Herdr source")
    if herdr.get("commit") != source or herdr.get("build_id") != build_id:
        _fail("pair Herdr identity mismatch")
    pair_omp = _mapping(sources.get("omp"), "pair OMP source")
    if pair_omp.get("commit") != omp or pair_omp.get("tree") != omp_tree or pair_omp.get("version") != omp_version or pair_omp.get("build_id") != omp_build_id:
        _fail("pair OMP identity mismatch")
    artifacts = _mapping(pair.get("artifacts"), "pair artifacts")
    herdr_shas = {platform: _sha256(_mapping(artifacts.get(name), f"pair asset {name}").get("sha256"), name)
                  for platform, name in trusted.HERDR_ASSETS.items()}
    omp_shas = {platform: _sha256(_mapping(artifacts.get(name), f"pair asset {name}").get("sha256"), name)
                for platform, name in trusted.OMP_ASSETS.items()}
    metadata = {"base_version": _one_line(herdr.get("version"), "pair Herdr version"),
                "protocol": release._protocol(herdr.get("protocol")), "pair_sha256": record["sha256"]}
    return herdr_shas, omp_shas, metadata


def _write_channel_inputs(channel: Path, pair_path: Path, *, build_id: str, source: str, omp: str,
                          omp_tree: str, omp_version: str, omp_build_id: str, metadata: dict[str, Any],
                          herdr_shas: dict[str, str], omp_shas: dict[str, str]) -> None:
    _write_json(channel / "herdr-shas.json", herdr_shas)
    _write_json(channel / "omp-shas.json", omp_shas)
    _write_json(channel / "omp-source.json", {"repository": trusted.OMP_REPOSITORY, "commit": omp, "tree": omp_tree, "version": omp_version, "build_id": omp_build_id})
    (channel / "PREVIEW_NOTES.md").write_text(f"Smarty preview build {build_id}\n\nBuilt from {source}.\n", encoding="utf-8")
    shutil.copyfile(pair_path, channel / "smarty-pair.json")
    _write_json(channel / "pair-metadata.json", metadata)


def _validate_transition(channel: Path, *, previous: dict[str, Any], bridge: dict[str, Any], canonical: dict[str, Any],
                         parent: str, source: str, omp: str, omp_tree: str, omp_version: str,
                         omp_build_id: str, tag: str, build_id: str, built_at: str,
                         metadata: dict[str, Any], herdr_shas: dict[str, str], omp_shas: dict[str, str]) -> None:
    if previous.get("schema_version") == 2:
        previous = release.canonical_manifest_from_legacy_bootstrap(previous)
    release.validate_legacy_bootstrap_manifest(bridge, canonical)
    release.validate_bootstrap_promotion(bridge, canonical)
    release.validate_channel_transition(
        previous, canonical, expected_parent=parent, expected_source=source, expected_omp=omp,
        expected_omp_tree=omp_tree, expected_omp_version=omp_version, expected_omp_build_id=omp_build_id,
        expected_tag=tag, expected_build_id=build_id, expected_built_at=built_at,
        expected_base_version=metadata["base_version"], expected_protocol=metadata["protocol"],
        expected_herdr_shas=herdr_shas, expected_omp_shas=omp_shas,
    )


def _state(value: Any) -> dict[str, Any]:
    state = _mapping(value, "phase A state")
    expected = {"schema", "repository", "producer_attempt", "mode", "tag", "build_id", "built_at", "parent", "source", "omp", "previous_channel", "authorization_lease", "bridge_commit", "canonical_commit", "promotion_lease", "pair_sha256"}
    if set(state) != expected or state.get("schema") != 1 or state.get("repository") != REPOSITORY:
        _fail("phase A state schema mismatch")
    if state.get("mode") not in {"lease", "bridge", "canonical"}:
        _fail("phase A state mode mismatch")
    _positive(state.get("producer_attempt"), "phase A producer attempt")
    identity = trusted.paired_identity(_one_line(state.get("tag"), "phase A tag"), source_sha=_sha1(state.get("source"), "phase A source"), built_at=_one_line(state.get("built_at"), "phase A built_at"))
    for key in ("build_id", "parent", "omp"):
        if state.get(key) != identity[key]:
            _fail(f"phase A {key} mismatch")
    for key in ("previous_channel", "authorization_lease", "bridge_commit", "canonical_commit", "promotion_lease"):
        _sha1(state.get(key), f"phase A {key}")
    _sha256(state.get("pair_sha256"), "phase A pair digest")
    return state


def _load_state(path: Path) -> dict[str, Any]:
    return _state(_json(path, "phase A state"))


def _artifact_binding(state: dict[str, Any], producer_attempt: int) -> None:
    if state["producer_attempt"] != _positive(producer_attempt, "producer attempt"):
        _fail("phase handoff producer attempt mismatch")



def _same_git_file(repo: Path, revision: str, path: str, local: Path) -> None:
    if _git_bytes(repo, "show", f"{revision}:{path}") != local.read_bytes():
        _fail(f"{revision}:{path} differs from rendered channel file")


def render(args: argparse.Namespace) -> None:
    root = Path(args.output_root)
    producer_attempt = _positive(args.producer_attempt, "producer attempt")
    publisher_attempt = _positive(args.publisher_attempt, "publisher attempt")
    tag = _one_line(args.tag, "tag")
    built_at = trusted.normalize_built_at(_one_line(args.built_at, "built_at"))
    parent, source, omp, omp_tree = (_sha1(args.parent, "parent"), _sha1(args.source, "source"), _sha1(args.omp, "OMP"), _sha1(args.omp_tree, "OMP tree"))
    build_id = trusted.paired_identity(tag, source_sha=source, built_at=built_at)["build_id"]
    omp_version, omp_build_id = _one_line(args.omp_version, "OMP version"), _one_line(args.omp_build_id, "OMP build ID")
    remote = _remote(args.repository)
    repo = root / "channel-repo"
    with _git_auth(os.environ.get("GH_TOKEN", "")) as env:
        observed = observe_refs(remote, env)
        _fetch(repo, remote, tag, env)
    for name, ref in (("channel", "refs/remotes/smarty/channel"), ("authorization", "refs/remotes/smarty/authorization"), ("promotion", "refs/remotes/smarty/promotion")):
        if _git(repo, "rev-parse", ref) != observed[name]:
            _fail(f"fetched {name} ref differs from observed remote ref")
    if _git(repo, "rev-parse", "refs/smarty/release-source^{commit}") != source:
        _fail("release source tag does not resolve to exact Herdr source")
    entry = _git(repo, "ls-tree", "refs/smarty/release-source", "--", "scripts/install-smarty.sh")
    if not re.fullmatch(r"100755 blob [0-9a-f]{40}\tscripts/install-smarty\.sh", entry):
        _fail("exact release source has no executable Smarty installer")
    channel = root / "channel"
    channel.mkdir(exist_ok=False)
    (channel / "install.sh").write_bytes(_git_bytes(repo, "show", "refs/smarty/release-source:scripts/install-smarty.sh"))
    (channel / "install.sh").chmod(0o755)
    herdr_shas, omp_shas, metadata = _pair_inputs(Path(args.release_assets), Path(args.final_seal), tag=tag, build_id=build_id, built_at=built_at, parent=parent, source=source, omp=omp, omp_tree=omp_tree, omp_version=omp_version, omp_build_id=omp_build_id)
    _write_channel_inputs(channel, Path(args.release_assets) / "smarty-pair.json", build_id=build_id, source=source, omp=omp, omp_tree=omp_tree, omp_version=omp_version, omp_build_id=omp_build_id, metadata=metadata, herdr_shas=herdr_shas, omp_shas=omp_shas)
    authorization = _authorization_lease(repo, parent, source, omp)
    mode: str
    previous: str
    if observed["authorization"] == authorization and observed["promotion"] == observed["channel"]:
        mode, previous = "lease", observed["channel"]
        (channel / "previous-preview.json").write_bytes(_git_bytes(repo, "show", f"{previous}:preview.json"))
        bridge_value = _mapping(
            json.loads(
                release.build_manifest(
                    channel / "previous-preview.json", REPOSITORY, tag, build_id, source, built_at,
                    metadata["base_version"], metadata["protocol"],
                    (channel / "PREVIEW_NOTES.md").read_text(encoding="utf-8"), herdr_shas,
                    CHANNEL_RETAIN, _json(channel / "omp-source.json", "OMP source"), omp_shas,
                )
            ),
            "rendered paired preview",
        )
        (channel / "preview.json").write_text(release.build_legacy_bootstrap_manifest(bridge_value), encoding="utf-8")
        (channel / "canonical-preview.json").write_text(json.dumps(release.canonical_manifest_from_legacy_bootstrap(_json(channel / "preview.json", "bridge manifest")), indent=2) + "\n", encoding="utf-8")
    elif observed["authorization"] == observed["channel"]:
        bridge_bytes = _git_bytes(repo, "show", f"{observed['channel']}:preview.json")
        bridge_value = _mapping(json.loads(bridge_bytes), "bridge manifest")
        if bridge_value.get("schema_version") == 2:
            mode, bridge = "bridge", observed["channel"]
            previous = _git(repo, "show", "-s", "--format=%P", bridge)
            _sha1(previous, "bridge parent")
            (channel / "preview.json").write_bytes(bridge_bytes)
            _same_git_file(repo, bridge, "install.sh", channel / "install.sh")
            (channel / "canonical-preview.json").write_text(json.dumps(release.canonical_manifest_from_legacy_bootstrap(bridge_value), indent=2) + "\n", encoding="utf-8")
        elif bridge_value.get("schema_version") == 1 and observed["promotion"] == observed["channel"]:
            mode, canonical = "canonical", observed["channel"]
            bridge = _git(repo, "show", "-s", "--format=%P", canonical)
            _sha1(bridge, "canonical bridge")
            previous = _git(repo, "show", "-s", "--format=%P", bridge)
            _sha1(previous, "canonical bridge predecessor")
            bridge_bytes = _git_bytes(repo, "show", f"{bridge}:preview.json")
            bridge_value = _mapping(json.loads(bridge_bytes), "bridge manifest")
            (channel / "preview.json").write_bytes(bridge_bytes)
            _same_git_file(repo, bridge, "install.sh", channel / "install.sh")
            canonical_bytes = _git_bytes(repo, "show", f"{canonical}:preview.json")
            canonical_value = _mapping(json.loads(canonical_bytes), "canonical manifest")
            expected = release.canonical_manifest_from_legacy_bootstrap(bridge_value)
            if canonical_value != expected:
                _fail("consumed canonical channel differs from bridge promotion")
            (channel / "canonical-preview.json").write_bytes(canonical_bytes)
        else:
            _fail("consumed Smarty channel is neither exact bridge nor canonical phase")
        (channel / "previous-preview.json").write_bytes(_git_bytes(repo, "show", f"{previous}:preview.json"))
    else:
        _fail("primary Smarty authorization is neither exact lease nor consumed channel")
    bridge_value = _mapping(_json(channel / "preview.json", "bridge manifest"), "bridge manifest")
    canonical_value = _mapping(_json(channel / "canonical-preview.json", "canonical manifest"), "canonical manifest")
    _validate_transition(channel, previous=_mapping(_json(channel / "previous-preview.json", "previous manifest"), "previous manifest"), bridge=bridge_value, canonical=canonical_value, parent=parent, source=source, omp=omp, omp_tree=omp_tree, omp_version=omp_version, omp_build_id=omp_build_id, tag=tag, build_id=build_id, built_at=built_at, metadata=metadata, herdr_shas=herdr_shas, omp_shas=omp_shas)
    bridge, canonical = _channel_commits(repo, channel, previous, build_id, built_at)
    promotion = _promotion_lease(repo, parent, source, omp, tag, build_id, bridge)
    if mode == "bridge" and (observed["channel"] != bridge or observed["promotion"] != promotion):
        _fail("published bridge does not match deterministic transaction")
    if mode == "canonical" and (observed["channel"] != canonical or observed["promotion"] != canonical):
        _fail("published canonical does not match deterministic transaction")
    state = {"schema": 1, "repository": REPOSITORY, "producer_attempt": producer_attempt, "mode": mode, "tag": tag, "build_id": build_id, "built_at": built_at, "parent": parent, "source": source, "omp": omp, "previous_channel": previous, "authorization_lease": authorization, "bridge_commit": bridge, "canonical_commit": canonical, "promotion_lease": promotion, "pair_sha256": metadata["pair_sha256"]}
    _write_json(root / "phase-a-state.json", state)
    _github_output("bridge_commit", bridge)
    _github_output("canonical_commit", canonical)
    _github_output("artifact_attempt", str(publisher_attempt))




def atomic_push_command(remote: str, updates: list[tuple[str, str, str]]) -> list[str]:
    command = ["git", "-c", "credential.helper=", "push", "--atomic"]
    command.extend(f"--force-with-lease={ref}:{old}" for ref, old, _ in updates)
    command.append(remote)
    command.extend(f"{new}:{ref}" for ref, _, new in updates)
    return command


def _push_atomic(repo: Path, remote: str, updates: list[tuple[str, str, str]], env: dict[str, str]) -> None:
    command = atomic_push_command(remote, updates)
    command[3:3] = ["-c", f"core.askPass={env['GIT_ASKPASS']}"]
    subprocess.check_call(command, cwd=repo, env=env)


def _phase_a_status(current: dict[str, str], state: dict[str, Any]) -> str:
    if all(current[key] == state["canonical_commit"] for key in ("channel", "authorization", "promotion")):
        return "canonical"
    if current == {"channel": state["bridge_commit"], "authorization": state["bridge_commit"], "promotion": state["promotion_lease"]}:
        return "bridge"
    expected = {"channel": state["previous_channel"], "authorization": state["authorization_lease"], "promotion": state["previous_channel"]}
    if state["mode"] == "lease" and current == expected:
        return "lease"
    _fail("channel refs changed outside the expected Phase A retry states")


def publish_bridge(args: argparse.Namespace) -> None:
    state = _load_state(Path(args.state))
    _artifact_binding(state, _positive(args.producer_attempt, "producer attempt"))
    remote = _remote(args.repository)
    with _git_auth(os.environ.get("GH_TOKEN", "")) as env:
        current = observe_refs(remote, env)
        if _phase_a_status(current, state) == "lease":
            repo = Path(args.workdir) / "channel-publish"
            _git_init(repo)
            _git(repo, "-c", "credential.helper=", "-c", f"core.askPass={env['GIT_ASKPASS']}", "fetch", "--no-tags", remote, f"+{CHANNEL_REF}:refs/remotes/smarty/channel", env=env)
            actual_bridge, _ = _channel_commits(repo, Path(args.channel), state["previous_channel"], state["build_id"], state["built_at"])
            if actual_bridge != state["bridge_commit"]:
                _fail("bridge commit differs from Phase A handoff")
            actual_promotion = _promotion_lease(repo, state["parent"], state["source"], state["omp"], state["tag"], state["build_id"], actual_bridge)
            if actual_promotion != state["promotion_lease"]:
                _fail("promotion lease differs from Phase A handoff")
            _push_atomic(repo, remote, [(CHANNEL_REF, state["previous_channel"], actual_bridge), (AUTHORIZATION_REF, state["authorization_lease"], actual_bridge), (PROMOTION_REF, state["previous_channel"], actual_promotion)], env)


def _review_release(tag: str, root: Path) -> None:
    query = "query($owner:String!,$name:String!,$tag:String!){repository(owner:$owner,name:$name){release(tagName:$tag){databaseId}}}"
    release_id = subprocess.check_output(["gh", "api", "graphql", "-f", f"query={query}", "-F", "owner=Smarty-Pants-Inc", "-F", "name=herdr", "-F", f"tag={tag}", "--jq", ".data.repository.release.databaseId"], text=True).strip()
    if not re.fullmatch(r"[1-9][0-9]*", release_id):
        _fail("immutable release ID is invalid")
    (root / "reviewed-release.json").write_text(subprocess.check_output(["gh", "api", f"repos/{REPOSITORY}/releases/{release_id}"], text=True), encoding="utf-8")
    reviewed = root / "reviewed-release"
    reviewed.mkdir(exist_ok=False)
    subprocess.check_call(["gh", "release", "download", tag, "--repo", REPOSITORY, "--pattern", "smarty-pair.json", "--dir", str(reviewed)])


def review(args: argparse.Namespace) -> None:
    root = Path(args.workdir)
    state = _load_state(Path(args.state))
    _artifact_binding(state, _positive(args.producer_attempt, "producer attempt"))
    publisher_attempt = _positive(args.publisher_attempt, "publisher attempt")
    if state["tag"] != _one_line(args.tag, "tag") or state["parent"] != _sha1(args.parent, "parent") or state["source"] != _sha1(args.source, "source") or state["omp"] != _sha1(args.omp, "OMP"):
        _fail("protected promotion state identity mismatch")
    remote = _remote(args.repository)
    repo = root / "promotion-review"
    with _git_auth(os.environ.get("GH_TOKEN", "")) as env:
        _fetch(repo, remote, state["tag"], env)
    _review_release(state["tag"], root)
    if _git(repo, "rev-parse", "refs/smarty/release-source") != state["source"]:
        _fail("reviewed release tag differs from Phase A source")
    current = {"channel": _git(repo, "rev-parse", "refs/remotes/smarty/channel"), "authorization": _git(repo, "rev-parse", "refs/remotes/smarty/authorization"), "promotion": _git(repo, "rev-parse", "refs/remotes/smarty/promotion")}
    status = _phase_a_status(current, state)
    if status not in {"bridge", "canonical"}:
        _fail("protected promotion did not observe bridge or canonical state")
    if _git(repo, "show", "-s", "--format=%P", state["bridge_commit"]) != state["previous_channel"]:
        _fail("bridge parent differs from Phase A handoff")
    actual_bridge, actual_canonical = _channel_commits(repo, Path(args.channel), state["previous_channel"], state["build_id"], state["built_at"])
    if actual_bridge != state["bridge_commit"] or actual_canonical != state["canonical_commit"]:
        _fail("protected promotion commits differ from Phase A handoff")
    for revision, file, local in ((state["bridge_commit"], "preview.json", "preview.json"), (state["bridge_commit"], "install.sh", "install.sh"), (state["canonical_commit"], "preview.json", "canonical-preview.json"), (state["canonical_commit"], "install.sh", "install.sh")):
        _same_git_file(repo, revision, file, Path(args.channel) / local)
    pair_bytes = (root / "reviewed-release" / "smarty-pair.json").read_bytes()
    if hashlib.sha256(pair_bytes).hexdigest() != state["pair_sha256"] or pair_bytes != (Path(args.channel) / "smarty-pair.json").read_bytes():
        _fail("protected promotion pair manifest bytes mismatch")
    release_data = _mapping(_json(root / "reviewed-release.json", "reviewed release"), "reviewed release")
    assets = release_data.get("assets")
    pair_asset = next((item for item in assets if isinstance(item, dict) and item.get("name") == "smarty-pair.json"), None) if isinstance(assets, list) else None
    if release_data.get("tag_name") != state["tag"] or release_data.get("draft") is not False or release_data.get("prerelease") is not True or release_data.get("immutable") is not True:
        _fail("protected promotion release is not immutable preview")
    if not isinstance(pair_asset, dict) or pair_asset.get("size") != len(pair_bytes) or pair_asset.get("digest") != f"sha256:{state['pair_sha256']}":
        _fail("protected promotion pair asset metadata mismatch")
    pair = _mapping(json.loads(pair_bytes), "reviewed pair manifest")
    sources = _mapping(pair.get("sources"), "reviewed pair sources")
    if pair.get("release", {}).get("tag") != state["tag"] or sources.get("parent", {}).get("commit") != state["parent"] or sources.get("herdr", {}).get("commit") != state["source"] or sources.get("omp", {}).get("commit") != state["omp"]:
        _fail("protected promotion pair P/R/O mismatch")
    bridge_value, canonical_value = _json(Path(args.channel) / "preview.json", "bridge manifest"), _json(Path(args.channel) / "canonical-preview.json", "canonical manifest")
    release.validate_legacy_bootstrap_manifest(bridge_value, canonical_value)
    release.validate_bootstrap_promotion(bridge_value, canonical_value)
    authorization = {"schema": 1, "status": "approved", "environment": "smarty-preview-promotion", "producer_attempt": state["producer_attempt"], "observed_phase": status, "tag": state["tag"], "parent": state["parent"], "source": state["source"], "omp": state["omp"], "bridge_commit": state["bridge_commit"], "canonical_commit": state["canonical_commit"], "promotion_lease": state["promotion_lease"]}
    _write_json(root / "promotion-authorization.json", authorization)
    _github_output("canonical_commit", state["canonical_commit"])
    _github_output("artifact_attempt", str(publisher_attempt))


def _authorization(path: Path, state: dict[str, Any], producer_attempt: int) -> None:
    value = _mapping(_json(path, "promotion authorization"), "promotion authorization")
    expected = {"schema": 1, "status": "approved", "environment": "smarty-preview-promotion", "producer_attempt": _positive(producer_attempt, "producer attempt"), "tag": state["tag"], "parent": state["parent"], "source": state["source"], "omp": state["omp"], "bridge_commit": state["bridge_commit"], "canonical_commit": state["canonical_commit"], "promotion_lease": state["promotion_lease"]}
    if {key: value.get(key) for key in expected} != expected or value.get("observed_phase") not in {"bridge", "canonical"} or set(value) != {*expected, "observed_phase"}:
        _fail("protected promotion authorization does not bind the exact handoff")


def publish_canonical(args: argparse.Namespace) -> None:
    state = _load_state(Path(args.state))
    producer_attempt = _positive(args.producer_attempt, "producer attempt")
    _artifact_binding(state, producer_attempt)
    if state["canonical_commit"] != _sha1(args.canonical_commit, "approved canonical commit"):
        _fail("protected promotion output differs from Phase A canonical commit")
    _authorization(Path(args.authorization), state, producer_attempt)
    remote = _remote(args.repository)
    with _git_auth(os.environ.get("GH_TOKEN", "")) as env:
        current = observe_refs(remote, env)
        if not all(current[key] == state["canonical_commit"] for key in current):
            expected = {"channel": state["bridge_commit"], "authorization": state["bridge_commit"], "promotion": state["promotion_lease"]}
            if current != expected:
                _fail("channel refs changed outside the protected promotion retry states")
            repo = Path(args.workdir) / "promotion-publish"
            _git_init(repo)
            _git(repo, "-c", "credential.helper=", "-c", f"core.askPass={env['GIT_ASKPASS']}", "fetch", "--no-tags", remote, f"+{CHANNEL_REF}:refs/remotes/smarty/channel", env=env)
            actual_bridge, actual_canonical = _channel_commits(repo, Path(args.channel), state["previous_channel"], state["build_id"], state["built_at"])
            if actual_bridge != state["bridge_commit"] or actual_canonical != state["canonical_commit"]:
                _fail("canonical commit differs from protected handoff")
            _push_atomic(repo, remote, [(CHANNEL_REF, state["bridge_commit"], actual_canonical), (AUTHORIZATION_REF, state["bridge_commit"], actual_canonical), (PROMOTION_REF, state["promotion_lease"], actual_canonical)], env)
        final = observe_refs(remote, env)
    if not all(value == state["canonical_commit"] for value in final.values()):
        _fail("canonical promotion did not advance all protected refs")
    _write_json(Path(args.output), {"schema": 1, "status": "canonical", "tag": state["tag"], "parent": state["parent"], "source": state["source"], "omp": state["omp"], "bridge_commit": state["bridge_commit"], "canonical_commit": state["canonical_commit"], "refs": {"smarty-channel": state["canonical_commit"], "smarty-preview-authorization": state["canonical_commit"], "smarty-preview-promotion-authorization": state["canonical_commit"]}})


def _github_output(key: str, value: str) -> None:
    output = os.environ.get("GITHUB_OUTPUT")
    if output:
        with Path(output).open("a", encoding="utf-8") as file:
            file.write(f"{key}={value}\n")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    sub = result.add_subparsers(required=True)
    render_parser = sub.add_parser("render")
    for name in ("repository", "tag", "built-at", "parent", "source", "omp", "omp-tree", "omp-version", "omp-build-id", "release-assets", "final-seal", "output-root"):
        render_parser.add_argument(f"--{name}", required=True)
    render_parser.add_argument("--producer-attempt", type=int, required=True)
    render_parser.add_argument("--publisher-attempt", type=int, required=True)
    render_parser.set_defaults(func=render)
    bridge = sub.add_parser("publish-bridge")
    bridge.add_argument("--repository", required=True)
    bridge.add_argument("--state", required=True)
    bridge.add_argument("--channel", required=True)
    bridge.add_argument("--workdir", default=".")
    bridge.add_argument("--producer-attempt", type=int, required=True)
    bridge.set_defaults(func=publish_bridge)
    review_parser = sub.add_parser("review")
    for name in ("repository", "state", "channel", "tag", "parent", "source", "omp"):
        review_parser.add_argument(f"--{name}", required=True)
    review_parser.add_argument("--workdir", default=".")
    review_parser.add_argument("--producer-attempt", type=int, required=True)
    review_parser.add_argument("--publisher-attempt", type=int, required=True)
    review_parser.set_defaults(func=review)
    canonical = sub.add_parser("publish-canonical")
    for name in ("repository", "state", "channel", "authorization", "canonical-commit", "output"):
        canonical.add_argument(f"--{name}", required=True)
    canonical.add_argument("--workdir", default=".")
    canonical.add_argument("--producer-attempt", type=int, required=True)
    canonical.set_defaults(func=publish_canonical)
    return result


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        args.func(args)
    except (OSError, subprocess.CalledProcessError, ValueError, json.JSONDecodeError) as error:
        print(f"trusted preview promotion failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
