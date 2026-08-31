from __future__ import annotations

import argparse
import hashlib
import json
import tempfile
import unittest
from contextlib import nullcontext
from pathlib import Path
from unittest import mock

import scripts.smarty_preview_promotion as promotion
import scripts.smarty_preview_trusted as trusted


class TrustedPromotionTransactionTests(unittest.TestCase):
    parent = "1" * 40
    source = "2" * 40
    omp = "3" * 40
    tree = "4" * 40
    previous = "5" * 40
    authorization = "6" * 40
    bridge = "7" * 40
    canonical = "8" * 40
    promotion_lease = "9" * 40
    built_at = "2026-08-22T00:00:00Z"

    def _state(self, *, pair_sha256: str = "a" * 64) -> dict[str, object]:
        tag = f"smarty-preview-2026-08-22-p{self.parent}-r{self.source}-o{self.omp}"
        return {
            "schema": 1,
            "repository": trusted.REPOSITORY,
            "producer_attempt": 7,
            "mode": "lease",
            "tag": tag,
            "build_id": tag.removeprefix("smarty-preview-"),
            "built_at": self.built_at,
            "parent": self.parent,
            "source": self.source,
            "omp": self.omp,
            "previous_channel": self.previous,
            "authorization_lease": self.authorization,
            "bridge_commit": self.bridge,
            "canonical_commit": self.canonical,
            "promotion_lease": self.promotion_lease,
            "pair_sha256": pair_sha256,
        }

    def _write_state(self, root: Path, **kwargs: object) -> tuple[Path, dict[str, object]]:
        state = self._state(**kwargs)
        path = root / "phase-a-state.json"
        path.write_text(json.dumps(state), encoding="utf-8")
        return path, state


    def test_channel_commits_read_sibling_channel_files_from_temporary_repository(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            channel = root / "channel"
            channel.mkdir()
            (channel / "install.sh").write_bytes(b"#!/bin/sh\necho install\n")
            (channel / "preview.json").write_bytes(b'{"schema_version":2}\n')
            (channel / "canonical-preview.json").write_bytes(b'{"schema_version":1}\n')
            repo = root / "channel-repo"
            promotion._git_init(repo)
            previous = promotion._lease_commit(repo, "lease.json", {"schema": 1}, "lease")
            bridge, canonical = promotion._channel_commits(
                repo, channel, previous, "test-build", self.built_at
            )
            self.assertEqual(promotion._git(repo, "show", "-s", "--format=%P", bridge), previous)
            self.assertEqual(promotion._git_bytes(repo, "show", f"{bridge}:install.sh"), (channel / "install.sh").read_bytes())
            self.assertEqual(promotion._git_bytes(repo, "show", f"{bridge}:preview.json"), (channel / "preview.json").read_bytes())
            self.assertEqual(promotion._git(repo, "show", "-s", "--format=%P", canonical), bridge)
            self.assertEqual(promotion._git_bytes(repo, "show", f"{canonical}:preview.json"), (channel / "canonical-preview.json").read_bytes())

    def test_phase_state_binds_producer_attempt_and_rejects_mismatch(self) -> None:
        state = promotion._state(self._state())
        promotion._artifact_binding(state, 7)
        with self.assertRaisesRegex(ValueError, "producer attempt mismatch"):
            promotion._artifact_binding(state, 8)
        malformed = self._state()
        malformed["producer_attempt"] = 0
        with self.assertRaisesRegex(ValueError, "positive integer"):
            promotion._state(malformed)

    def test_pair_input_rejects_malformed_or_mismatched_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            assets = root / "assets"
            assets.mkdir()
            tag = self._state()["tag"]
            artifacts = {name: {"sha256": "b" * 64} for name in (*trusted.HERDR_ASSETS.values(), *trusted.OMP_ASSETS.values())}
            pair = {
                "release": {"repository": trusted.REPOSITORY, "tag": tag, "build_id": str(tag).removeprefix("smarty-preview-"), "built_at": self.built_at, "immutable": True},
                "sources": {
                    "parent": {"commit": self.parent},
                    "herdr": {"commit": self.source, "build_id": str(tag).removeprefix("smarty-preview-"), "version": "1.2.3", "protocol": 1},
                    "omp": {"commit": self.omp, "tree": self.tree, "version": "1.2.3", "build_id": f"managed-omp-{self.tree}"},
                },
                "artifacts": artifacts,
            }
            pair_path = assets / "smarty-pair.json"
            pair_path.write_text(json.dumps(pair), encoding="utf-8")
            seal = root / "sealed-handoff.json"
            seal.write_text(json.dumps({"files": {"smarty-pair.json": trusted.file_record(pair_path)}}), encoding="utf-8")
            promotion._pair_inputs(assets, seal, tag=str(tag), build_id=str(tag).removeprefix("smarty-preview-"), built_at=self.built_at, parent=self.parent, source=self.source, omp=self.omp, omp_tree=self.tree, omp_version="1.2.3", omp_build_id=f"managed-omp-{self.tree}")
            pair["artifacts"]["herdr-linux-x86_64"] = {}
            pair_path.write_text(json.dumps(pair), encoding="utf-8")
            seal.write_text(json.dumps({"files": {"smarty-pair.json": trusted.file_record(pair_path)}}), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "SHA-256 digest"):
                promotion._pair_inputs(assets, seal, tag=str(tag), build_id=str(tag).removeprefix("smarty-preview-"), built_at=self.built_at, parent=self.parent, source=self.source, omp=self.omp, omp_tree=self.tree, omp_version="1.2.3", omp_build_id=f"managed-omp-{self.tree}")

    def test_phase_a_retry_states_are_exact(self) -> None:
        state = promotion._state(self._state())
        self.assertEqual(promotion._phase_a_status({"channel": self.previous, "authorization": self.authorization, "promotion": self.previous}, state), "lease")
        self.assertEqual(promotion._phase_a_status({"channel": self.bridge, "authorization": self.bridge, "promotion": self.promotion_lease}, state), "bridge")
        self.assertEqual(promotion._phase_a_status({"channel": self.canonical, "authorization": self.canonical, "promotion": self.canonical}, state), "canonical")
        with self.assertRaisesRegex(ValueError, "changed outside"):
            promotion._phase_a_status({"channel": "0" * 40, "authorization": self.authorization, "promotion": self.previous}, state)

    def test_render_uses_canonical_bridge_predecessor_and_treats_version_as_data(self) -> None:
        malicious_version = "17.4.0'; touch /tmp/preview-promotion-injected; #"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tag = f"smarty-preview-2026-08-22-p{self.parent}-r{self.source}-o{self.omp}"
            arguments = [
                "render", "--repository", trusted.REPOSITORY, "--tag", tag,
                "--built-at", self.built_at, "--parent", self.parent, "--source", self.source,
                "--omp", self.omp, "--omp-tree", self.tree, "--omp-version", malicious_version,
                "--omp-build-id", f"managed-omp-{self.tree}", "--producer-attempt", "7",
                "--publisher-attempt", "3", "--release-assets", str(root / "assets"),
                "--final-seal", str(root / "seal.json"), "--output-root", str(root),
            ]
            observed = {"channel": self.canonical, "authorization": self.canonical, "promotion": self.canonical}
            captured: dict[str, object] = {}

            def git(_: Path, *command: str, **__: object) -> str:
                if command[:2] == ("rev-parse", "refs/remotes/smarty/channel"):
                    return self.canonical
                if command[:2] == ("rev-parse", "refs/remotes/smarty/authorization"):
                    return self.canonical
                if command[:2] == ("rev-parse", "refs/remotes/smarty/promotion"):
                    return self.canonical
                if command == ("rev-parse", "refs/smarty/release-source^{commit}"):
                    return self.source
                if command == ("ls-tree", "refs/smarty/release-source", "--", "scripts/install-smarty.sh"):
                    return "100755 blob 0" * 0 + f"100755 blob {self.previous}\tscripts/install-smarty.sh"
                if command == ("show", "-s", "--format=%P", self.canonical):
                    return self.bridge
                if command == ("show", "-s", "--format=%P", self.bridge):
                    return self.previous
                raise AssertionError(command)

            def git_bytes(_: Path, *command: str, **__: object) -> bytes:
                values = {
                    ("show", "refs/smarty/release-source:scripts/install-smarty.sh"): b"#!/bin/sh\n",
                    ("show", f"{self.bridge}:preview.json"): b'{"schema_version":2}',
                    ("show", f"{self.canonical}:preview.json"): b'{"schema_version":1}',
                    ("show", f"{self.previous}:preview.json"): b'{"schema_version":1}',
                }
                return values[command]

            def channel_commits(repo: Path, channel: Path, previous: str, _: str, __: str) -> tuple[str, str]:
                self.assertEqual(repo, root / "channel-repo")
                self.assertEqual(channel, root / "channel")
                self.assertEqual(previous, self.previous)
                return self.bridge, self.canonical

            def pair_inputs(*_: object, **kwargs: object) -> tuple[dict[str, str], dict[str, str], dict[str, object]]:
                captured["pair_version"] = kwargs["omp_version"]
                return {}, {}, {"base_version": "1.2.3", "protocol": 1, "pair_sha256": "a" * 64}

            def write_inputs(*_: object, **kwargs: object) -> None:
                captured["written_version"] = kwargs["omp_version"]

            with (
                mock.patch.object(promotion, "_git_auth", return_value=nullcontext({})),
                mock.patch.object(promotion, "observe_refs", return_value=observed),
                mock.patch.object(promotion, "_fetch"),
                mock.patch.object(promotion, "_git", side_effect=git),
                mock.patch.object(promotion, "_git_bytes", side_effect=git_bytes),
                mock.patch.object(promotion, "_pair_inputs", side_effect=pair_inputs),
                mock.patch.object(promotion, "_write_channel_inputs", side_effect=write_inputs),
                mock.patch.object(promotion, "_authorization_lease", return_value=self.authorization),
                mock.patch.object(promotion, "_same_git_file"),
                mock.patch.object(promotion, "_validate_transition"),
                mock.patch.object(promotion, "_channel_commits", side_effect=channel_commits),
                mock.patch.object(promotion, "_promotion_lease", return_value=self.promotion_lease),
                mock.patch.object(promotion.release, "canonical_manifest_from_legacy_bootstrap", return_value={"schema_version": 1}),
            ):
                self.assertEqual(promotion.main(arguments), 0)

            self.assertEqual(captured, {"pair_version": malicious_version, "written_version": malicious_version})
            self.assertEqual(json.loads((root / "phase-a-state.json").read_text(encoding="utf-8"))["previous_channel"], self.previous)

    def test_publish_bridge_directly_uses_exact_lease_cas(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state_path, state = self._write_state(root)
            pushed: list[list[tuple[str, str, str]]] = []
            args = argparse.Namespace(repository=trusted.REPOSITORY, state=state_path, channel=root / "channel", workdir=root, producer_attempt=7)
            current = {"channel": self.previous, "authorization": self.authorization, "promotion": self.previous}
            with (
                mock.patch.object(promotion, "_git_auth", return_value=nullcontext({"GIT_ASKPASS": "askpass"})),
                mock.patch.object(promotion, "observe_refs", return_value=current),
                mock.patch.object(promotion, "_git_init"),
                mock.patch.object(promotion, "_git"),
                mock.patch.object(promotion, "_channel_commits", return_value=(self.bridge, self.canonical)),
                mock.patch.object(promotion, "_promotion_lease", return_value=self.promotion_lease),
                mock.patch.object(promotion, "_push_atomic", side_effect=lambda _, __, updates, ___: pushed.append(updates)),
            ):
                promotion.publish_bridge(args)
            self.assertEqual(pushed, [[
                (promotion.CHANNEL_REF, state["previous_channel"], self.bridge),
                (promotion.AUTHORIZATION_REF, state["authorization_lease"], self.bridge),
                (promotion.PROMOTION_REF, state["previous_channel"], self.promotion_lease),
            ]])

    def test_review_directly_binds_bridge_and_release(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pair = {
                "release": {"tag": self._state()["tag"]},
                "sources": {"parent": {"commit": self.parent}, "herdr": {"commit": self.source}, "omp": {"commit": self.omp}},
            }
            pair_bytes = json.dumps(pair).encode("utf-8")
            state_path, state = self._write_state(root, pair_sha256=hashlib.sha256(pair_bytes).hexdigest())
            channel = root / "channel"
            channel.mkdir()
            (channel / "preview.json").write_text("{}", encoding="utf-8")
            (channel / "canonical-preview.json").write_text("{}", encoding="utf-8")
            (channel / "smarty-pair.json").write_bytes(pair_bytes)

            def review_release(_: str, workdir: Path) -> None:
                reviewed = workdir / "reviewed-release"
                reviewed.mkdir()
                (reviewed / "smarty-pair.json").write_bytes(pair_bytes)
                (workdir / "reviewed-release.json").write_text(json.dumps({
                    "tag_name": state["tag"], "draft": False, "prerelease": True, "immutable": True,
                    "assets": [{"name": "smarty-pair.json", "size": len(pair_bytes), "digest": f"sha256:{state['pair_sha256']}"}],
                }), encoding="utf-8")

            def git(_: Path, *command: str, **__: object) -> str:
                values = {
                    ("rev-parse", "refs/smarty/release-source"): self.source,
                    ("rev-parse", "refs/remotes/smarty/channel"): self.bridge,
                    ("rev-parse", "refs/remotes/smarty/authorization"): self.bridge,
                    ("rev-parse", "refs/remotes/smarty/promotion"): self.promotion_lease,
                    ("show", "-s", "--format=%P", self.bridge): self.previous,
                }
                return values[command]

            args = argparse.Namespace(repository=trusted.REPOSITORY, state=state_path, channel=channel, tag=state["tag"], parent=self.parent, source=self.source, omp=self.omp, workdir=root, producer_attempt=7, publisher_attempt=4)
            with (
                mock.patch.object(promotion, "_git_auth", return_value=nullcontext({})),
                mock.patch.object(promotion, "_fetch"),
                mock.patch.object(promotion, "_review_release", side_effect=review_release),
                mock.patch.object(promotion, "_git", side_effect=git),
                mock.patch.object(promotion, "_channel_commits", return_value=(self.bridge, self.canonical)),
                mock.patch.object(promotion, "_same_git_file"),
                mock.patch.object(promotion.release, "validate_legacy_bootstrap_manifest"),
                mock.patch.object(promotion.release, "validate_bootstrap_promotion"),
            ):
                promotion.review(args)
            self.assertEqual(json.loads((root / "promotion-authorization.json").read_text(encoding="utf-8"))["observed_phase"], "bridge")

    def test_publish_canonical_directly_accepts_k_k_k_retry_without_push(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state_path, state = self._write_state(root)
            authorization = {
                "schema": 1, "status": "approved", "environment": "smarty-preview-promotion", "producer_attempt": 7,
                "observed_phase": "canonical", "tag": state["tag"], "parent": self.parent, "source": self.source,
                "omp": self.omp, "bridge_commit": self.bridge, "canonical_commit": self.canonical,
                "promotion_lease": self.promotion_lease,
            }
            authorization_path = root / "promotion-authorization.json"
            authorization_path.write_text(json.dumps(authorization), encoding="utf-8")
            output = root / "channel-promotion.json"
            current = {"channel": self.canonical, "authorization": self.canonical, "promotion": self.canonical}
            args = argparse.Namespace(repository=trusted.REPOSITORY, state=state_path, channel=root / "channel", authorization=authorization_path, canonical_commit=self.canonical, workdir=root, producer_attempt=7, output=output)
            with (
                mock.patch.object(promotion, "_git_auth", return_value=nullcontext({})),
                mock.patch.object(promotion, "observe_refs", return_value=current),
                mock.patch.object(promotion, "_push_atomic") as push,
            ):
                promotion.publish_canonical(args)
            push.assert_not_called()
            self.assertEqual(json.loads(output.read_text(encoding="utf-8"))["refs"]["smarty-channel"], self.canonical)

    def test_atomic_push_command_is_exact(self) -> None:
        command = promotion.atomic_push_command("https://example.invalid/herdr.git", [
            (promotion.CHANNEL_REF, "1" * 40, "2" * 40),
            (promotion.AUTHORIZATION_REF, "3" * 40, "2" * 40),
            (promotion.PROMOTION_REF, "4" * 40, "2" * 40),
        ])
        self.assertEqual(command, [
            "git", "-c", "credential.helper=", "push", "--atomic",
            f"--force-with-lease={promotion.CHANNEL_REF}:{'1' * 40}",
            f"--force-with-lease={promotion.AUTHORIZATION_REF}:{'3' * 40}",
            f"--force-with-lease={promotion.PROMOTION_REF}:{'4' * 40}",
            "https://example.invalid/herdr.git",
            f"{'2' * 40}:{promotion.CHANNEL_REF}",
            f"{'2' * 40}:{promotion.AUTHORIZATION_REF}",
            f"{'2' * 40}:{promotion.PROMOTION_REF}",
        ])

    def test_push_atomic_passes_complete_git_config_pairs_to_subprocess(self) -> None:
        remote = "https://example.invalid/herdr.git"
        updates = [(promotion.CHANNEL_REF, "1" * 40, "2" * 40)]
        env = {"GIT_ASKPASS": "/tmp/askpass"}
        with mock.patch.object(promotion.subprocess, "check_call") as check_call:
            promotion._push_atomic(Path("/tmp/channel"), remote, updates, env)
        self.assertEqual(check_call.call_args.args[0], [
            "git", "-c", "credential.helper=", "-c", "core.askPass=/tmp/askpass", "push", "--atomic",
            f"--force-with-lease={promotion.CHANNEL_REF}:{'1' * 40}", remote,
            f"{'2' * 40}:{promotion.CHANNEL_REF}",
        ])


class PhaseAWorkflowTests(unittest.TestCase):
    def test_phase_a_passes_artifact_values_through_environment(self) -> None:
        source = (Path(__file__).resolve().parents[1] / ".github/workflows/smarty-preview-publish.yml").read_text(encoding="utf-8")
        phase_a = source.split("\n  phase-a-channel:", 1)[1].split("\n  phase-a-commit:", 1)[0]
        self.assertIn("OMP_VERSION: ${{ needs.validate-seal.outputs.omp_version }}", phase_a)
        self.assertIn('--omp-version "$OMP_VERSION"', phase_a)

    def test_render_workflow_binds_producer_and_publisher_attempts(self) -> None:
        source = (Path(__file__).resolve().parents[1] / ".github/workflows/smarty-preview-publish.yml").read_text(encoding="utf-8")
        phase_a = source.split("\n  phase-a-channel:", 1)[1].split("\n  phase-a-commit:", 1)[0]
        self.assertIn('--producer-attempt "${{ needs.validate-seal.outputs.run_attempt }}"', phase_a)
        self.assertIn('--publisher-attempt "$GITHUB_RUN_ATTEMPT"', phase_a)
        self.assertNotIn('--omp-version "${{', phase_a)
