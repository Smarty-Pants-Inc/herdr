import base64
import hashlib
import json
import os
import re
import stat
import subprocess
import sys
import tempfile
import unittest
import zipfile
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

import scripts.conventional_commits as conventional_commits
import scripts.preview as preview

CI_WORKFLOW_PATH = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"
PR_GATE_PATH = Path(__file__).resolve().parents[1] / ".github/workflows/pr-gate.yml"


def pr_gate_source() -> str:
    return PR_GATE_PATH.read_text(encoding="utf-8")


def ci_workflow_source() -> str:
    return CI_WORKFLOW_PATH.read_text(encoding="utf-8")


class CiWorkflowTests(unittest.TestCase):
    def test_mergify_queue_ci_uses_exact_identity_and_non_cancelling_runs(self):
        source = ci_workflow_source()
        self.assertIn("github.event.pull_request.user.id == 37929162", source)
        self.assertIn("github.event.pull_request.user.id != 37929162", source)
        self.assertIn(
            "startsWith(github.event.pull_request.head.ref, 'mergify/merge-queue/') && github.run_id",
            source,
        )
        self.assertIn("cancel-in-progress: ${{ github.event_name != 'pull_request'", source)

class PrGateWorkflowTests(unittest.TestCase):
    def test_mergify_queue_exemption_requires_bot_id_prefix_and_allowed_base(self):
        source = pr_gate_source()
        self.assertIn("const MERGIFY_BOT_USER_ID = 37929162;", source)
        self.assertIn(
            "const MERGIFY_QUEUE_BASES = new Set(['master', 'smarty-preview-source']);",
            source,
        )
        self.assertIn("pr.user.id === MERGIFY_BOT_USER_ID", source)
        self.assertIn("pr.head.ref.startsWith('mergify/merge-queue/')", source)
        self.assertIn("MERGIFY_QUEUE_BASES.has(pr.base.ref)", source)

OMP_SOURCE = {
    "repository": "Smarty-Pants-Inc/oh-my-pi",
    "commit": "a" * 40,
    "tree": "b" * 40,
    "version": "17.3.7",
    "build_id": "omp-build-a",
}
OMP_SHAS = {target: "c" * 64 for target in preview.OMP_ASSET_TARGETS}
HERDR_SHAS = {target: "d" * 64 for target in preview.ASSET_TARGETS}
WORKFLOW_PATH = (
    Path(__file__).resolve().parents[1] / ".github/workflows/smarty-preview.yml"
)
PROMOTION_WORKFLOW_PATH = (
    Path(__file__).resolve().parents[1] / ".github/workflows/smarty-preview-promote.yml"
)
INSTALLER_PATH = Path(__file__).resolve().parents[1] / "website/install.ps1"


def promotion_workflow_source() -> str:
    return PROMOTION_WORKFLOW_PATH.read_text(encoding="utf-8")


def workflow_source() -> str:
    return WORKFLOW_PATH.read_text(encoding="utf-8")


def workflow_jobs(source: str) -> dict[str, str]:
    """Split the workflow into job identifier to job body text."""

    jobs = {}
    current = None
    started = False
    for line in source.splitlines(keepends=True):
        if not started:
            started = line.rstrip("\n") == "jobs:"
            continue
        header = re.fullmatch(r"  ([a-z][a-z0-9-]*):\n?", line)
        if header:
            current = header.group(1)
            jobs[current] = []
        elif current is not None:
            jobs[current].append(line)
    return {name: "".join(body) for name, body in jobs.items()}


def workflow_steps(job: str) -> dict[str, str]:
    """Split one job body into ordered step name to step body text."""

    steps = {}
    current = None
    for line in job.splitlines(keepends=True):
        named = re.fullmatch(r"      - name: (.*)\n?", line)
        if named:
            current = named.group(1)
            steps[current] = []
        elif re.fullmatch(r"      - .*\n?", line):
            current = None
        elif current is not None:
            steps[current].append(line)
    return {name: "".join(body) for name, body in steps.items()}


class PreviewNotesTests(unittest.TestCase):
    def test_humanize_groups_conventional_subjects(self):
        self.assertEqual(
            preview.humanize_subject("feat(update): add preview channel"),
            ("Added", "Add preview channel"),
        )
        self.assertEqual(
            preview.humanize_subject("fix: handle preview manifest"),
            ("Fixed", "Handle preview manifest"),
        )
        self.assertEqual(
            preview.humanize_subject("not conventional"),
            ("Other", "Not conventional"),
        )

    def test_build_manifest_archives_current_assets(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            notes = "Preview notes\n"
            content = preview.build_manifest(
                output=output,
                repo="herdrdev/herdr",
                tag="preview-2026-06-02-abcdef123456",
                build_id="2026-06-02-abcdef123456",
                commit="abcdef1234567890",
                built_at="2026-06-02T03:00:00Z",
                base_version="0.6.6",
                protocol=12,
                notes=notes,
                shas={
                    **HERDR_SHAS,
                    "linux-x86_64": "e" * 64,
                    "windows-x86_64": "a" * 64,
                },
                retain=30,
                omp_source=OMP_SOURCE,
                omp_shas=OMP_SHAS,
            )
            data = json.loads(content)
            self.assertEqual(data["channel"], "preview")
            self.assertEqual(data["build_id"], "2026-06-02-abcdef123456")
            self.assertEqual(
                data["assets"]["linux-x86_64"]["sha256"],
                "e" * 64,
            )
            self.assertEqual(
                data["assets"]["windows-x86_64"]["url"],
                "https://github.com/herdrdev/herdr/releases/download/preview-2026-06-02-abcdef123456/herdr-windows-x86_64.zip",
            )
            self.assertEqual(
                data["assets"]["windows-x86_64"]["sha256"],
                "a" * 64,
            )
            self.assertEqual(data["assets"]["windows-x86_64"]["format"], "zip")
            self.assertIn("2026-06-02-abcdef123456", data["builds"])
            self.assertEqual(data["omp"]["build_id"], "omp-build-a")
            self.assertEqual(data["omp"]["commit"], "a" * 40)
            self.assertEqual(data["omp"]["tree"], "b" * 40)
            self.assertEqual(data["omp"]["version"], "17.3.7")
            self.assertEqual(
                data["omp"]["assets"]["macos-aarch64"]["sha256"],
                "c" * 64,
            )
            self.assertEqual(
                set(data["omp"]["assets"]),
                {
                    "linux-x86_64",
                    "linux-aarch64",
                    "macos-x86_64",
                    "macos-aarch64",
                },
            )
            self.assertEqual(
                data["omp"]["assets"]["linux-x86_64"]["url"],
                "https://github.com/herdrdev/herdr/releases/download/preview-2026-06-02-abcdef123456/omp-linux-x86_64",
            )
            self.assertEqual(data["builds"][data["build_id"]]["omp"], data["omp"])

    def test_manifest_retains_current_and_retain_minus_one_history(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"

            def render(build_id: str, built_at: str, retain: int) -> dict:
                content = preview.build_manifest(
                    output=output,
                    repo="herdrdev/herdr",
                    tag=f"preview-{build_id}",
                    build_id=build_id,
                    commit=f"commit-{build_id}",
                    built_at=built_at,
                    base_version="0.6.6",
                    protocol=12,
                    notes=build_id,
                    shas=HERDR_SHAS,
                    retain=retain,
                )
                output.write_text(content, encoding="utf-8")
                return json.loads(content)

            render("oldest", "2026-06-01T00:00:00Z", 3)
            render("newer", "2026-06-02T00:00:00Z", 3)
            manifest = render("current", "2026-06-03T00:00:00Z", 2)

            self.assertEqual(list(manifest["builds"]), ["current", "newer"])
            self.assertEqual(
                preview.validate_current_archive(manifest, "preview-current"), manifest
            )
            current = manifest["builds"]["current"]
            self.assertEqual(
                {
                    key: manifest[key]
                    for key in (
                        "base_version",
                        "commit",
                        "built_at",
                        "protocol",
                        "assets",
                    )
                },
                {
                    key: current[key]
                    for key in (
                        "base_version",
                        "commit",
                        "built_at",
                        "protocol",
                        "assets",
                    )
                },
            )
            self.assertEqual(current["tag"], "preview-current")

            single = render("single", "2026-06-04T00:00:00Z", 1)
            self.assertEqual(list(single["builds"]), ["single", "current"])
            self.assertEqual(
                preview.validate_current_archive(single, "preview-single"), single
            )

    def _render_paired(self, output, day, herdr, retain):
        built_at = f"{day}T00:00:00Z"
        build_id = preview.paired_build_id(
            built_at, "1" * 40, herdr, OMP_SOURCE["commit"]
        )
        content = preview.build_manifest(
            output=output,
            repo="Smarty-Pants-Inc/herdr",
            tag=f"smarty-preview-{build_id}",
            build_id=build_id,
            commit=herdr,
            built_at=built_at,
            base_version="0.8.2",
            protocol=12,
            notes=build_id,
            shas=HERDR_SHAS,
            retain=retain,
            omp_source=OMP_SOURCE,
            omp_shas=OMP_SHAS,
        )
        output.write_text(content, encoding="utf-8")
        return json.loads(content)

    def _validate_paired_transition(self, previous, candidate, retain, consumed=False):
        build_id = candidate["build_id"]
        return preview.validate_channel_transition(
            previous,
            candidate,
            expected_parent="1" * 40,
            expected_source=candidate["commit"],
            expected_omp=OMP_SOURCE["commit"],
            expected_omp_tree=OMP_SOURCE["tree"],
            expected_omp_version=OMP_SOURCE["version"],
            expected_omp_build_id=OMP_SOURCE["build_id"],
            expected_tag=f"smarty-preview-{build_id}",
            expected_build_id=build_id,
            expected_built_at=candidate["built_at"],
            expected_base_version="0.8.2",
            expected_protocol=12,
            expected_herdr_shas=HERDR_SHAS,
            expected_omp_shas=OMP_SHAS,
            consumed=consumed,
            retain=retain,
        )

    def test_windows_bootstrap_bridge_round_trips_exact_canonical_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            canonical = self._render_paired(output, "2026-08-22", "4" * 40, 30)
            bridge = json.loads(preview.build_legacy_bootstrap_manifest(canonical))
            alias = preview.legacy_bootstrap_build_id(canonical["build_id"])

            self.assertEqual(len(canonical["build_id"]), 136)
            self.assertEqual(len(alias), 74)
            self.assertRegex(alias, r"^bootstrap-[0-9a-f]{64}$")
            self.assertEqual(bridge["schema_version"], 2)
            self.assertEqual(bridge["build_id"], alias)
            self.assertEqual(bridge["canonical_build_id"], canonical["build_id"])
            self.assertEqual(set(bridge["assets"]), {"windows-x86_64"})
            self.assertEqual(bridge["builds"], canonical["builds"])
            self.assertEqual(
                bridge["bootstrap"],
                {
                    "schema": preview.LEGACY_BOOTSTRAP_SCHEMA,
                    "paired_build_id": canonical["build_id"],
                    "paired_tag": f"smarty-preview-{canonical['build_id']}",
                    "paired_manifest": "preview.json",
                    "windows_asset_sha256": canonical["assets"]["windows-x86_64"][
                        "sha256"
                    ],
                },
            )
            self.assertEqual(
                preview.validate_legacy_bootstrap_manifest(bridge, canonical), bridge
            )
            self.assertEqual(
                preview.canonical_manifest_from_legacy_bootstrap(bridge), canonical
            )
            self.assertEqual(
                preview.validate_bootstrap_promotion(bridge, canonical), canonical
            )

    def test_windows_bootstrap_bridge_tampering_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            canonical = self._render_paired(output, "2026-08-22", "4" * 40, 30)
            bridge = json.loads(preview.build_legacy_bootstrap_manifest(canonical))

            mutations = (
                lambda value: value.update(build_id="bootstrap-" + "0" * 64),
                lambda value: value.update(canonical_build_id="not-a-paired-id"),
                lambda value: value["assets"]["windows-x86_64"].update(sha256="0" * 64),
                lambda value: value["bootstrap"].update(paired_manifest="other.json"),
                lambda value: value["builds"][canonical["build_id"]]["assets"].pop(
                    "linux-x86_64"
                ),
                lambda value: value["builds"][canonical["build_id"]]["assets"][
                    "windows-x86_64"
                ].update(sha256="0" * 64),
                lambda value: value.update(base_version="9.9.9"),
                lambda value: value["omp"].update(commit="0" * 40),
                lambda value: value.update(protocol=value["protocol"] + 1),
                lambda value: value["omp"].update(tree="0" * 40),
                lambda value: value["omp"].update(version="999.0.0"),
                lambda value: value["omp"].update(build_id="wrong-omp-build"),
                lambda value: value["builds"].pop(canonical["build_id"]),
            )
            for mutate in mutations:
                tampered = json.loads(json.dumps(bridge))
                mutate(tampered)
                with self.assertRaises(ValueError):
                    preview.validate_legacy_bootstrap_manifest(tampered, canonical)

            promoted = json.loads(json.dumps(canonical))
            promoted["notes"] += " changed"
            with self.assertRaisesRegex(
                ValueError, "promotion changes canonical channel state"
            ):
                preview.validate_bootstrap_promotion(bridge, promoted)

    def test_channel_transition_accepts_rendered_history_for_every_retention(self):
        for retain in (1, 2, 30):
            with self.subTest(retain=retain), tempfile.TemporaryDirectory() as tmp:
                output = Path(tmp) / "preview.json"
                first = self._render_paired(output, "2026-08-20", "2" * 40, retain)
                self._validate_paired_transition(None, first, retain)
                previous = json.loads(output.read_text(encoding="utf-8"))
                second = self._render_paired(output, "2026-08-21", "3" * 40, retain)
                self._validate_paired_transition(previous, second, retain)
                previous = json.loads(output.read_text(encoding="utf-8"))
                third = self._render_paired(output, "2026-08-22", "4" * 40, retain)
                self._validate_paired_transition(previous, third, retain)
                self.assertIn(previous["build_id"], third["builds"])
                self.assertEqual(list(third["builds"])[0], third["build_id"])
                self.assertEqual(len(third["builds"]), {1: 2, 2: 2, 30: 3}[retain])

    def test_channel_transition_rejects_dropped_or_mutated_retained_history(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            self._render_paired(output, "2026-08-20", "2" * 40, 30)
            previous = json.loads(output.read_text(encoding="utf-8"))
            candidate = self._render_paired(output, "2026-08-21", "3" * 40, 30)
            self._validate_paired_transition(previous, candidate, 30)

            dropped = json.loads(json.dumps(candidate))
            del dropped["builds"][previous["build_id"]]
            with self.assertRaisesRegex(ValueError, "deterministic retained prefix"):
                self._validate_paired_transition(previous, dropped, 30)

            mutated = json.loads(json.dumps(candidate))
            mutated["builds"][previous["build_id"]]["base_version"] = "0.8.3"
            with self.assertRaisesRegex(ValueError, "changed historical build"):
                self._validate_paired_transition(previous, mutated, 30)

            replayed = json.loads(json.dumps(previous))
            with self.assertRaisesRegex(ValueError, "already exists in authenticated"):
                self._validate_paired_transition(previous, replayed, 30)

            with self.assertRaisesRegex(
                ValueError, "consumed channel candidate differs"
            ):
                self._validate_paired_transition(previous, candidate, 30, consumed=True)

    def test_channel_transition_rejects_malformed_fixed_schema_fields(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            candidate = self._render_paired(output, "2026-08-22", "4" * 40, 30)

            malformed = json.loads(json.dumps(candidate))
            malformed["unexpected"] = True
            with self.assertRaisesRegex(
                ValueError, "channel manifest must contain exactly"
            ):
                self._validate_paired_transition(None, malformed, 30)

            malformed = json.loads(json.dumps(candidate))
            malformed["base_version"] = "0.8.3"
            malformed["builds"][malformed["build_id"]]["base_version"] = "0.8.3"
            with self.assertRaisesRegex(
                ValueError, "candidate channel current identity mismatch"
            ):
                self._validate_paired_transition(None, malformed, 30)

            malformed = json.loads(json.dumps(candidate))
            malformed["protocol"] = True
            malformed["builds"][malformed["build_id"]]["protocol"] = True
            with self.assertRaisesRegex(
                ValueError, "protocol must be a positive integer"
            ):
                self._validate_paired_transition(None, malformed, 30)

            malformed = json.loads(json.dumps(candidate))
            malformed["notes"] = ["not", "text"]
            with self.assertRaisesRegex(ValueError, "notes must be a string"):
                self._validate_paired_transition(None, malformed, 30)

            malformed = json.loads(json.dumps(candidate))
            malformed["omp"]["version"] += "\n"
            malformed["builds"][malformed["build_id"]]["omp"]["version"] += "\n"
            with self.assertRaisesRegex(
                ValueError, "must be a nonempty one-line string"
            ):
                self._validate_paired_transition(None, malformed, 30)

            malformed = json.loads(json.dumps(candidate))
            malformed["assets"]["windows-x86_64"]["format"] = "tar"
            malformed["builds"][malformed["build_id"]]["assets"]["windows-x86_64"][
                "format"
            ] = "tar"
            with self.assertRaisesRegex(ValueError, "format mismatch"):
                self._validate_paired_transition(None, malformed, 30)

    def test_preview_assets_all_require_sha256(self):
        with tempfile.TemporaryDirectory() as tmp:
            shas = dict(HERDR_SHAS)
            del shas["linux-aarch64"]
            with self.assertRaisesRegex(
                ValueError, "Herdr SHA targets must be exactly"
            ):
                preview.build_manifest(
                    output=Path(tmp) / "preview.json",
                    repo="herdrdev/herdr",
                    tag="preview-test",
                    build_id="test",
                    commit="abcdef",
                    built_at="2026-06-02T03:00:00Z",
                    base_version="0.6.6",
                    protocol=12,
                    notes="test",
                    shas=shas,
                    retain=1,
                    omp_source=OMP_SOURCE,
                    omp_shas=OMP_SHAS,
                )

    def test_omp_source_descriptor_rejects_replacement_placeholders(self):
        with tempfile.TemporaryDirectory() as tmp:
            descriptor = Path(tmp) / "omp-source.json"
            source = dict(OMP_SOURCE)
            source["commit"] = "REPLACE_WITH_OMP_COMMIT_SHA"
            descriptor.write_text(json.dumps(source), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "replacement placeholder"):
                preview.read_omp_source(descriptor)

    def test_omp_preview_assets_all_require_sha256(self):
        with tempfile.TemporaryDirectory() as tmp:
            omp_shas = dict(OMP_SHAS)
            del omp_shas["linux-aarch64"]
            with self.assertRaisesRegex(ValueError, "OMP SHA targets must be exactly"):
                preview.build_manifest(
                    output=Path(tmp) / "preview.json",
                    repo="herdrdev/herdr",
                    tag="preview-test",
                    build_id="test",
                    commit="abcdef",
                    built_at="2026-06-02T03:00:00Z",
                    base_version="0.6.6",
                    protocol=12,
                    notes="test",
                    shas=HERDR_SHAS,
                    retain=1,
                    omp_source=OMP_SOURCE,
                    omp_shas=omp_shas,
                )

    def test_manifest_asset_target_sets_reject_unexpected_keys(self):
        with self.assertRaisesRegex(ValueError, "Herdr SHA targets must be exactly"):
            preview.asset_objects(
                preview.default_asset_urls("herdrdev/herdr", "preview-test"),
                {**HERDR_SHAS, "linux-gnu-x86_64": "e" * 64},
            )
        with self.assertRaisesRegex(ValueError, "OMP SHA targets must be exactly"):
            preview.omp_asset_objects(
                preview.default_omp_asset_urls("herdrdev/herdr", "preview-test"),
                {**OMP_SHAS, "windows-x86_64": "e" * 64},
            )

    def test_manifest_cli_omp_pair_is_optional_but_atomic(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            output = root / "preview.json"
            notes = root / "notes.md"
            sha_file = root / "herdr-shas.json"
            omp_source = root / "omp-source.json"
            omp_sha_file = root / "omp-shas.json"
            notes.write_text("Preview notes\n", encoding="utf-8")
            sha_file.write_text(json.dumps(HERDR_SHAS), encoding="utf-8")
            omp_source.write_text(json.dumps(OMP_SOURCE), encoding="utf-8")
            omp_sha_file.write_text(json.dumps(OMP_SHAS), encoding="utf-8")
            command = [
                sys.executable,
                str(Path(preview.__file__).resolve()),
                "manifest",
                "--output",
                str(output),
                "--repo",
                "herdrdev/herdr",
                "--tag",
                "preview-test",
                "--build-id",
                "test",
                "--commit",
                "abcdef",
                "--built-at",
                "2026-06-02T03:00:00Z",
                "--base-version",
                "0.6.6",
                "--protocol",
                "12",
                "--notes",
                str(notes),
                "--sha-file",
                str(sha_file),
            ]

            result = subprocess.run(
                command, capture_output=True, text=True, check=False
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = json.loads(output.read_text(encoding="utf-8"))
            self.assertNotIn("omp", manifest)
            self.assertEqual(manifest["builds"]["test"]["commit"], "abcdef")
            self.assertNotIn("omp", manifest["builds"]["test"])

            for incomplete_pair in (
                ["--omp-source", str(omp_source)],
                ["--omp-sha-file", str(omp_sha_file)],
            ):
                with self.subTest(arguments=incomplete_pair):
                    result = subprocess.run(
                        [*command, *incomplete_pair],
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn(
                        "--omp-source and --omp-sha-file must be provided together",
                        result.stderr,
                    )

            result = subprocess.run(
                [
                    *command,
                    "--omp-source",
                    str(omp_source),
                    "--omp-sha-file",
                    str(omp_sha_file),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            manifest = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(manifest["omp"]["build_id"], OMP_SOURCE["build_id"])
            self.assertEqual(manifest["builds"]["test"]["omp"], manifest["omp"])

    def test_retained_build_keeps_its_exact_omp_pair(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            old_source = {
                **OMP_SOURCE,
                "commit": "d" * 40,
                "tree": "e" * 40,
                "build_id": "omp-build-old",
            }
            output.write_text(
                preview.build_manifest(
                    output=output,
                    repo="herdrdev/herdr",
                    tag="preview-old",
                    build_id="herdr-old",
                    commit="old",
                    built_at="2026-06-01T03:00:00Z",
                    base_version="0.6.6",
                    protocol=12,
                    notes="old",
                    shas={target: "f" * 64 for target in preview.ASSET_TARGETS},
                    retain=30,
                    omp_source=old_source,
                    omp_shas={target: "1" * 64 for target in preview.OMP_ASSET_TARGETS},
                ),
                encoding="utf-8",
            )
            current = json.loads(
                preview.build_manifest(
                    output=output,
                    repo="herdrdev/herdr",
                    tag="preview-new",
                    build_id="herdr-new",
                    commit="new",
                    built_at="2026-06-02T03:00:00Z",
                    base_version="0.6.6",
                    protocol=12,
                    notes="new",
                    shas={target: "2" * 64 for target in preview.ASSET_TARGETS},
                    retain=30,
                    omp_source=OMP_SOURCE,
                    omp_shas=OMP_SHAS,
                )
            )
            self.assertEqual(
                current["builds"]["herdr-old"]["omp"]["build_id"], "omp-build-old"
            )
            self.assertEqual(
                current["builds"]["herdr-new"]["omp"]["build_id"], "omp-build-a"
            )

    def test_retained_legacy_build_without_omp_is_retained(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            legacy = {
                "base_version": "0.6.5",
                "commit": "legacy",
                "built_at": "2026-05-31T03:00:00Z",
                "protocol": 11,
                "tag": "preview-legacy",
                "assets": preview.asset_objects(
                    preview.default_asset_urls("herdrdev/herdr", "preview-legacy"),
                    HERDR_SHAS,
                ),
            }
            invalid_claimed_pair = {**legacy, "omp": None}
            output.write_text(
                json.dumps(
                    {
                        "builds": {
                            "herdr-legacy": legacy,
                            "invalid-claimed-pair": invalid_claimed_pair,
                        }
                    }
                ),
                encoding="utf-8",
            )

            current = json.loads(
                preview.build_manifest(
                    output=output,
                    repo="herdrdev/herdr",
                    tag="preview-new",
                    build_id="herdr-new",
                    commit="new",
                    built_at="2026-06-02T03:00:00Z",
                    base_version="0.6.6",
                    protocol=12,
                    notes="new",
                    shas={target: "2" * 64 for target in preview.ASSET_TARGETS},
                    retain=30,
                    omp_source=OMP_SOURCE,
                    omp_shas=OMP_SHAS,
                )
            )

            self.assertIn("herdr-legacy", current["builds"])
            self.assertNotIn("omp", current["builds"]["herdr-legacy"])
            self.assertNotIn("invalid-claimed-pair", current["builds"])
            self.assertEqual(current["builds"]["herdr-new"]["omp"], current["omp"])

    def test_retained_builds_with_inexact_or_unverified_pairs_are_pruned(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "preview.json"
            paired = json.loads(
                preview.build_manifest(
                    output=output,
                    repo="herdrdev/herdr",
                    tag="preview-old",
                    build_id="herdr-old",
                    commit="old",
                    built_at="2026-06-01T03:00:00Z",
                    base_version="0.6.6",
                    protocol=12,
                    notes="old",
                    shas={target: "f" * 64 for target in preview.ASSET_TARGETS},
                    retain=30,
                    omp_source=OMP_SOURCE,
                    omp_shas=OMP_SHAS,
                )
            )["builds"]["herdr-old"]
            missing_checksum = json.loads(json.dumps(paired))
            del missing_checksum["omp"]["assets"]["linux-aarch64"]["sha256"]
            wrong_target = json.loads(json.dumps(paired))
            wrong_target["omp"]["assets"]["linux-gnu-x86_64"] = wrong_target["omp"][
                "assets"
            ].pop("linux-x86_64")
            output.write_text(
                json.dumps(
                    {
                        "builds": {
                            "missing-checksum": missing_checksum,
                            "wrong-target": wrong_target,
                        }
                    }
                ),
                encoding="utf-8",
            )

            current = json.loads(
                preview.build_manifest(
                    output=output,
                    repo="herdrdev/herdr",
                    tag="preview-new",
                    build_id="herdr-new",
                    commit="new",
                    built_at="2026-06-02T03:00:00Z",
                    base_version="0.6.6",
                    protocol=12,
                    notes="new",
                    shas={target: "2" * 64 for target in preview.ASSET_TARGETS},
                    retain=30,
                    omp_source=OMP_SOURCE,
                    omp_shas=OMP_SHAS,
                )
            )

            self.assertEqual(set(current["builds"]), {"herdr-new"})

    def test_hidden_subjects_include_preview_manifest_commits(self):
        self.assertTrue(preview.hidden_subject("docs: update preview manifest"))
        self.assertTrue(preview.hidden_subject("docs: update website manifest"))
        self.assertFalse(preview.hidden_subject("release: v0.7.0"))
        self.assertFalse(preview.hidden_subject("fix: repair preview manifest"))

    def test_latest_publishable_commit_keeps_release_commits(self):
        output = "\n".join(
            [
                "manifest\x00docs: update website manifest for v0.7.0",
                "release\x00release: v0.7.0",
                "feature\x00feat: add plugin v1 system",
            ]
        )
        with mock.patch.object(preview, "run_git", return_value=output):
            self.assertEqual(
                preview.latest_publishable_commit("origin/master"), "release"
            )

    def test_preview_range_base_advances_to_stable_tag(self):
        with (
            mock.patch.object(preview, "latest_stable_tag", return_value="v0.7.0"),
            mock.patch.object(preview, "git_is_ancestor", return_value=True),
        ):
            self.assertEqual(
                preview.preview_range_base("previous-preview", "release"),
                "v0.7.0",
            )

    def test_preview_range_base_keeps_previous_preview_for_unreleased_work(self):
        def is_ancestor(ancestor: str, descendant: str) -> bool:
            return (ancestor, descendant) == ("v0.7.0", "new-feature")

        with (
            mock.patch.object(preview, "latest_stable_tag", return_value="v0.7.0"),
            mock.patch.object(preview, "git_is_ancestor", side_effect=is_ancestor),
        ):
            self.assertEqual(
                preview.preview_range_base("previous-preview", "new-feature"),
                "previous-preview",
            )

    def test_post_stable_history_selects_release_and_bases_range_on_stable_tag(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp)

            def git(*args: str) -> str:
                return subprocess.check_output(
                    ["git", *args],
                    cwd=repo,
                    text=True,
                    stderr=subprocess.DEVNULL,
                ).strip()

            git("init")
            git("config", "user.email", "test@example.com")
            git("config", "user.name", "Test User")

            marker = repo / "marker.txt"
            marker.write_text("preview\n", encoding="utf-8")
            git("add", "marker.txt")
            git("commit", "-m", "feat: previous preview")
            previous_preview = git("rev-parse", "HEAD")

            marker.write_text("release\n", encoding="utf-8")
            git("commit", "-am", "release: v0.7.0")
            release = git("rev-parse", "HEAD")
            git("tag", "v0.7.0")

            marker.write_text("manifest\n", encoding="utf-8")
            git("commit", "-am", "docs: update website manifest for v0.7.0")

            original_cwd = os.getcwd()
            try:
                os.chdir(repo)
                self.assertEqual(preview.latest_publishable_commit("HEAD"), release)
                self.assertEqual(
                    preview.preview_range_base(previous_preview, release),
                    "v0.7.0",
                )
            finally:
                os.chdir(original_cwd)

    def test_preview_docs_rewrite_links_to_preview_namespace(self):
        source = """---
title: Install Herdr
---

import ConfigReference from '../../components/ConfigReference.astro';
import LocaleWidget from '../../../components/LocaleWidget.astro';

[Install](/docs/install/)
file: ../../../public/assets/logo.svg
"""
        output = subprocess.check_output(
            [
                "node",
                "website/scripts/prepare-docs.mjs",
                "--rewrite-preview-doc-fixture",
            ],
            input=source,
            text=True,
        )
        self.assertIn("[Install](/docs/preview/install/)", output)
        self.assertIn("file: ../../../../public/assets/logo.svg", output)
        self.assertIn("from '../../../components/ConfigReference.astro'", output)
        self.assertIn("from '../../../../components/LocaleWidget.astro'", output)
        self.assertIn("Preview build `2026-07-29-44b3adb12552`", output)
        self.assertIn(
            "blob/44b3adb125524ea9a55739eee3776f922f2115ad/docs/next/website/src/content/docs/",
            output,
        )

    def test_version_docs_rewrite_links_and_source_paths(self):
        source = """---
title: Install Herdr
---

import ConfigReference from '../../components/ConfigReference.astro';

[Install](/docs/install/)
[Skill](https://github.com/herdrdev/herdr/blob/master/SKILL.md)
file: ../../../public/assets/logo.svg
"""
        output = subprocess.check_output(
            [
                "node",
                "website/scripts/prepare-docs.mjs",
                "--rewrite-version-doc-fixture",
                "0.7.4",
            ],
            input=source,
            text=True,
        )
        self.assertIn("[Install](/docs/0.7.4/install/)", output)
        self.assertIn("file: ../../../../../public/assets/logo.svg", output)
        self.assertIn("from '../../../../components/ConfigReference.astro'", output)
        self.assertIn(
            "blob/master/docs/versions/0.7.4/website/src/content/docs/index.mdx", output
        )
        self.assertIn("blob/v0.7.4/SKILL.md", output)

    def test_smarty_preview_verifies_compiled_omp_version(self):
        workflow = workflow_source()
        steps = workflow_steps(workflow_jobs(workflow)["build"])
        source_identity = steps["Verify paired OMP source identity"]
        build_identity = steps["Verify candidate OMP build identity"]

        self.assertIn("packages/coding-agent/package.json", source_identity)
        self.assertIn("needs.preflight.outputs.omp_version", source_identity)
        self.assertLess(
            workflow.index("- name: Verify candidate OMP build identity"),
            workflow.index("- name: Upload paired candidate payload"),
        )
        self.assertIn(
            'test "$("$omp_binary" __build-id)" = "$EXPECTED_OMP_BUILD_ID"',
            build_identity,
        )
        self.assertIn(
            'test "$("$omp_binary" --version)" = "omp/$EXPECTED_OMP_VERSION"',
            build_identity,
        )
        self.assertIn('"$omp_binary" --smoke-test', build_identity)
        self.assertNotIn("${{", build_identity.split("run: |", 1)[1])

    def test_smarty_preview_checks_exact_macos_zig_version(self):
        workflow = (
            Path(__file__).resolve().parents[1] / ".github/workflows/smarty-preview.yml"
        ).read_text(encoding="utf-8")
        install = workflow.split("- name: Install patched Zig on macOS", 1)[1].split(
            "- name: Prefer official Ubuntu mirrors over Azure", 1
        )[0]

        self.assertIn('zig_prefix="$(brew --prefix zig@0.15)"', install)
        self.assertIn(
            'test "$("$zig_prefix/bin/zig" version)" = "$ZIG_VERSION"', install
        )

    def test_smarty_preview_uses_injective_paired_build_identity(self):
        root = Path(__file__).resolve().parents[1]
        official = (root / ".github/workflows/preview.yml").read_text(encoding="utf-8")
        smarty = (root / ".github/workflows/smarty-preview.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn("on:\n  push:\n    tags:\n      - 'smarty-preview-*'", smarty)
        self.assertNotIn("workflow_dispatch", smarty)
        self.assertIn('tag="$EVENT_REF_NAME"', smarty)
        self.assertIn('test "$EVENT_SHA" = "$source"', smarty)
        self.assertIn("ref: ${{ github.sha }}", smarty)
        self.assertIn(
            'merge-base --is-ancestor "$EXPECTED_PARENT" refs/remotes/origin/main',
            smarty,
        )
        self.assertIn('--source-ref "refs/tags/$TAG"', smarty)

        self.assertIn('build_id="$day-$short_sha"', official)
        self.assertNotIn("--format=%cs", smarty)
        self.assertIn(
            'build_id = f"{source_built_at[:10]}-p{expected_parent}-r{expected_source}-o{expected_omp}"',
            smarty,
        )
        self.assertIn('"build_id": build_id', smarty)
        self.assertIn('"tag": tag', smarty)
        self.assertIn('output.write(f"{key}={value}\\n")', smarty)

    def test_smarty_preview_guards_authorization_and_records_native_toolchains(self):
        workflow = workflow_source()
        jobs = workflow_jobs(workflow)
        candidate_validation = workflow_steps(jobs["preflight"])[
            "Validate exact P/R/O candidate"
        ]
        render = workflow_steps(jobs["render-channel"])
        snapshot = render["Snapshot Smarty channel for rendering"]
        advance = workflow_steps(jobs["advance-channel"])[
            "Advance Smarty channel with compare-and-swap"
        ]

        self.assertIn(
            'git_auth -C parent-source fetch --no-tags "$parent_remote"', workflow
        )
        self.assertIn("'+refs/heads/main:refs/remotes/origin/main'", workflow)
        self.assertIn('-c credential.helper= -c core.askPass="$askpass"', workflow)
        self.assertNotIn("https://x-access-token:", workflow)
        self.assertIn(
            "authorization_ref=refs/heads/smarty-preview-authorization", workflow
        )
        self.assertIn(
            "authorization_ref: ${{ steps.plan.outputs.authorization_ref }}", workflow
        )
        self.assertIn(
            "authorization_state: ${{ steps.plan.outputs.authorization_state }}",
            workflow,
        )
        self.assertIn('authorization_state = "consumed"', candidate_validation)
        self.assertNotIn("EXPECTED_AUTHORIZATION_LEASE", workflow)
        self.assertIn(
            '"--force-with-lease=$authorization_ref:$EXPECTED_AUTHORIZATION_REF"',
            advance,
        )
        self.assertIn(
            '"--force-with-lease=$channel_ref:$EXPECTED_CHANNEL_COMMIT"', advance
        )
        self.assertIn(
            '"$candidate:$channel_ref" "$candidate:$authorization_ref"', advance
        )
        self.assertIn("push --atomic", advance)
        self.assertIn("commit-tree", advance)
        self.assertIn('cat-file blob "$observed_channel:preview.json"', advance)
        self.assertIn("EXPECTED_AUTHORIZATION_STATE", advance)
        self.assertIn("--toolchain_resolution_debug='.*rules_rust.*'", workflow)
        self.assertIn("report: omp-rules-rust-linux-x86_64.json", jobs["build"])
        self.assertIn('f"omp-rules-rust-{platform}.json"', jobs["assemble-spdx"])
        self.assertIn(
            "--omp-rules-rust-toolchains candidate/metadata/omp-rules-rust-toolchains.json",
            jobs["attest-pair"],
        )

        self.assertIn("unset SMARTY_RELEASE_TOKEN", candidate_validation)
        self.assertIn("env -i", candidate_validation)
        self.assertNotIn("from scripts.preview", candidate_validation)
        self.assertNotIn("scripts/preview.py", candidate_validation)
        self.assertNotIn("scripts.preview", snapshot)
        self.assertNotIn("python3", snapshot)
        self.assertNotIn("python3", advance)
        self.assertNotIn("scripts.preview", advance)

        credential_variables = (
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "SMARTY_RELEASE_TOKEN",
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
            "ACTIONS_ID_TOKEN_REQUEST_URL",
        )
        sanitized = (
            render["Render Smarty channel candidate"],
            render["Verify rendered channel transition with candidate code"],
        )
        for step in sanitized:
            self.assertIn("env -i", step)
            for credential in credential_variables:
                self.assertNotIn(credential, step)
        for step in sanitized:
            for forwarded in ('BASE_VERSION="$BASE_VERSION"', 'BUILT_AT="$BUILT_AT"'):
                self.assertIn(forwarded, step)

        for name in (
            "Require release token",
            "Parse immutable release tag",
            "Validate exact P/R/O candidate",
            "Inspect immutable release reuse",
            "Verify publication prerequisites",
            "Fetch exact immutable release before build reuse",
            "Verify exact immutable release attestations before build reuse",
            "Create or reconcile draft-first paired release",
            "Fetch reconciled draft release",
            "Verify reconciled draft attestations",
            "Publish verified draft",
            "Fetch immutable paired release",
            "Verify immutable paired release attestations",
            "Record TUF promotion input for external signing",
            "Verify rendered Smarty channel candidate",
            "Advance Smarty channel with compare-and-swap",
        ):
            authenticated = workflow.split(f"- name: {name}\n", 1)[1].split(
                "\n      - name:", 1
            )[0]
            for candidate_command in (
                "scripts/preview.py",
                "from scripts.preview",
                "herdr-source/",
                "cargo fmt",
                "cargo build",
                "cargo metadata",
                "bazel --",
                "bun install",
                "bun scripts",
                "just test",
            ):
                self.assertNotIn(candidate_command, authenticated, name)

        windows_package = workflow_steps(jobs["build"])[
            "Package Windows candidate payload"
        ]
        self.assertIn(".\\herdr-source\\scripts\\package_windows_conpty.ps1", workflow)
        for credential in credential_variables:
            self.assertNotIn(credential, windows_package)

    def test_privileged_preview_verifiers_are_env_isolated(self):
        jobs = workflow_jobs(workflow_source())
        cases = (
            (
                "attest-spdx",
                "Verify exact SPDX attestation inputs",
                "candidate/handoff/trusted-release-verifier.py spdx",
            ),
            (
                "attest-pair",
                "Verify exact pair attestation inputs",
                "candidate/handoff/trusted-release-verifier.py pair-manifest",
            ),
        )
        for job, step, command in cases:
            self.assertIn("id-token: write", jobs[job])
            body = workflow_steps(jobs[job])[step]
            invocation = f"/usr/bin/env -i /usr/bin/python3 -I -S {command}"
            self.assertIn(invocation, body)
            isolated = body[body.index(invocation) :]
            for credential in (
                "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
                "ACTIONS_ID_TOKEN_REQUEST_URL",
                "GH_TOKEN",
                "GITHUB_TOKEN",
                "SMARTY_RELEASE_TOKEN",
            ):
                self.assertNotIn(credential, isolated)

    def test_promotion_renderer_drops_release_credentials_before_helpers(self):
        render = workflow_steps(workflow_jobs(promotion_workflow_source())["promote"])[
            "Render exact canonical channel from authenticated verifier"
        ]
        cleanup = '          /bin/rm -f "$askpass"\n'
        unset = "          unset GH_TOKEN\n"
        helper = (
            "/usr/bin/git -C promotion-repo show "
            "refs/smarty/promotion-source:scripts/preview.py"
        )
        invocation = (
            "/usr/bin/env -i /usr/bin/python3 -I -S trusted-preview.py "
            "promote-bootstrap-manifest"
        )
        self.assertLess(render.index(cleanup), render.index(unset))
        self.assertLess(render.index(unset), render.index("git_public() {"))
        self.assertLess(render.index(unset), render.index(helper))
        self.assertLess(render.index(helper), render.index(invocation))
        isolated = render[render.index(invocation) :]
        self.assertNotIn("GH_TOKEN", isolated)
        self.assertNotIn("GIT_ASKPASS", isolated)

    def test_candidate_packagers_and_legacy_renderer_are_env_isolated(self):
        jobs = workflow_jobs(workflow_source())
        windows = workflow_steps(jobs["build"])["Package Windows candidate payload"]
        self.assertIn("& $envExe -i `", windows)
        self.assertIn("$pwsh -NoLogo -NoProfile -NonInteractive", windows)
        self.assertIn('"DOTNET_CLI_HOME=$trustedHome"', windows)
        for credential in (
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
            "ACTIONS_ID_TOKEN_REQUEST_URL",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "SMARTY_RELEASE_TOKEN",
        ):
            self.assertNotIn(credential, windows)
        render = workflow_steps(jobs["render-channel"])[
            "Render Smarty channel candidate"
        ]
        self.assertIn(
            "/usr/bin/env -i /usr/bin/python3 -S "
            "herdr-source/scripts/preview.py legacy-bootstrap-manifest",
            render,
        )
        self.assertGreaterEqual(render.count("env -i"), 2)

    def test_consumed_channel_requires_exact_tree_modes_at_every_boundary(self):
        jobs = workflow_jobs(workflow_source())
        candidate = workflow_steps(jobs["preflight"])["Validate exact P/R/O candidate"]
        archive = workflow_steps(jobs["preflight"])["Archive validated source inputs"]
        advance = workflow_steps(jobs["advance-channel"])[
            "Advance Smarty channel with compare-and-swap"
        ]
        self.assertIn('"install.sh": ("100755", "blob")', candidate)
        self.assertIn('"preview.json": ("100644", "blob")', candidate)
        for body in (archive, advance):
            self.assertIn("install.sh:100755|preview.json:100644", body)
            self.assertIn("ls-tree -r", body)
        self.assertIn('cat-file blob "$observed_channel:preview.json"', advance)
        self.assertIn('cat-file blob "$observed_channel:install.sh"', advance)

    def test_workflows_canonicalize_source_commit_time_to_utc_z(self):
        candidate = workflow_steps(workflow_jobs(workflow_source())["preflight"])[
            "Validate exact P/R/O candidate"
        ]
        promotion = workflow_steps(
            workflow_jobs(promotion_workflow_source())["promote"]
        )["Render exact canonical channel from authenticated verifier"]
        self.assertEqual(candidate.count("--format=%cI"), 1)
        self.assertNotIn("--format=%cs", candidate)
        self.assertIn("from datetime import datetime, timezone", candidate)
        self.assertIn("source_time.astimezone(timezone.utc).strftime(", candidate)
        self.assertIn('"%Y-%m-%dT%H:%M:%SZ"', candidate)
        self.assertIn('"built_at": source_built_at', candidate)
        self.assertNotIn('"built_at": git(', candidate)
        self.assertIn(
            'build_id = f"{source_built_at[:10]}-p{expected_parent}-r{expected_source}-o{expected_omp}"',
            candidate,
        )
        self.assertEqual(promotion.count("--format=%cI"), 1)
        self.assertIn(
            "/usr/bin/env -i GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1",
            promotion,
        )
        self.assertIn(
            'test "$(git_public rev-parse "$source_ref")" = "$EXPECTED_SOURCE"',
            promotion,
        )
        self.assertIn(
            'test "$(git_public cat-file -t "$source_ref")" = commit', promotion
        )
        self.assertIn(
            '/usr/bin/env -i SOURCE_BUILT_AT_RAW="$source_built_at_raw"', promotion
        )
        self.assertIn('test "$source_built_at" = "$EXPECTED_BUILT_AT"', promotion)
        self.assertIn('"${BUILD_ID:0:10}" != "${source_built_at:0:10}"', promotion)
        canonical = (
            datetime.fromisoformat("2026-08-21T20:00:00-07:00")
            .astimezone(timezone.utc)
            .strftime("%Y-%m-%dT%H:%M:%SZ")
        )
        self.assertEqual(canonical, "2026-08-22T03:00:00Z")
        self.assertEqual(canonical[:10], "2026-08-22")

    def test_smarty_preview_job_graph_is_least_privilege_and_fresh(self):
        jobs = workflow_jobs(workflow_source())
        self.assertEqual(
            list(jobs),
            [
                "preflight",
                "candidate-checks",
                "build",
                "attest-platform",
                "assemble-spdx",
                "attest-spdx",
                "assemble-pair",
                "attest-pair",
                "verify-release",
                "render-channel",
                "reuse-release",
                "verify-reuse",
                "publish-release",
                "verify-publication",
                "advance-channel",
                "promote-canonical",
            ],
        )
        for name, body in jobs.items():
            for dependency in re.findall(r"needs\.([a-z][a-z0-9-]*)\.", body):
                self.assertIn(dependency, jobs, f"{name} needs {dependency}")
        for name in (
            "candidate-checks",
            "build",
            "assemble-spdx",
            "assemble-pair",
            "verify-release",
            "render-channel",
            "verify-reuse",
        ):
            body = jobs[name]
            self.assertIn("    permissions: {}\n", body, name)
            self.assertNotIn("secrets.", body, name)
            self.assertNotIn("id-token", body, name)
            self.assertNotIn("environment:", body, name)
            self.assertNotIn("contents: write", body, name)
        for name in ("attest-platform", "attest-spdx", "attest-pair"):
            body = jobs[name]
            self.assertIn("id-token: write", body, name)
            self.assertIn("attestations: write", body, name)
            self.assertNotIn("secrets.", body, name)
            self.assertNotIn("contents: write", body, name)
        for name in (
            "preflight",
            "reuse-release",
            "publish-release",
            "verify-publication",
            "advance-channel",
        ):
            body = jobs[name]
            self.assertIn("environment: smarty-release", body, name)
            self.assertIn("secrets.SMARTY_RELEASE_TOKEN", body, name)
            self.assertNotIn("id-token", body, name)
        self.assertIn("contents: write", jobs["publish-release"])
        for name in (
            "preflight",
            "reuse-release",
            "verify-publication",
            "advance-channel",
        ):
            self.assertNotIn("contents: write", jobs[name], name)
        for name, key in (("build", "target"), ("attest-platform", "platform")):
            self.assertEqual(
                re.findall(r"^          - (\w+):", jobs[name], re.MULTILINE),
                [key] * 5,
                name,
            )
            self.assertNotIn("|-", jobs[name].split("    steps:", 1)[0], name)
        self.assertIn("subject-path: subjects/*", jobs["attest-platform"])
        self.assertIn(
            "staged attestation subjects are not the exact platform payloads",
            jobs["attest-platform"],
        )
        for name in (
            "attest-platform",
            "attest-spdx",
            "attest-pair",
            "reuse-release",
            "publish-release",
            "verify-publication",
            "advance-channel",
        ):
            body = jobs[name]
            for command in (
                "scripts/preview.py",
                "from scripts.preview",
                "herdr-source",
                "omp-source",
                "cargo ",
                "bazel",
                "bun ",
                "just ",
            ):
                self.assertNotIn(command, body, f"{name}: {command}")

    def test_smarty_preview_artifacts_are_bound_to_producer_attempts(self):
        source = workflow_source()
        jobs = workflow_jobs(source)
        actions = re.findall(
            r"^        uses: actions/(upload|download)-artifact@[^\n]+\n"
            r"        with:\n"
            r"          name: ([^\n]+)$",
            source,
            re.MULTILINE,
        )
        expected_count = source.count("uses: actions/upload-artifact@") + source.count(
            "uses: actions/download-artifact@"
        )
        self.assertEqual(len(actions), expected_count)
        uploads = [name for kind, name in actions if kind == "upload"]
        downloads = [name for kind, name in actions if kind == "download"]
        self.assertGreater(len(uploads), 0)
        self.assertGreater(len(downloads), 0)
        suffix = "-${{ github.run_attempt }}"
        for name in uploads:
            self.assertTrue(name.endswith(suffix), f"artifact is not attempt-scoped: {name}")
            self.assertEqual(name.count("${{ github.run_attempt }}"), 1, name)
        for name in downloads:
            self.assertNotIn("github.run_attempt", name, name)

        producer_attempt = "1"
        consumer_attempt = "2"
        self.assertNotEqual(producer_attempt, consumer_attempt)
        self.assertEqual("smarty-release-plan-" + producer_attempt, "smarty-release-plan-1")
        self.assertNotEqual(
            "smarty-release-plan-" + producer_attempt,
            "smarty-release-plan-" + consumer_attempt,
        )
        producer_links = (
            ("candidate-checks", "smarty-release-plan", "needs.preflight.outputs.artifact_attempt"),
            ("build", "smarty-candidate-sources", "needs.preflight.outputs.artifact_attempt"),
            ("attest-spdx", "smarty-spdx-candidate", "needs.assemble-spdx.outputs.artifact_attempt"),
            ("assemble-pair", "smarty-spdx-attested", "needs.attest-spdx.outputs.artifact_attempt"),
            ("attest-pair", "smarty-pair-candidate", "needs.assemble-pair.outputs.artifact_attempt"),
            ("verify-release", "smarty-release-ready", "needs.attest-pair.outputs.artifact_attempt"),
            ("verify-reuse", "smarty-release-reuse", "needs.reuse-release.outputs.artifact_attempt"),
            ("advance-channel", "smarty-channel-candidate", "needs.render-channel.outputs.artifact_attempt"),
        )
        for job, artifact, producer_output in producer_links:
            self.assertIn(
                "name: " + artifact + "-${{ " + producer_output + " }}",
                jobs[job],
                f"{job} must consume {artifact} from its producer",
            )

        for platform in (
            "linux-x86_64",
            "linux-aarch64",
            "macos-x86_64",
            "macos-aarch64",
            "windows-x86_64",
        ):
            suffix = platform.replace("-", "_") + "_attempt"
            candidate = "candidate_" + suffix
            attested = "attested_" + suffix
            self.assertIn(
                candidate + ": ${{ steps.candidate-artifact.outputs." + candidate + " }}",
                jobs["build"],
            )
            self.assertIn(
                "artifact_attempt: ${{ needs.build.outputs." + candidate + " }}",
                jobs["attest-platform"],
            )
            self.assertIn(
                attested + ": ${{ steps.attested-artifact.outputs." + attested + " }}",
                jobs["attest-platform"],
            )
            self.assertIn(
                "name: attested-" + platform + "-${{ needs.attest-platform.outputs." + attested + " }}",
                jobs["assemble-spdx"],
            )
        self.assertIn(
            "name: candidate-${{ matrix.platform }}-${{ matrix.artifact_attempt }}",
            jobs["attest-platform"],
        )
        self.assertNotIn("overwrite:", source)
        self.assertNotIn("merge-multiple:", source)

    def test_verify_publication_downloads_the_exact_sealed_producer_artifact(self):
        jobs = workflow_jobs(workflow_source())
        fetch = workflow_steps(jobs["verify-publication"])["Fetch immutable paired release"]
        self.assertIn(
            "RELEASE_READY_ATTEMPT: ${{ needs.publish-release.result == 'success' && needs.publish-release.outputs.release_ready_attempt || needs.verify-reuse.outputs.artifact_attempt }}",
            jobs["verify-publication"],
        )
        self.assertIn(
            '--name "smarty-release-ready-$RELEASE_READY_ATTEMPT"',
            fetch,
        )
        self.assertNotIn("--name smarty-release-ready --", fetch)
        self.assertIn(
            "release_ready_attempt: ${{ needs.attest-pair.outputs.artifact_attempt }}",
            jobs["verify-release"],
        )
        self.assertIn(
            "release_ready_attempt: ${{ needs.verify-release.outputs.release_ready_attempt }}",
            jobs["publish-release"],
        )

    def test_smarty_preview_pair_candidate_artifact_has_one_producer_and_consumer(self):
        jobs = workflow_jobs(workflow_source())
        produced_name = "name: smarty-pair-candidate-${{ github.run_attempt }}"
        consumed_name = "name: smarty-pair-candidate-${{ needs.assemble-pair.outputs.artifact_attempt }}"
        self.assertEqual(
            [name for name, body in jobs.items() if produced_name in body],
            ["assemble-pair"],
        )
        self.assertEqual(
            [name for name, body in jobs.items() if consumed_name in body],
            ["attest-pair"],
        )
        self.assertIn("uses: actions/upload-artifact@", jobs["assemble-pair"])
        self.assertIn("uses: actions/download-artifact@", jobs["attest-pair"])
        self.assertNotIn("paired-preview", workflow_source())

    def test_smarty_preview_fixed_native_rebuild_is_fresh_and_precedes_upload(self):
        build = workflow_jobs(workflow_source())["build"]
        steps = workflow_steps(build)
        fixed_name = "Independently rebuild fixed native targets and toolchain report"
        fixed = steps[fixed_name]
        names = list(steps)
        self.assertLess(
            names.index("Package Windows candidate payload"), names.index(fixed_name)
        )
        for upload in (
            "Upload paired candidate payload",
            "Upload Windows candidate payload",
        ):
            self.assertLess(names.index(fixed_name), names.index(upload))
        self.assertIn("source-archives/omp-source.tar", fixed)
        self.assertIn("fixed-omp-source", fixed)
        self.assertIn("--ignore_all_rc_files build --lockfile_mode=error", fixed)
        self.assertIn('expected = set(os.environ["OMP_NATIVE_ASSETS"].split())', fixed)
        self.assertIn("path.read_bytes() != (candidate / name).read_bytes()", fixed)
        self.assertIn("    permissions: {}\n", build)

    def test_phase_a_restores_bridge_installer_execute_mode(self):
        advance = workflow_steps(workflow_jobs(workflow_source())["advance-channel"])[
            "Advance Smarty channel with compare-and-swap"
        ]
        copied = "/bin/cp channel/preview.json channel/install.sh channel-repo/"
        chmod = "/bin/chmod 0755 channel-repo/install.sh"
        staged = "/usr/bin/git -C channel-repo add preview.json install.sh"
        self.assertLess(advance.index(copied), advance.index(chmod))
        self.assertLess(advance.index(chmod), advance.index(staged))

    def test_phase_b_idempotence_uses_one_fetched_ref_snapshot(self):
        promote = workflow_steps(workflow_jobs(promotion_workflow_source())["promote"])[
            "Promote bridge to canonical channel with compare-and-swap"
        ]
        self.assertNotIn("ls-remote", promote)
        fetch = "git_auth -C channel-repo fetch --no-tags"
        fetched_channel = 'fetched_channel="$(fetched_ref channel)"'
        fetched_primary = (
            'fetched_primary_authorization="$(fetched_ref primary-authorization)"'
        )
        fetched_promotion = (
            'fetched_authorization="$(fetched_ref promotion-authorization)"'
        )
        idempotent = 'if [ "$fetched_channel" = "$candidate_commit" ]'
        for fetched in (fetched_channel, fetched_primary, fetched_promotion):
            self.assertLess(promote.index(fetch), promote.index(fetched))
        self.assertLess(promote.index(fetched_channel), promote.index(idempotent))
        self.assertIn('"$fetched_channel" != "$BRIDGE_COMMIT"', promote)
        self.assertIn('"$fetched_primary_authorization" != "$BRIDGE_COMMIT"', promote)
        self.assertIn(
            '"$fetched_authorization" != "$EXPECTED_AUTHORIZATION_COMMIT"',
            promote,
        )

    def test_bootstrap_promotion_workflow_is_reviewed_verified_and_atomic(self):
        source = promotion_workflow_source()
        jobs = workflow_jobs(source)
        caller = workflow_jobs(workflow_source())["promote-canonical"]
        self.assertEqual(list(jobs), ["promote"])
        self.assertIn("  workflow_call:\n", source)
        self.assertNotIn("workflow_dispatch", source)
        self.assertIn("    environment: smarty-preview-promotion\n", jobs["promote"])
        self.assertIn("uses: ./.github/workflows/smarty-preview-promote.yml", caller)
        self.assertIn("needs.advance-channel.outputs.bridge_commit", caller)
        self.assertIn('os.environ["EVENT_NAME"] != "push"', source)
        self.assertIn('os.environ["EVENT_REF"] != f"refs/tags/{tag}"', source)
        self.assertIn('os.environ["EVENT_SHA"] != match.group("source")', source)
        self.assertNotIn("pull_request", source)
        self.assertIn("refs/heads/smarty-preview-promotion-authorization", source)
        self.assertIn("refs/heads/smarty-preview-authorization", source)
        self.assertIn('"action": "promote-windows-bootstrap-to-canonical"', source)
        self.assertIn(
            '"workflow": ".github/workflows/smarty-preview-promote.yml"', source
        )
        self.assertIn(
            'expected_authorization_commit="$(create_promotion_lease)"', source
        )
        self.assertIn(
            'test "$authorization_commit" = "$expected_authorization_commit"', source
        )
        self.assertIn(
            "promotion release is not the exact 37-file immutable release", source
        )
        self.assertIn("/usr/bin/gh attestation verify", source)
        self.assertIn("pi_natives.linux-x64-modern.node", source)
        self.assertLess(
            source.index("Cryptographically verify immutable release evidence"),
            source.index("Render exact canonical channel from authenticated verifier"),
        )
        self.assertIn("trusted-preview.py promote-bootstrap-manifest", source)
        self.assertIn(
            "cmp -s candidate/preview.json bridge/canonical-preview.json", source
        )
        self.assertIn("push --atomic", source)
        self.assertIn('"--force-with-lease=$channel_ref:$BRIDGE_COMMIT"', source)
        self.assertIn(
            '"--force-with-lease=$primary_authorization_ref:$BRIDGE_COMMIT"',
            source,
        )
        self.assertIn(
            '"--force-with-lease=$promotion_authorization_ref:$EXPECTED_AUTHORIZATION_COMMIT"',
            source,
        )
        self.assertIn("already promoted to canonical", source)

    def test_phase_b_pair_bridge_scalar_mutations_fail_closed(self):
        source = promotion_workflow_source()
        start = source.index("          def validate_pair_bridge_sources(")
        end = source.index('\n          if (\n              pair.get("pair_id")', start)
        function_source = "\n".join(
            line[10:] for line in source[start:end].splitlines()
        )
        namespace = {}
        exec(function_source, namespace)
        validate = namespace["validate_pair_bridge_sources"]
        parent = "1" * 40
        herdr = "2" * 40
        omp = "3" * 40
        build_id = f"2026-08-22-p{parent}-r{herdr}-o{omp}"
        canonical = {
            "base_version": "0.8.0",
            "protocol": 21,
            "omp": {
                "tree": "4" * 40,
                "version": "18.2.0",
                "build_id": "omp-build-3",
            },
        }
        pair = {
            "sources": {
                "parent": {"commit": parent},
                "herdr": {
                    "commit": herdr,
                    "build_id": build_id,
                    "version": canonical["base_version"],
                    "protocol": canonical["protocol"],
                },
                "omp": {
                    "commit": omp,
                    **canonical["omp"],
                },
            }
        }
        validate(pair, canonical, parent, herdr, omp, build_id)
        mutations = (
            lambda value: value.update(base_version="9.9.9"),
            lambda value: value.update(protocol=22),
            lambda value: value["omp"].update(tree="5" * 40),
            lambda value: value["omp"].update(version="18.2.1"),
            lambda value: value["omp"].update(build_id="omp-build-other"),
        )
        for mutate in mutations:
            tampered = json.loads(json.dumps(canonical))
            mutate(tampered)
            with self.assertRaisesRegex(
                SystemExit,
                "promotion pair sources do not match the exact bridge scalars",
            ):
                validate(pair, tampered, parent, herdr, omp, build_id)

    def test_smarty_preview_embedded_python_gates_compile(self):
        for label, source, minimum in (
            ("smarty-preview", workflow_source(), 30),
            ("smarty-preview-promote", promotion_workflow_source(), 4),
        ):
            lines = source.splitlines()
            blocks = []
            index = 0
            while index < len(lines):
                if lines[index].rstrip().endswith("<<'PY'"):
                    body = []
                    index += 1
                    while lines[index].strip() != "PY":
                        body.append(lines[index][10:])
                        index += 1
                    blocks.append("\n".join(body))
                index += 1
            self.assertGreaterEqual(len(blocks), minimum, label)
            for block in blocks:
                compile(block, f"<{label}>", "exec")

    def test_smarty_preview_pins_exact_trusted_verifier_bytes(self):
        source = workflow_source()
        match = re.search(
            r"^  TRUSTED_RELEASE_VERIFIER_SHA256: ([0-9a-f]{64})$",
            source,
            re.MULTILINE,
        )
        self.assertIsNotNone(match)
        expected = hashlib.sha256(Path(preview.__file__).read_bytes()).hexdigest()
        self.assertNotEqual(match.group(1), "0" * 64)
        self.assertEqual(match.group(1), expected)
        archive = workflow_steps(workflow_jobs(source)["preflight"])[
            "Archive validated source inputs"
        ]
        self.assertIn("sha256sum scripts/preview.py", archive)
        self.assertIn('= "$TRUSTED_RELEASE_VERIFIER_SHA256"', archive)
        self.assertIn(
            "cp scripts/preview.py handoff/trusted-release-verifier.py", archive
        )

    def test_smarty_preview_semantic_verification_uses_fixed_handoff_verifier(self):
        source = workflow_source()
        jobs = workflow_jobs(source)
        spdx = workflow_steps(jobs["attest-spdx"])[
            "Verify exact SPDX attestation inputs"
        ]
        pair = workflow_steps(jobs["attest-pair"])[
            "Verify exact pair attestation inputs"
        ]
        verified = workflow_steps(jobs["verify-release"])[
            "Verify complete paired release with candidate metadata code"
        ]
        reused = workflow_steps(jobs["verify-reuse"])[
            "Verify reused release bytes and exact 37-file identity"
        ]
        publishing = workflow_steps(jobs["publish-release"])[
            "Verify sealed release bytes before publication"
        ]
        published = workflow_steps(jobs["verify-publication"])[
            "Verify immutable paired release bytes"
        ]

        self.assertIn("candidate/handoff/trusted-release-verifier.py spdx", spdx)
        self.assertIn('cmp -s "$RUNNER_TEMP/trusted-spdx.json"', spdx)
        self.assertLess(
            source.index('cmp -s "$RUNNER_TEMP/trusted-spdx.json"'),
            source.index("- name: Attest payloads and deterministic SPDX"),
        )
        self.assertIn(
            "candidate/handoff/trusted-release-verifier.py pair-manifest", pair
        )
        self.assertIn('cmp -s "$RUNNER_TEMP/trusted-pair.json"', pair)
        self.assertLess(
            source.index('cmp -s "$RUNNER_TEMP/trusted-pair.json"'),
            source.index("- name: Attest exact pair manifest"),
        )
        for body in (verified, publishing, published):
            self.assertIn("trusted-release-verifier.py verify-pair", body)
            self.assertIn("--omp-rules-rust-toolchains", body)
        self.assertIn("--cargo-metadata-dir", verified)
        self.assertIn("--dependency-metadata", publishing)
        self.assertIn("--dependency-metadata", published)
        self.assertIn("trusted-release-verifier.py verify-attested-pair", reused)
        self.assertIn("trusted-release-verifier.py verify-attested-pair", published)
        self.assertIn('"verification"', pair)
        self.assertIn("EXPECTED_VERIFIER_SHA256", spdx)
        self.assertIn("EXPECTED_VERIFIER_SHA256", pair)

    def test_smarty_preview_never_interpolates_expressions_into_shell(self):
        inside = False
        for line in workflow_source().splitlines():
            if re.fullmatch(r" {8}run: \|", line):
                inside = True
                continue
            single = re.fullmatch(r" {8}run: (?!\|)(.*)", line)
            if single:
                inside = False
                self.assertNotIn("${{", single.group(1))
                continue
            if inside:
                if line.strip() and not line.startswith(" " * 10):
                    inside = False
                else:
                    self.assertNotIn("${{", line)

    def test_smarty_preview_release_gates_use_the_exact_37_file_set(self):
        jobs = workflow_jobs(workflow_source())
        self.assertEqual(len(preview.FULL_RELEASE_ASSET_NAMES), 37)
        self.assertEqual(len(set(preview.FULL_RELEASE_ASSET_NAMES)), 37)
        for name, message in (
            (
                "attest-pair",
                "sealed release does not match the exact 37-name allow-list",
            ),
            ("verify-reuse", "reuse allow-list is not the exact 37-file release set"),
            (
                "publish-release",
                "publication allow-list is not the exact 37-file release set",
            ),
            (
                "verify-publication",
                "publication allow-list is not the exact 37-file release set",
            ),
            (
                "render-channel",
                "render sealed file set is not the exact 37-file release set",
            ),
            (
                "reuse-release",
                "reused release does not match the exact 37-name allow-list",
            ),
        ):
            self.assertIn(message, jobs[name], name)
        for name in ("verify-reuse", "publish-release", "verify-publication"):
            self.assertIn("if len(expected) != 37:", jobs[name], name)
        for name in ("publish-release", "verify-publication"):
            self.assertIn("!= 37", jobs[name], name)

    def test_smarty_preview_publication_and_channel_tail_is_complete(self):
        source = workflow_source()
        jobs = workflow_jobs(source)
        self.assertEqual(
            list(workflow_steps(jobs["publish-release"])),
            [
                "Download exact sealed release",
                "Verify publication prerequisites",
                "Verify sealed release bytes before publication",
                "Create or reconcile draft-first paired release",
                "Fetch reconciled draft release",
                "Verify reconciled draft bytes",
                "Verify reconciled draft attestations",
                "Publish verified draft",
            ],
        )
        self.assertEqual(
            list(workflow_steps(jobs["verify-publication"])),
            [
                "Fetch immutable paired release",
                "Verify immutable paired release bytes",
                "Verify immutable paired release attestations",
                "Record TUF promotion input for external signing",
                "Upload verified publication records",
                "Upload TUF promotion input",
            ],
        )
        self.assertEqual(
            list(workflow_steps(jobs["advance-channel"])),
            [
                "Download immutable release plan",
                "Download exact rendered channel candidate",
                "Download verified publication records",
                "Verify rendered Smarty channel candidate",
                "Advance Smarty channel with compare-and-swap",
            ],
        )
        self.assertEqual(
            list(workflow_steps(jobs["render-channel"]))[3:],
            [
                "Generate legacy channel inputs",
                "Snapshot Smarty channel for rendering",
                "Render Smarty channel candidate",
                "Verify rendered channel transition with candidate code",
                "Seal exact rendered channel candidate",
                "Upload exact rendered channel candidate",
            ],
        )
        self.assertEqual(
            list(workflow_steps(jobs["verify-reuse"])),
            [
                "Download immutable release plan",
                "Download exact immutable release reuse",
                "Verify reused release bytes and exact 37-file identity",
                "Seal verified reuse as the exact release handoff",
                "Upload exact sealed release",
            ],
        )
        publish = workflow_steps(jobs["publish-release"])
        create = publish["Create or reconcile draft-first paired release"]
        self.assertIn("--draft \\\n", create)
        self.assertIn("--prerelease\n", create)
        self.assertIn('release.get("draft") is True', create)
        self.assertIn('release.get("immutable") is False', create)
        self.assertIn('release.get("draft") is False', create)
        self.assertIn('release.get("immutable") is True', create)
        self.assertIn("publication-already-complete", create)
        self.assertIn("--clobber", create)
        self.assertLess(source.index("--draft \\"), source.index("--draft=false"))
        published = publish["Publish verified draft"]
        self.assertIn("--draft=false", published)
        self.assertIn('release.get("immutable") is not True', published)
        reconciled = publish["Verify reconciled draft bytes"]
        self.assertIn("not in ((True, False), (False, True))", reconciled)
        self.assertIn("if [ ! -f publication-already-complete ]", published)
        self.assertIn(
            "needs.publish-release.result == 'success' || "
            "needs.verify-reuse.result == 'success'",
            jobs["verify-publication"],
        )
        self.assertIn(
            "needs.verify-release.result == 'success' || "
            "needs.verify-reuse.result == 'success'",
            jobs["render-channel"],
        )
        self.assertIn("smarty-release-ready", jobs["verify-reuse"])
        self.assertIn("smarty-release-ready", jobs["attest-pair"])

    def test_smarty_preview_channel_update_is_authenticated_compare_and_swap(self):
        steps = workflow_steps(workflow_jobs(workflow_source())["advance-channel"])
        verify = steps["Verify rendered Smarty channel candidate"]
        advance = steps["Advance Smarty channel with compare-and-swap"]
        self.assertIn(
            "keep = max(retain - 1, ordered.index(previous_current) + 1)", verify
        )
        self.assertIn(
            "channel history is not the deterministic retained prefix", verify
        )
        self.assertIn("channel candidate changed retained build", verify)
        self.assertIn(
            "authenticated channel snapshot is not the observed channel commit", verify
        )
        self.assertIn(
            "rendered channel asset does not match the published release", verify
        )
        self.assertIn(
            "rendered channel installer is not the authenticated candidate installer",
            verify,
        )
        self.assertIn(
            "consumed channel candidate differs from the authenticated snapshot", verify
        )
        self.assertIn("Smarty channel moved from", advance)
        self.assertIn(
            "already advanced to the deterministic bootstrap candidate", advance
        )
        self.assertIn('GIT_AUTHOR_DATE="$BUILT_AT"', advance)
        self.assertIn('GIT_COMMITTER_DATE="$BUILT_AT"', advance)
        self.assertIn("CHANNEL_KEYS = {", verify)
        self.assertIn("exact_keys(manifest, CHANNEL_KEYS", verify)
        self.assertIn('candidate["base_version"] != expected_base_version', verify)
        self.assertIn('candidate["protocol"] != expected_protocol', verify)
        self.assertIn('manifest.get("notes"), str', verify)
        self.assertIn('asset.get("format") != "zip"', verify)
        self.assertIn('observed_channel="$(observe_ref "$channel_ref")"', advance)
        self.assertIn(
            'observed_authorization="$(observe_ref "$authorization_ref")"', advance
        )
        self.assertIn(
            'observed_promotion_authorization="$(observe_ref "$promotion_authorization_ref")"',
            advance,
        )
        self.assertIn(
            '[ "$observed_channel" = "$candidate" ] \\\n'
            '            && [ "$observed_authorization" = "$candidate" ] \\\n'
            '            && [ "$observed_promotion_authorization" = "$promotion_lease" ]',
            advance,
        )
        self.assertIn(
            'if [ "$observed_authorization" != "$EXPECTED_AUTHORIZATION_REF" ]',
            advance,
        )
        self.assertIn("Smarty authorization moved from", advance)
        self.assertIn(".smarty-preview-promotion-authorization.json", advance)
        self.assertIn("$promotion_lease:$promotion_authorization_ref", advance)

    def test_smarty_preview_tuf_promotion_input_is_deterministic_and_unsigned(self):
        steps = workflow_steps(workflow_jobs(workflow_source())["verify-publication"])
        promotion = steps["Record TUF promotion input for external signing"]
        self.assertLess(
            list(steps).index("Verify immutable paired release attestations"),
            list(steps).index("Record TUF promotion input for external signing"),
        )
        self.assertIn('"schema": "smarty.tuf-promotion-input.v1"', promotion)
        self.assertIn('"canary": "canary/smarty-pair.json"', promotion)
        self.assertIn('"stable": "stable/smarty-pair.json"', promotion)
        self.assertIn('"target_name": "smarty-pair.json"', promotion)
        self.assertIn(
            '"gates": {"keys": "external", "performed": False, "signing": "external"}',
            promotion,
        )
        self.assertIn("json.dumps(promotion, indent=2, sort_keys=True)", promotion)
        self.assertIn(
            "TUF promotion input requires an immutable published release", promotion
        )
        self.assertIn(
            "verified publication record does not match the TUF target bytes", promotion
        )
        for forbidden in (
            "--signers",
            "prepare-root",
            "approve-root",
            "smarty-tuf",
            "PRIVATE KEY",
            "keyid",
            "gh release",
            "git push",
        ):
            self.assertNotIn(forbidden, promotion, forbidden)
        self.assertEqual(preview.PAIR_MANIFEST_ASSET_NAME, "smarty-pair.json")
        self.assertEqual(preview.PAIR_MANIFEST_SCHEMA, "smarty.paired-release.v1")
        self.assertEqual(preview.PAIR_ID_DOMAIN, b"smarty.paired-release.v1\x00")

    def test_smarty_preview_reconstructs_sbom_and_binds_channel_pair_fields(self):
        workflow = workflow_source()
        jobs = workflow_jobs(workflow)
        channel = workflow_steps(jobs["render-channel"])[
            "Generate legacy channel inputs"
        ]

        self.assertEqual(
            workflow.count("- name: Checkout exact OMP source for metadata"), 1
        )
        self.assertEqual(
            workflow.count("mod --lockfile_mode=error graph --output=json"), 2
        )
        self.assertEqual(workflow.count("--herdr-root herdr-source"), 3)
        self.assertEqual(workflow.count("--omp-root omp-source"), 3)
        self.assertIn(
            "--omp-bazel-graph verification/omp-bazel-graph.json",
            jobs["verify-release"],
        )
        self.assertIn(
            "--omp-bazel-graph metadata/omp-bazel-graph.json", jobs["assemble-spdx"]
        )
        self.assertIn(
            'pair["sources"]["herdr"]["version"] != os.environ["BASE_VERSION"]',
            channel,
        )
        self.assertIn('pair["release"]["built_at"] != os.environ["BUILT_AT"]', channel)
        self.assertIn("Render Smarty channel candidate", workflow)
        self.assertIn("Verify rendered Smarty channel candidate", workflow)


class PairedReleaseMetadataTests(unittest.TestCase):
    parent_commit = "1" * 40
    parent_tree = "2" * 40
    herdr_commit = "3" * 40
    herdr_tree = "4" * 40
    built_at = "2026-08-22T03:00:00Z"
    build_id = preview.paired_build_id(
        built_at, parent_commit, herdr_commit, OMP_SOURCE["commit"]
    )
    tag = f"smarty-preview-{build_id}"

    @staticmethod
    def _digest(path: Path) -> str:
        return hashlib.sha256(path.read_bytes()).hexdigest()

    @staticmethod
    def _bundle(subjects: dict[str, str]) -> str:
        statement = {
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [
                {"name": name, "digest": {"sha256": digest}}
                for name, digest in subjects.items()
            ],
        }
        payload = base64.b64encode(
            json.dumps(statement, sort_keys=True).encode("utf-8")
        ).decode("ascii")
        return (
            json.dumps(
                {
                    "dsseEnvelope": {
                        "payloadType": "application/vnd.in-toto+json",
                        "payload": payload,
                        "signatures": [],
                    }
                },
                sort_keys=True,
            )
            + "\n"
        )

    def _jsonl_bundle(self, subjects: dict[str, str]) -> str:
        return "".join(
            self._bundle({name: digest}) for name, digest in subjects.items()
        )

    def _write_payloads(self, asset_dir: Path, herdr_root: Path) -> None:
        for index, name in enumerate(preview.RELEASE_PAYLOAD_ASSET_NAMES):
            if name == preview.EXPECTED_ASSET_NAMES["windows-x86_64"]:
                continue
            payload = asset_dir / name
            payload.write_bytes(f"payload-{index}-{name}\n".encode("utf-8"))

        metadata = json.loads(
            (herdr_root / preview.CONPTY_METADATA_PATH).read_text(encoding="utf-8")
        )
        marker = {
            "schema_version": 1,
            "package": metadata["package"]["id"],
            "version": metadata["package"]["version"],
            "architecture": "x86_64",
            "files": {
                item["destination"]: item["sha256"]
                for item in metadata["bundles"]["x86_64"]["files"]
            },
        }
        windows = asset_dir / preview.EXPECTED_ASSET_NAMES["windows-x86_64"]
        with zipfile.ZipFile(windows, "w", compression=zipfile.ZIP_STORED) as archive:
            archive.writestr("herdr.exe", b"fixture herdr.exe\n")
            archive.writestr(
                "conpty/herdr-conpty.json",
                json.dumps(marker, indent=2, sort_keys=True) + "\n",
            )
            for item in metadata["bundles"]["x86_64"]["files"]:
                archive.writestr(item["destination"], b"fixture conpty.dll\n")
            for item in metadata["notices"]:
                archive.writestr(
                    item["destination"], (herdr_root / item["source"]).read_bytes()
                )

        for name in preview.RELEASE_PAYLOAD_ASSET_NAMES:
            payload = asset_dir / name
            (asset_dir / f"{name}.sha256").write_text(
                f"{self._digest(payload)} {name}\n", encoding="utf-8"
            )

    def _write_lock_inputs(self, root: Path) -> tuple[Path, Path, Path, Path, Path]:
        herdr_root = root / "herdr"
        omp_root = root / "omp"
        herdr_root.mkdir()
        omp_root.mkdir()
        (herdr_root / "Cargo.toml").write_text(
            """[package]
name = "herdr"
version = "0.8.2"
license = "BSD-3-Clause"

[dependencies]
serde = "1"

[build-dependencies]
serde = "1"

[target.'cfg(target_os = "windows")'.dependencies]
windows-sys = "0.59"

[dev-dependencies]
parking_lot = "0.12"
""",
            encoding="utf-8",
        )
        (herdr_root / "Cargo.lock").write_text(
            """version = 4

[[package]]
name = "herdr"
version = "0.8.2"
dependencies = ["parking_lot", "serde", "windows-sys"]

[[package]]
name = "parking_lot"
version = "0.12.5"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "1111111111111111111111111111111111111111111111111111111111111111"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "2222222222222222222222222222222222222222222222222222222222222222"

[[package]]
name = "windows-sys"
version = "0.59.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "3333333333333333333333333333333333333333333333333333333333333333"
""",
            encoding="utf-8",
        )
        (herdr_root / "rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "1.96.1"\n', encoding="utf-8"
        )

        conpty_bytes = b"fixture conpty.dll\n"
        license_bytes = b"fixture ConPTY license\n"
        notice_bytes = b"fixture ConPTY notice\n"
        license_root = herdr_root / "packaging/windows/licenses"
        license_root.mkdir(parents=True)
        license_path = license_root / "Microsoft.Windows.Console.ConPTY-LICENSE.txt"
        notice_path = license_root / "Microsoft.Windows.Console.ConPTY-NOTICE.md"
        license_path.write_bytes(license_bytes)
        notice_path.write_bytes(notice_bytes)
        conpty_path = herdr_root / preview.CONPTY_METADATA_PATH
        conpty_path.parent.mkdir(parents=True, exist_ok=True)
        conpty_path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "package": {
                        "id": "Microsoft.Windows.Console.ConPTY",
                        "version": "1.24.260710001",
                        "release_tag": "v1.24.11911.0",
                        "url": "https://example.invalid/Microsoft.Windows.Console.ConPTY.nupkg",
                        "sha256": "3" * 64,
                        "license": "MIT",
                    },
                    "bundles": {
                        "x86_64": {
                            "target_triple": "x86_64-pc-windows-msvc",
                            "files": [
                                {
                                    "destination": "conpty/conpty.dll",
                                    "sha256": hashlib.sha256(conpty_bytes).hexdigest(),
                                }
                            ],
                        }
                    },
                    "notices": [
                        {
                            "source": license_path.relative_to(herdr_root).as_posix(),
                            "destination": "THIRD-PARTY-NOTICES/ConPTY-LICENSE.txt",
                            "sha256": hashlib.sha256(license_bytes).hexdigest(),
                        },
                        {
                            "source": notice_path.relative_to(herdr_root).as_posix(),
                            "destination": "THIRD-PARTY-NOTICES/ConPTY-NOTICE.md",
                            "sha256": hashlib.sha256(notice_bytes).hexdigest(),
                        },
                    ],
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )

        coding_agent = omp_root / "packages/coding-agent"
        coding_agent.mkdir(parents=True)
        (coding_agent / "package.json").write_text(
            json.dumps(
                {
                    "name": "@oh-my-pi/pi-coding-agent",
                    "version": OMP_SOURCE["version"],
                    "license": "ISC",
                }
            )
            + "\n",
            encoding="utf-8",
        )
        integrity = "sha512-" + base64.b64encode(b"i" * 64).decode("ascii")
        (omp_root / "bun.lock").write_text(
            json.dumps(
                {
                    "lockfileVersion": 1,
                    "workspaces": {
                        "packages/coding-agent": {
                            "name": "@oh-my-pi/pi-coding-agent",
                            "version": OMP_SOURCE["version"],
                            "dependencies": {
                                "@fixture/root": "1.0.0",
                                "left-pad": "1.0.0",
                            },
                            "optionalDependencies": {"optional-pkg": "2.0.0"},
                        }
                    },
                    "packages": {
                        "@oh-my-pi/pi-coding-agent": [
                            "@oh-my-pi/pi-coding-agent@workspace:packages/coding-agent"
                        ],
                        "@fixture/root": [
                            "@fixture/root@1.0.0",
                            "",
                            {
                                "dependencies": {
                                    "@fixture/child": "1.0.0",
                                    "onnxruntime-node": "1.0.0",
                                    "platform-pkg": "1.0.0",
                                },
                                "peerDependencies": {
                                    "peer-optional": "1.0.0",
                                    "peer-required": "1.0.0",
                                },
                                "optionalPeers": ["peer-optional"],
                            },
                            integrity,
                        ],
                        "@fixture/root/@fixture/child": [
                            "@fixture/child@1.0.0",
                            "",
                            {"dependencies": {"nested": "1.0.0"}},
                            integrity,
                        ],
                        "@fixture/root/@fixture/child/nested": [
                            "nested@1.0.0",
                            "",
                            {},
                            integrity,
                        ],
                        "@fixture/root/platform-pkg": [
                            "platform-pkg@1.0.0",
                            "",
                            {"os": ["linux"], "cpu": "x64"},
                            integrity,
                        ],
                        "@fixture/root/onnxruntime-node": [
                            "onnxruntime-node@1.0.0",
                            "",
                            {
                                "dependencies": {"adm-zip": "1.0.0"},
                                "os": ["linux", "win32", "darwin"],
                                "cpu": ["x64", "arm64"],
                            },
                            integrity,
                        ],
                        "@fixture/root/onnxruntime-node/adm-zip": [
                            "adm-zip@1.0.0",
                            "",
                            {"os": ["linux", "darwin"], "cpu": ["x86_64", "aarch64"]},
                            integrity,
                        ],
                        "@fixture/root/peer-optional": [
                            "peer-optional@1.0.0",
                            "",
                            {},
                            integrity,
                        ],
                        "@fixture/root/peer-required": [
                            "peer-required@1.0.0",
                            "",
                            {},
                            integrity,
                        ],
                        "left-pad": [
                            "left-pad@1.0.0",
                            "",
                            {"dependencies": {"transitive": "3.0.0"}},
                            integrity,
                        ],
                        "optional-pkg": ["optional-pkg@2.0.0", "", {}, integrity],
                        "transitive": ["transitive@3.0.0", "", {}, integrity],
                    },
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        native = omp_root / "crates/pi-natives"
        native.mkdir(parents=True)
        (native / "Cargo.toml").write_text(
            f"""[package]
name = "pi-natives"
version = "{OMP_SOURCE["version"]}"
license = "MIT"

[dependencies]
anyhow = "1"
serde = "1"

[build-dependencies]
anyhow = "1"

[dev-dependencies]
fancy-regex = "0.14"
""",
            encoding="utf-8",
        )
        (omp_root / "Cargo.lock").write_text(
            f"""version = 4

[[package]]
name = "pi-natives"
version = "{OMP_SOURCE["version"]}"
dependencies = ["anyhow", "fancy-regex", "serde"]

[[package]]
name = "anyhow"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "4444444444444444444444444444444444444444444444444444444444444444"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "2222222222222222222222222222222222222222222222222222222222222222"

[[package]]
name = "fancy-regex"
version = "0.14.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "5555555555555555555555555555555555555555555555555555555555555555"
""",
            encoding="utf-8",
        )
        (omp_root / "MODULE.bazel.lock").write_text("{}\n", encoding="utf-8")
        (omp_root / ".bazelversion").write_text("9.2.0\n", encoding="utf-8")
        (omp_root / "rust-toolchain.toml").write_text(
            '[toolchain]\nchannel = "nightly-2026-07-28"\n', encoding="utf-8"
        )
        (omp_root / "MODULE.bazel").write_text(
            'bazel_dep(name = "rules_rust", version = "0.71.3")\n',
            encoding="utf-8",
        )
        rules_rust_toolchains = root / "omp-rules-rust-toolchains.json"
        rules_rust_toolchains.write_text(
            json.dumps(
                {
                    "schema": 1,
                    "toolchains": {
                        platform: {
                            "toolchain_type": "@@rules_rust+//rust:toolchain_type",
                            "resolved": [
                                "@@rules_rust++rust+fixture_"
                                f"{platform}//:rust_toolchain"
                            ],
                        }
                        for platform in preview.OMP_ASSET_TARGETS
                    },
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        bazel_graph = root / "omp-bazel-graph.json"
        bazel_graph.write_text(
            json.dumps(
                {
                    "key": "<root>",
                    "name": "oh-my-pi",
                    "version": "",
                    "apparentName": "oh-my-pi",
                    "root": True,
                    "dependencies": [
                        {
                            "key": "rules_rust@0.71.3",
                            "name": "rules_rust",
                            "version": "0.71.3",
                            "apparentName": "rules_rust",
                            "dependencies": [
                                {
                                    "key": "platforms@1.1.0",
                                    "name": "platforms",
                                    "version": "1.1.0",
                                    "apparentName": "platforms",
                                    "dependencies": [],
                                    "indirectDependencies": [],
                                    "cycles": [],
                                }
                            ],
                            "indirectDependencies": [],
                            "cycles": [],
                        }
                    ],
                    "indirectDependencies": [],
                    "cycles": [],
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        metadata_dir = root / "cargo-metadata"
        metadata_dir.mkdir()
        registry = "registry+https://github.com/rust-lang/crates.io-index"

        def cargo_package(
            package_id: str,
            name: str,
            version: str,
            *,
            source: str | None = None,
            manifest_path: Path | None = None,
            license: str = "MIT",
        ) -> dict:
            package = {
                "id": package_id,
                "license": license,
                "name": name,
                "version": version,
            }
            if source is None:
                package["manifest_path"] = str(manifest_path)
            else:
                package["source"] = source
            return package

        def write_metadata(
            component: str,
            platform: str,
            workspace: Path,
            root_id: str,
            packages: list[dict],
            nodes: list[dict],
        ) -> None:
            path = metadata_dir / preview.CARGO_METADATA_FILENAMES[component][platform]
            path.write_text(
                json.dumps(
                    {
                        "packages": packages,
                        "resolve": {"nodes": nodes, "root": root_id},
                        "version": 1,
                        "workspace_root": str(workspace),
                    },
                    sort_keys=True,
                ),
                encoding="utf-8",
            )

        herdr_id = "path+file:///fixture/herdr#herdr@0.8.2"
        serde_id = "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0"
        parking_lot_id = (
            "registry+https://github.com/rust-lang/crates.io-index#parking_lot@0.12.5"
        )
        windows_sys_id = (
            "registry+https://github.com/rust-lang/crates.io-index#windows-sys@0.59.0"
        )
        for platform in preview.CARGO_METADATA_TARGETS["herdr"]:
            herdr_packages = [
                cargo_package(
                    herdr_id,
                    "herdr",
                    "0.8.2",
                    manifest_path=herdr_root / "Cargo.toml",
                    license="BSD-3-Clause",
                ),
                cargo_package(serde_id, "serde", "1.0.0", source=registry),
                cargo_package(parking_lot_id, "parking_lot", "0.12.5", source=registry),
            ]
            herdr_deps = [
                {
                    "dep_kinds": [
                        {"kind": None, "target": None},
                        {"kind": "build", "target": None},
                    ],
                    "name": "serde",
                    "pkg": serde_id,
                },
                {
                    "dep_kinds": [{"kind": "dev", "target": None}],
                    "name": "parking_lot",
                    "pkg": parking_lot_id,
                },
            ]
            if platform == "windows-x86_64":
                herdr_packages.append(
                    cargo_package(
                        windows_sys_id, "windows-sys", "0.59.0", source=registry
                    )
                )
                herdr_deps.append(
                    {
                        "dep_kinds": [
                            {
                                "kind": None,
                                "target": 'cfg(target_os = "windows")',
                            }
                        ],
                        "name": "windows-sys",
                        "pkg": windows_sys_id,
                    }
                )
            write_metadata(
                "herdr",
                platform,
                herdr_root,
                herdr_id,
                herdr_packages,
                [{"deps": herdr_deps, "id": herdr_id}]
                + [{"deps": [], "id": package["id"]} for package in herdr_packages[1:]],
            )

        native_id = "path+file:///fixture/omp/crates/pi-natives#pi-natives@17.3.7"
        anyhow_id = "registry+https://github.com/rust-lang/crates.io-index#anyhow@1.0.0"
        fancy_regex_id = (
            "registry+https://github.com/rust-lang/crates.io-index#fancy-regex@0.14.0"
        )
        for platform in preview.CARGO_METADATA_TARGETS["omp"]:
            omp_packages = [
                cargo_package(
                    native_id,
                    "pi-natives",
                    OMP_SOURCE["version"],
                    manifest_path=native / "Cargo.toml",
                ),
                cargo_package(anyhow_id, "anyhow", "1.0.0", source=registry),
                cargo_package(serde_id, "serde", "1.0.0", source=registry),
                cargo_package(fancy_regex_id, "fancy-regex", "0.14.0", source=registry),
            ]
            write_metadata(
                "omp",
                platform,
                omp_root,
                native_id,
                omp_packages,
                [
                    {
                        "deps": [
                            {
                                "dep_kinds": [
                                    {"kind": None, "target": None},
                                    {"kind": "build", "target": None},
                                ],
                                "name": "anyhow",
                                "pkg": anyhow_id,
                            },
                            {
                                "dep_kinds": [{"kind": None, "target": None}],
                                "name": "serde",
                                "pkg": serde_id,
                            },
                            {
                                "dep_kinds": [{"kind": "dev", "target": None}],
                                "name": "fancy-regex",
                                "pkg": fancy_regex_id,
                            },
                        ],
                        "id": native_id,
                    },
                    *[
                        {"deps": [], "id": package["id"]}
                        for package in omp_packages[1:]
                    ],
                ],
            )
        return herdr_root, omp_root, bazel_graph, metadata_dir, rules_rust_toolchains

    def _write_semantic_verification_inputs(self, root: Path) -> tuple[Path, Path]:
        verifier = root / "trusted-release-verifier.py"
        verifier.write_bytes(Path(preview.__file__).read_bytes())
        archives = root / "source-archives"
        archives.mkdir()
        for name in preview.SOURCE_ARCHIVE_NAMES:
            (archives / name).write_bytes(f"fixture {name}\n".encode("ascii"))
        return verifier, archives

    def _refresh_pair_provenance(self, asset_dir: Path) -> None:
        pair = asset_dir / preview.PAIR_MANIFEST_ASSET_NAME
        (asset_dir / preview.PAIR_PROVENANCE_ASSET_NAME).write_text(
            self._bundle({preview.PAIR_MANIFEST_ASSET_NAME: self._digest(pair)}),
            encoding="utf-8",
        )

    def _rewrite_pair(self, asset_dir: Path, mutate) -> None:
        pair = asset_dir / preview.PAIR_MANIFEST_ASSET_NAME
        document = json.loads(pair.read_text(encoding="utf-8"))
        mutate(document)
        pair.write_text(
            json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        self._refresh_pair_provenance(asset_dir)

    def _refresh_spdx_evidence(self, asset_dir: Path) -> None:
        spdx = asset_dir / preview.SPDX_ASSET_NAME

        def update(document):
            document["evidence"][preview.SPDX_ASSET_NAME].update(
                length=spdx.stat().st_size,
                sha256=self._digest(spdx),
            )
            document["verification"]["spdx"].update(
                length=spdx.stat().st_size,
                sha256=self._digest(spdx),
            )

        self._rewrite_pair(asset_dir, update)

    def _build_release(self, root: Path, *, built_at: str | None = None) -> Path:
        built_at = built_at or self.built_at
        root.mkdir(parents=True, exist_ok=True)
        asset_dir = root / "assets"
        asset_dir.mkdir()
        (
            herdr_root,
            omp_root,
            bazel_graph,
            cargo_metadata_dir,
            rules_rust_toolchains,
        ) = self._write_lock_inputs(root)
        trusted_verifier, source_archive_dir = self._write_semantic_verification_inputs(
            root
        )
        self._write_payloads(asset_dir, herdr_root)
        payload_digests = {
            name: self._digest(asset_dir / name)
            for name in preview.RELEASE_PAYLOAD_ASSET_NAMES
        }
        (asset_dir / preview.SPDX_ASSET_NAME).write_text(
            preview.build_spdx(
                asset_dir=asset_dir,
                built_at=built_at,
                parent_commit=self.parent_commit,
                herdr_commit=self.herdr_commit,
                omp_commit=OMP_SOURCE["commit"],
                herdr_version=f"0.8.2-preview.{self.build_id}",
                omp_version=OMP_SOURCE["version"],
                herdr_root=herdr_root,
                omp_root=omp_root,
                omp_bazel_graph=bazel_graph,
                cargo_metadata_dir=cargo_metadata_dir,
            ),
            encoding="utf-8",
        )
        for target, name in preview.PLATFORM_PROVENANCE_ASSET_NAMES.items():
            subjects = {
                payload: payload_digests[payload]
                for payload in preview.PLATFORM_PAYLOAD_ASSET_NAMES[target]
            }
            content = (
                self._jsonl_bundle(subjects)
                if target == "linux-x86_64"
                else self._bundle(subjects)
            )
            (asset_dir / name).write_text(content, encoding="utf-8")
        (asset_dir / preview.SPDX_PROVENANCE_ASSET_NAME).write_text(
            self._bundle(
                {
                    **payload_digests,
                    preview.SPDX_ASSET_NAME: self._digest(
                        asset_dir / preview.SPDX_ASSET_NAME
                    ),
                }
            ),
            encoding="utf-8",
        )
        (asset_dir / preview.PAIR_MANIFEST_ASSET_NAME).write_text(
            preview.build_pair_manifest(
                asset_dir=asset_dir,
                repo="Smarty-Pants-Inc/herdr",
                tag=self.tag,
                build_id=self.build_id,
                built_at=built_at,
                parent_repo="Smarty-Pants-Inc/smarty-dev",
                parent_commit=self.parent_commit,
                parent_tree=self.parent_tree,
                herdr_commit=self.herdr_commit,
                herdr_tree=self.herdr_tree,
                base_version="0.8.2",
                protocol=25,
                omp_source=OMP_SOURCE,
                herdr_root=herdr_root,
                omp_root=omp_root,
                omp_rules_rust_toolchains=rules_rust_toolchains,
                trusted_verifier=trusted_verifier,
                source_archive_dir=source_archive_dir,
                omp_bazel_graph=bazel_graph,
                cargo_metadata_dir=cargo_metadata_dir,
                bun_version="1.3.14",
                zig_version="0.15.2",
            ),
            encoding="utf-8",
        )
        self._refresh_pair_provenance(asset_dir)
        return asset_dir

    def _verify(
        self,
        asset_dir: Path,
        *,
        rules_rust_toolchains: Path | None = None,
    ) -> dict:
        root = asset_dir.parent
        return preview.verify_pair(
            asset_dir=asset_dir,
            expected_parent=self.parent_commit,
            expected_source=self.herdr_commit,
            expected_omp=OMP_SOURCE["commit"],
            expected_parent_tree=self.parent_tree,
            expected_source_tree=self.herdr_tree,
            expected_omp_tree=OMP_SOURCE["tree"],
            expected_tag=self.tag,
            expected_build_id=self.build_id,
            herdr_root=root / "herdr",
            omp_root=root / "omp",
            omp_bazel_graph=root / "omp-bazel-graph.json",
            trusted_verifier=root / "trusted-release-verifier.py",
            source_archive_dir=root / "source-archives",
            omp_rules_rust_toolchains=rules_rust_toolchains
            or root / "omp-rules-rust-toolchains.json",
            cargo_metadata_dir=root / "cargo-metadata",
        )

    def test_release_allow_lists_are_exact(self):
        self.assertEqual(
            set(preview.LEGACY_PAYLOAD_ASSET_NAMES),
            {
                "herdr-linux-x86_64",
                "herdr-linux-aarch64",
                "herdr-macos-x86_64",
                "herdr-macos-aarch64",
                "herdr-windows-x86_64.zip",
                "omp-linux-x86_64",
                "omp-linux-aarch64",
                "omp-macos-x86_64",
                "omp-macos-aarch64",
            },
        )
        self.assertEqual(len(preview.LEGACY_ASSET_NAMES), 18)
        self.assertEqual(
            preview.METADATA_ASSET_NAMES,
            (
                "smarty-pair.json",
                "smarty-pair.spdx.json",
                "smarty-provenance-linux-x86_64.sigstore.json",
                "smarty-provenance-linux-aarch64.sigstore.json",
                "smarty-provenance-macos-x86_64.sigstore.json",
                "smarty-provenance-macos-aarch64.sigstore.json",
                "smarty-provenance-windows-x86_64.sigstore.json",
                "smarty-pair.provenance.sigstore.json",
                "smarty-pair.spdx.sigstore.json",
            ),
        )
        self.assertEqual(len(preview.FULL_RELEASE_ASSET_NAMES), 37)
        self.assertEqual(
            set(preview.FULL_RELEASE_ASSET_NAMES),
            set(preview.RELEASE_ASSET_NAMES) | set(preview.METADATA_ASSET_NAMES),
        )
        self.assertIsNone(preview.PLATFORM_MATRIX["windows-x86_64"]["payloads"]["omp"])

    def test_windows_zip_requires_exact_unique_regular_members(self):
        with tempfile.TemporaryDirectory() as tmp:
            asset_dir = self._build_release(Path(tmp))
            windows = asset_dir / preview.EXPECTED_ASSET_NAMES["windows-x86_64"]
            original = windows.read_bytes()

            def reject(mutator) -> None:
                windows.write_bytes(original)
                mutator()
                with self.assertRaisesRegex(ValueError, "bundle layout mismatch"):
                    preview._conpty_dependency(
                        asset_dir, asset_dir.parent / "herdr", "SPDXRef-Windows"
                    )

            def append(name: str, data: bytes) -> None:
                with zipfile.ZipFile(windows, "a") as archive:
                    archive.writestr(name, data)

            reject(lambda: append("herdr.exe", b"duplicate"))
            reject(lambda: append("unexpected.txt", b"unexpected"))
            reject(lambda: append("unexpected/", b""))

            def write_symlink() -> None:
                with zipfile.ZipFile(windows) as archive:
                    members = [
                        (info.filename, archive.read(info))
                        for info in archive.infolist()
                    ]
                with zipfile.ZipFile(windows, "w") as archive:
                    for name, data in members:
                        if name == "herdr.exe":
                            info = zipfile.ZipInfo(name)
                            info.create_system = 3
                            info.external_attr = (stat.S_IFLNK | 0o777) << 16
                            archive.writestr(info, b"target")
                        else:
                            archive.writestr(name, data)

            reject(write_symlink)

    def test_bazel_9_2_json_graph_accepts_real_expansion_and_cycles(self):
        def reference(key: str, name: str, version: str) -> dict:
            return {
                "key": key,
                "name": name,
                "version": version,
                "apparentName": name,
                "unexpanded": True,
            }

        def node(key: str, name: str, version: str, dependencies=(), cycles=()) -> dict:
            return {
                "key": key,
                "name": name,
                "version": version,
                "apparentName": name,
                "dependencies": list(dependencies),
                "indirectDependencies": [],
                "cycles": list(cycles),
            }

        abseil_ref = reference("abseil-cpp@20250814.1", "abseil-cpp", "20250814.1")
        graph = node(
            "<root>",
            "oh-my-pi",
            "",
            (
                node(
                    "rules_cc@0.2.17",
                    "rules_cc",
                    "0.2.17",
                    (
                        node(
                            "abseil-cpp@20250814.1",
                            "abseil-cpp",
                            "20250814.1",
                            (
                                node(
                                    "googletest@1.17.0",
                                    "googletest",
                                    "1.17.0",
                                    cycles=(abseil_ref,),
                                ),
                            ),
                        ),
                    ),
                ),
                node(
                    "rules_rust@0.71.3",
                    "rules_rust",
                    "0.71.3",
                    (reference("rules_cc@0.2.17", "rules_cc", "0.2.17"),),
                ),
            ),
        )
        graph["root"] = True

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "graph.json"

            def parse(value):
                path.write_text(json.dumps(value), encoding="utf-8")
                return preview._bazel_dependency_graph(path, "SPDXRef-Package-omp")

            packages, relationships = parse(graph)
            self.assertEqual(
                {package["name"] for package in packages},
                {"rules_cc", "abseil-cpp", "googletest", "rules_rust"},
            )
            self.assertEqual(len(relationships), 6)

            tampered = json.loads(json.dumps(graph))
            tampered["unexpected"] = True
            with self.assertRaisesRegex(ValueError, "unexpected fields"):
                parse(tampered)

            tampered = json.loads(json.dumps(graph))
            tampered["dependencies"][0]["indirectDependencies"] = "not-a-list"
            with self.assertRaisesRegex(
                ValueError, "indirectDependencies must be a list"
            ):
                parse(tampered)

            tampered = json.loads(json.dumps(graph))
            tampered["indirectDependencies"] = [
                reference("platforms@1.1.0", "platforms", "1.1.0")
            ]
            with self.assertRaisesRegex(ValueError, "full graph required"):
                parse(tampered)

            tampered = json.loads(json.dumps(graph))
            tampered["dependencies"][0]["dependencies"][0]["dependencies"][0][
                "cycles"
            ] = [reference("rules_rust@0.71.3", "rules_rust", "0.71.3")]
            with self.assertRaisesRegex(ValueError, "active ancestor"):
                parse(tampered)

            tampered = json.loads(json.dumps(graph))
            tampered["dependencies"][1]["dependencies"] = [
                reference("orphan@1.0.0", "orphan", "1.0.0")
            ]
            with self.assertRaisesRegex(ValueError, "unexpanded-only modules"):
                parse(tampered)

    def test_paired_build_id_is_injective_for_same_r_with_different_p(self):
        same_tuple = preview.paired_build_id(
            self.built_at,
            self.parent_commit,
            self.herdr_commit,
            OMP_SOURCE["commit"],
        )
        other_parent = "5" * 40
        other_tuple = preview.paired_build_id(
            self.built_at, other_parent, self.herdr_commit, OMP_SOURCE["commit"]
        )

        self.assertEqual(same_tuple, self.build_id)
        self.assertNotEqual(same_tuple, other_tuple)
        self.assertEqual(
            same_tuple,
            f"2026-08-22-p{self.parent_commit}-r{self.herdr_commit}-o{OMP_SOURCE['commit']}",
        )
        self.assertEqual(
            preview._validate_paired_build_id(
                other_tuple,
                other_parent,
                self.herdr_commit,
                OMP_SOURCE["commit"],
                self.built_at,
            ),
            other_tuple,
        )
        with self.assertRaisesRegex(ValueError, "P/R/O identity mismatch"):
            preview._validate_paired_build_id(
                other_tuple,
                self.parent_commit,
                self.herdr_commit,
                OMP_SOURCE["commit"],
                self.built_at,
            )
        wrong_day = other_tuple.replace("2026-08-22-", "2026-08-21-", 1)
        with self.assertRaisesRegex(ValueError, "date must match canonical built_at"):
            preview._validate_paired_build_id(
                wrong_day,
                other_parent,
                self.herdr_commit,
                OMP_SOURCE["commit"],
                self.built_at,
            )

    def test_offset_timestamp_is_canonical_across_pair_spdx_and_channel(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source_timestamp = "2026-08-21T23:30:00-04:00"
            canonical_timestamp = "2026-08-22T03:30:00Z"
            asset_dir = self._build_release(root, built_at=source_timestamp)
            pair = json.loads(
                (asset_dir / preview.PAIR_MANIFEST_ASSET_NAME).read_text(
                    encoding="utf-8"
                )
            )
            spdx = json.loads(
                (asset_dir / preview.SPDX_ASSET_NAME).read_text(encoding="utf-8")
            )
            herdr_shas = {
                target: self._digest(asset_dir / name)
                for target, name in preview.EXPECTED_ASSET_NAMES.items()
            }
            omp_shas = {
                target: self._digest(asset_dir / name)
                for target, name in preview.OMP_EXPECTED_ASSET_NAMES.items()
            }
            channel = json.loads(
                preview.build_manifest(
                    output=root / "preview.json",
                    repo="Smarty-Pants-Inc/herdr",
                    tag=self.tag,
                    build_id=self.build_id,
                    commit=self.herdr_commit,
                    built_at=source_timestamp,
                    base_version="0.8.2",
                    protocol=25,
                    notes="offset timestamp",
                    shas=herdr_shas,
                    retain=30,
                    omp_source=OMP_SOURCE,
                    omp_shas=omp_shas,
                )
            )

            self.assertEqual(
                {
                    pair["release"]["built_at"],
                    spdx["creationInfo"]["created"],
                    channel["built_at"],
                    channel["builds"][self.build_id]["built_at"],
                },
                {canonical_timestamp},
            )
            self.assertEqual(
                preview._timestamp(source_timestamp, "source timestamp"),
                canonical_timestamp,
            )
            self.assertEqual(
                preview._timestamp("2026-08-22T03:30:00.000000+00:00", "zero fraction"),
                canonical_timestamp,
            )
            with self.assertRaisesRegex(ValueError, "whole-second precision"):
                preview._timestamp("2026-08-22T03:30:00.123Z", "fraction")
            self.assertTrue(self.build_id.startswith("2026-08-22-"))
            self.assertEqual(
                preview.paired_build_id(
                    source_timestamp,
                    self.parent_commit,
                    self.herdr_commit,
                    OMP_SOURCE["commit"],
                ),
                self.build_id,
            )
            self.assertEqual(
                preview.validate_channel_transition(
                    None,
                    channel,
                    expected_parent=self.parent_commit,
                    expected_source=self.herdr_commit,
                    expected_omp=OMP_SOURCE["commit"],
                    expected_omp_tree=OMP_SOURCE["tree"],
                    expected_omp_version=OMP_SOURCE["version"],
                    expected_omp_build_id=OMP_SOURCE["build_id"],
                    expected_tag=self.tag,
                    expected_build_id=self.build_id,
                    expected_built_at=source_timestamp,
                    expected_base_version="0.8.2",
                    expected_protocol=25,
                    expected_herdr_shas=herdr_shas,
                    expected_omp_shas=omp_shas,
                ),
                channel,
            )
            noncanonical_channel = json.loads(json.dumps(channel))
            noncanonical_channel["built_at"] = source_timestamp
            noncanonical_channel["builds"][self.build_id]["built_at"] = source_timestamp
            with self.assertRaisesRegex(ValueError, "not canonical UTC Z"):
                preview._validate_channel_manifest(noncanonical_channel)
            self.assertEqual(
                self._verify(asset_dir)["release"]["built_at"], canonical_timestamp
            )

    def test_spdx_is_deterministic_and_records_exact_release_dependencies(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            asset_dir = root / "assets"
            asset_dir.mkdir()
            herdr_root, omp_root, bazel_graph, cargo_metadata_dir, _ = (
                self._write_lock_inputs(root)
            )
            self._write_payloads(asset_dir, herdr_root)
            arguments = {
                "asset_dir": asset_dir,
                "built_at": self.built_at,
                "parent_commit": self.parent_commit,
                "herdr_commit": self.herdr_commit,
                "omp_commit": OMP_SOURCE["commit"],
                "herdr_version": f"0.8.2-preview.{self.build_id}",
                "omp_version": OMP_SOURCE["version"],
                "herdr_root": herdr_root,
                "omp_root": omp_root,
                "omp_bazel_graph": bazel_graph,
                "cargo_metadata_dir": cargo_metadata_dir,
            }
            arguments["built_at"] = "2026-08-21T23:00:00-04:00"
            first = preview.build_spdx(**arguments)
            self.assertEqual(first, preview.build_spdx(**arguments))
            document = json.loads(first)
            self.assertEqual(document["spdxVersion"], "SPDX-2.3")
            self.assertEqual(
                document["creationInfo"],
                {
                    "created": "2026-08-22T03:00:00Z",
                    "creators": ["Tool: smarty-preview-1.0"],
                },
            )
            self.assertEqual(
                {entry["fileName"] for entry in document["files"]},
                set(preview.RELEASE_PAYLOAD_ASSET_NAMES),
            )
            for file in document["files"]:
                digests = {
                    item["algorithm"]: item["checksumValue"]
                    for item in file["checksums"]
                }
                self.assertEqual(set(digests), {"SHA1", "SHA256"})
                payload = asset_dir / file["fileName"]
                self.assertEqual(
                    digests["SHA1"], hashlib.sha1(payload.read_bytes()).hexdigest()
                )
                self.assertEqual(
                    digests["SHA256"], hashlib.sha256(payload.read_bytes()).hexdigest()
                )
            self.assertEqual(
                document["documentNamespace"],
                preview._spdx_namespace(
                    self.parent_commit, self.herdr_commit, OMP_SOURCE["commit"]
                ),
            )

            names = {package["name"] for package in document["packages"]}
            self.assertTrue(
                {
                    "herdr",
                    "serde",
                    "windows-sys",
                    "omp",
                    "pi-natives",
                    "anyhow",
                    "@fixture/root",
                    "@fixture/child",
                    "nested",
                    "onnxruntime-node",
                    "adm-zip",
                    "platform-pkg",
                    "peer-optional",
                    "peer-required",
                    "transitive",
                    "optional-pkg",
                    "rules_rust",
                    "platforms",
                    "Microsoft.Windows.Console.ConPTY",
                }.issubset(names)
            )
            self.assertNotIn("parking_lot", names)
            self.assertNotIn("fancy-regex", names)
            packages_by_name = {
                name: [
                    package
                    for package in document["packages"]
                    if package["name"] == name
                ]
                for name in names
            }
            self.assertEqual(len(packages_by_name["serde"]), 1)
            self.assertEqual(
                packages_by_name["herdr"][0]["licenseDeclared"], "BSD-3-Clause"
            )
            self.assertEqual(packages_by_name["omp"][0]["licenseDeclared"], "ISC")

            labels = {
                package["SPDXID"]: package["name"] for package in document["packages"]
            }
            labels.update(
                {entry["SPDXID"]: entry["fileName"] for entry in document["files"]}
            )
            relationship_records = {
                (
                    labels.get(item["spdxElementId"], item["spdxElementId"]),
                    item["relationshipType"],
                    labels.get(item["relatedSpdxElement"], item["relatedSpdxElement"]),
                ): item
                for item in document["relationships"]
            }
            relationships = set(relationship_records)
            self.assertIn(("herdr", "DEPENDS_ON", "serde"), relationships)
            self.assertIn(("omp", "DEPENDS_ON", "pi-natives"), relationships)
            self.assertIn(("pi-natives", "DEPENDS_ON", "anyhow"), relationships)
            self.assertIn(("pi-natives", "DEPENDS_ON", "serde"), relationships)
            self.assertNotIn(("herdr", "DEPENDS_ON", "parking_lot"), relationships)
            self.assertNotIn(("pi-natives", "DEPENDS_ON", "fancy-regex"), relationships)
            self.assertIn(("serde", "BUILD_DEPENDENCY_OF", "herdr"), relationships)
            self.assertIn(("herdr", "DEPENDS_ON", "windows-sys"), relationships)
            self.assertIn(
                "platform=windows-x86_64;filter=x86_64-pc-windows-msvc",
                relationship_records[("herdr", "DEPENDS_ON", "windows-sys")]["comment"],
            )
            self.assertIn(("omp", "DEPENDS_ON", "left-pad"), relationships)
            self.assertIn(("left-pad", "DEPENDS_ON", "transitive"), relationships)
            self.assertIn(
                ("optional-pkg", "OPTIONAL_DEPENDENCY_OF", "omp"), relationships
            )
            self.assertIn(
                ("anyhow", "BUILD_DEPENDENCY_OF", "pi-natives"), relationships
            )
            self.assertIn(("omp", "DEPENDS_ON", "@fixture/root"), relationships)
            self.assertIn(
                ("@fixture/root", "DEPENDS_ON", "@fixture/child"), relationships
            )
            self.assertIn(("@fixture/child", "DEPENDS_ON", "nested"), relationships)
            self.assertIn(("onnxruntime-node", "DEPENDS_ON", "adm-zip"), relationships)
            self.assertNotIn(
                ("adm-zip", "OPTIONAL_DEPENDENCY_OF", "onnxruntime-node"),
                relationships,
            )
            self.assertIn(
                ("platform-pkg", "OPTIONAL_DEPENDENCY_OF", "@fixture/root"),
                relationships,
            )
            self.assertNotIn(
                ("@fixture/root", "DEPENDS_ON", "platform-pkg"), relationships
            )
            self.assertIn(
                ("peer-optional", "OPTIONAL_DEPENDENCY_OF", "@fixture/root"),
                relationships,
            )
            self.assertNotIn(
                ("peer-optional", "PREREQUISITE_FOR", "@fixture/root"),
                relationships,
            )
            self.assertIn(
                ("peer-required", "PREREQUISITE_FOR", "@fixture/root"),
                relationships,
            )
            self.assertIn(
                "bun.dependency.os=linux;bun.dependency.cpu=x64",
                relationship_records[
                    ("platform-pkg", "OPTIONAL_DEPENDENCY_OF", "@fixture/root")
                ]["comment"],
            )
            self.assertIn(
                "bun.optional_peer=true",
                relationship_records[
                    ("peer-optional", "OPTIONAL_DEPENDENCY_OF", "@fixture/root")
                ]["comment"],
            )
            self.assertIn(
                "os=linux;cpu=x64",
                packages_by_name["platform-pkg"][0]["sourceInfo"],
            )
            self.assertIn(
                "optionalPeers=peer-optional",
                packages_by_name["@fixture/root"][0]["sourceInfo"],
            )
            self.assertIn(("rules_rust", "BUILD_DEPENDENCY_OF", "omp"), relationships)
            self.assertIn(
                (
                    "Microsoft.Windows.Console.ConPTY",
                    "RUNTIME_DEPENDENCY_OF",
                    "herdr-windows-x86_64.zip",
                ),
                relationships,
            )

    def test_pair_manifest_records_schema_metadata_and_verifies(self):
        with tempfile.TemporaryDirectory() as tmp:
            asset_dir = self._build_release(Path(tmp))
            document = json.loads(
                (asset_dir / preview.PAIR_MANIFEST_ASSET_NAME).read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(document["schema"], preview.PAIR_MANIFEST_SCHEMA)
            expected_pair_id = hashlib.sha256(
                b"smarty.paired-release.v1\0"
                + self.parent_commit.encode("ascii")
                + b"\0"
                + self.herdr_commit.encode("ascii")
                + b"\0"
                + OMP_SOURCE["commit"].encode("ascii")
            ).hexdigest()
            self.assertEqual(document["pair_id"], expected_pair_id)
            self.assertEqual(
                document["sources"]["parent"]["repository"],
                preview.PARENT_REPOSITORY,
            )
            self.assertEqual(
                document["sources"]["parent"]["commit"], self.parent_commit
            )
            self.assertEqual(document["sources"]["herdr"]["tree"], self.herdr_tree)
            self.assertEqual(document["sources"]["omp"]["tree"], OMP_SOURCE["tree"])
            self.assertEqual(document["toolchains"]["herdr"]["zig"], "0.15.2")
            self.assertEqual(document["toolchains"]["omp"]["bun"], "1.3.14")
            self.assertEqual(
                document["toolchains"]["herdr"]["declarations"]["rust"]["path"],
                "rust-toolchain.toml",
            )
            self.assertEqual(
                document["toolchains"]["omp"]["declarations"]["bazel"]["path"],
                ".bazelversion",
            )
            rules_rust = document["toolchains"]["omp"]["rules_rust"]
            self.assertEqual(rules_rust["version"], "0.71.3")
            self.assertEqual(rules_rust["declaration"]["path"], "MODULE.bazel")
            self.assertEqual(
                set(rules_rust),
                {"version", "declaration", "toolchain_type", "resolved"},
            )
            self.assertEqual(
                rules_rust["toolchain_type"], "@@rules_rust+//rust:toolchain_type"
            )
            self.assertEqual(
                set(rules_rust["resolved"]), set(preview.OMP_ASSET_TARGETS)
            )
            self.assertEqual(
                set(document["lock_inputs"]),
                {
                    "herdr/Cargo.lock",
                    "omp/bun.lock",
                    "omp/MODULE.bazel.lock",
                    "omp/Cargo.lock",
                    "omp/.bazelversion",
                    "omp/rust-toolchain.toml",
                },
            )
            self.assertEqual(
                set(document["artifacts"]), set(preview.RELEASE_ASSET_NAMES)
            )
            self.assertEqual(
                set(document["evidence"]), set(preview.EVIDENCE_ASSET_NAMES)
            )
            self.assertNotIn(preview.PAIR_PROVENANCE_ASSET_NAME, document["evidence"])
            verification = document["verification"]
            self.assertEqual(
                verification["schema"], preview.SEMANTIC_VERIFICATION_SCHEMA
            )
            self.assertEqual(
                verification["verifier"]["sha256"],
                self._digest(asset_dir.parent / "trusted-release-verifier.py"),
            )
            self.assertEqual(
                set(verification["inputs"]["cargo_metadata"]),
                {
                    name
                    for component in preview.CARGO_METADATA_FILENAMES.values()
                    for name in component.values()
                },
            )
            self.assertEqual(document["platforms"]["linux-x86_64"]["abi"], "glibc")
            self.assertEqual(
                document["platforms"]["linux-x86_64"]["runner"], "ubuntu-22.04"
            )
            self.assertEqual(document["artifacts"]["herdr-linux-x86_64"]["abi"], "musl")
            self.assertEqual(document["artifacts"]["omp-linux-x86_64"]["abi"], "glibc")
            self.assertIsNone(
                document["platforms"]["windows-x86_64"]["payloads"]["omp"]
            )
            self.assertEqual(
                preview.decode_attestation_subjects(
                    asset_dir / preview.SPDX_PROVENANCE_ASSET_NAME
                ),
                {
                    **{
                        name: self._digest(asset_dir / name)
                        for name in preview.RELEASE_PAYLOAD_ASSET_NAMES
                    },
                    preview.SPDX_ASSET_NAME: self._digest(
                        asset_dir / preview.SPDX_ASSET_NAME
                    ),
                },
            )
            verified = self._verify(asset_dir)
            self.assertEqual(verified["schema"], preview.PAIR_MANIFEST_SCHEMA)
            self.assertEqual(verified["pair_id"], expected_pair_id)

    def test_legacy_manifest_stays_schema_one_without_pair_metadata(self):
        with tempfile.TemporaryDirectory() as tmp:
            content = preview.build_manifest(
                output=Path(tmp) / "preview.json",
                repo="herdrdev/herdr",
                tag="preview-test",
                build_id="legacy-build",
                commit="legacy-commit",
                built_at=self.built_at,
                base_version="0.8.2",
                protocol=25,
                notes="legacy notes\n",
                shas=HERDR_SHAS,
                retain=30,
            )
            manifest = json.loads(content)
            self.assertEqual(manifest["schema_version"], 1)
            self.assertEqual(
                set(manifest),
                {
                    "schema_version",
                    "channel",
                    "base_version",
                    "build_id",
                    "commit",
                    "built_at",
                    "protocol",
                    "notes",
                    "assets",
                    "builds",
                },
            )
            self.assertNotIn("evidence", manifest)
            self.assertNotIn(preview.PAIR_MANIFEST_ASSET_NAME, content)

    def test_verify_pair_rejects_spdx_sha1_and_rules_rust_mismatches(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.subTest("SPDX SHA-1 mismatch"):
                asset_dir = self._build_release(root / "sha1")
                spdx = asset_dir / preview.SPDX_ASSET_NAME
                document = json.loads(spdx.read_text(encoding="utf-8"))
                document["files"][0]["checksums"][0]["checksumValue"] = "0" * 40
                spdx.write_text(
                    json.dumps(document, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
                self._refresh_spdx_evidence(asset_dir)
                with self.assertRaisesRegex(ValueError, "SPDX file SHA-1 mismatch"):
                    self._verify(asset_dir)

            with self.subTest("SPDX missing SHA-1"):
                asset_dir = self._build_release(root / "missing-sha1")
                spdx = asset_dir / preview.SPDX_ASSET_NAME
                document = json.loads(spdx.read_text(encoding="utf-8"))
                document["files"][0]["checksums"] = [
                    checksum
                    for checksum in document["files"][0]["checksums"]
                    if checksum["algorithm"] != "SHA1"
                ]
                spdx.write_text(
                    json.dumps(document, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
                self._refresh_spdx_evidence(asset_dir)
                with self.assertRaisesRegex(ValueError, "must have SHA-1 and SHA-256"):
                    self._verify(asset_dir)

            with self.subTest("rules_rust declaration"):
                asset_dir = self._build_release(root / "rules-rust-declaration")
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["toolchains"]["omp"]["rules_rust"].update(
                        version="0.0.0"
                    ),
                )
                with self.assertRaisesRegex(
                    ValueError, "pair manifest OMP rules_rust declaration mismatch"
                ):
                    self._verify(asset_dir)

            with self.subTest("rules_rust runtime selection claim"):
                asset_dir = self._build_release(root / "rules-rust-selection")

                def change_selection(document):
                    document["toolchains"]["omp"]["rules_rust"]["resolved"][
                        "linux-x86_64"
                    ] = ["@@rules_rust++rust+tampered//:rust_toolchain"]

                self._rewrite_pair(asset_dir, change_selection)
                with self.assertRaisesRegex(
                    ValueError, "semantic claims do not match trusted reconstruction"
                ):
                    self._verify(asset_dir)

    def test_verify_pair_rejects_signed_semantic_claim_tampering(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)

            with self.subTest("toolchain"):
                asset_dir = self._build_release(root / "toolchain")
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["toolchains"]["herdr"].update(
                        zig="0.0.0"
                    ),
                )
                with self.assertRaisesRegex(ValueError, "Zig toolchain mismatch"):
                    self._verify(asset_dir)

            with self.subTest("lock input"):
                asset_dir = self._build_release(root / "lock-input")
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["lock_inputs"]["herdr/Cargo.lock"].update(
                        sha256="0" * 64
                    ),
                )
                with self.assertRaisesRegex(ValueError, "lock inputs do not match"):
                    self._verify(asset_dir)

            with self.subTest("platform"):
                asset_dir = self._build_release(root / "platform")
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["platforms"]["linux-x86_64"].update(
                        runner="unexpected-runner"
                    ),
                )
                with self.assertRaisesRegex(
                    ValueError, "platform linux-x86_64 identity mismatch"
                ):
                    self._verify(asset_dir)

            with self.subTest("verification input"):
                asset_dir = self._build_release(root / "verification-input")
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["verification"]["inputs"][
                        "native_graph"
                    ].update(sha256="0" * 64),
                )
                with self.assertRaisesRegex(
                    ValueError, "semantic claims do not match trusted reconstruction"
                ):
                    self._verify(asset_dir)

            with self.subTest("verifier"):
                asset_dir = self._build_release(root / "verifier")
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["verification"]["verifier"].update(
                        sha256="0" * 64
                    ),
                )
                with self.assertRaisesRegex(
                    ValueError, "trusted verifier record mismatch"
                ):
                    self._verify(asset_dir)

    def test_verify_pair_rejects_consistently_rewritten_rules_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            asset_dir = self._build_release(Path(tmp))
            root = asset_dir.parent
            carried = root / "omp-rules-rust-toolchains.json"
            trusted = root / "trusted-omp-rules-rust-toolchains.json"
            trusted.write_bytes(carried.read_bytes())
            carried.write_text(
                carried.read_text(encoding="utf-8") + "\n", encoding="utf-8"
            )
            record = {
                "length": carried.stat().st_size,
                "sha256": self._digest(carried),
            }
            self._rewrite_pair(
                asset_dir,
                lambda document: document["verification"]["inputs"].update(
                    rules_rust_toolchains=record
                ),
            )
            with self.assertRaisesRegex(
                ValueError, "semantic claims do not match trusted reconstruction"
            ):
                self._verify(asset_dir, rules_rust_toolchains=trusted)

    def test_verify_pair_rejects_identity_digest_length_subject_and_name_mismatches(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with self.subTest("digest"):
                asset_dir = self._build_release(root / "digest")
                payload = asset_dir / "herdr-linux-x86_64"
                payload.write_bytes(b"x" * payload.stat().st_size)
                with self.assertRaisesRegex(
                    ValueError, "asset herdr-linux-x86_64 digest mismatch"
                ):
                    self._verify(asset_dir)

            with self.subTest("length"):
                asset_dir = self._build_release(root / "length")
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["artifacts"]["herdr-linux-x86_64"].update(
                        length=document["artifacts"]["herdr-linux-x86_64"]["length"] + 1
                    ),
                )
                with self.assertRaisesRegex(
                    ValueError, "asset herdr-linux-x86_64 length mismatch"
                ):
                    self._verify(asset_dir)

            with self.subTest("subjects"):
                asset_dir = self._build_release(root / "subjects")
                bundle = (
                    asset_dir / preview.PLATFORM_PROVENANCE_ASSET_NAMES["linux-x86_64"]
                )
                bundle.write_text(
                    self._bundle(
                        {
                            "herdr-linux-x86_64": self._digest(
                                asset_dir / "herdr-linux-x86_64"
                            )
                        }
                    ),
                    encoding="utf-8",
                )
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["evidence"][bundle.name].update(
                        length=bundle.stat().st_size,
                        sha256=self._digest(bundle),
                    ),
                )
                with self.assertRaisesRegex(
                    ValueError, "platform provenance linux-x86_64 subjects mismatch"
                ):
                    self._verify(asset_dir)
            with self.subTest("SPDX attestation subject"):
                asset_dir = self._build_release(root / "spdx-subject")
                bundle = asset_dir / preview.SPDX_PROVENANCE_ASSET_NAME
                bundle.write_text(
                    self._bundle(
                        {
                            name: self._digest(asset_dir / name)
                            for name in preview.RELEASE_PAYLOAD_ASSET_NAMES[:-1]
                        }
                    ),
                    encoding="utf-8",
                )
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["evidence"][bundle.name].update(
                        length=bundle.stat().st_size,
                        sha256=self._digest(bundle),
                    ),
                )
                with self.assertRaisesRegex(
                    ValueError, "SBOM attestation subjects mismatch"
                ):
                    self._verify(asset_dir)

            with self.subTest("parent identity"):
                asset_dir = self._build_release(root / "identity")

                def change_parent(document):
                    document["sources"]["parent"]["commit"] = "f" * 40
                    document["pair_id"] = preview.pair_id_for_sources(
                        "f" * 40, self.herdr_commit, OMP_SOURCE["commit"]
                    )

                self._rewrite_pair(asset_dir, change_parent)
                with self.assertRaisesRegex(
                    ValueError, "build_id P/R/O identity mismatch"
                ):
                    self._verify(asset_dir)

            with self.subTest("pair id"):
                asset_dir = self._build_release(root / "pair-id")
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document.update(pair_id="f" * 64),
                )
                with self.assertRaisesRegex(
                    ValueError, "pair manifest pair_id mismatch"
                ):
                    self._verify(asset_dir)

            with self.subTest("repository"):
                asset_dir = self._build_release(root / "repository")
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["sources"]["parent"].update(
                        repository="somewhere/else"
                    ),
                )
                with self.assertRaisesRegex(
                    ValueError, "pair manifest parent repository mismatch"
                ):
                    self._verify(asset_dir)

            with self.subTest("uppercase sidecar digest"):
                asset_dir = self._build_release(root / "uppercase-sidecar")
                sidecar = asset_dir / "herdr-linux-x86_64.sha256"
                fields = sidecar.read_text(encoding="utf-8").split()
                sidecar.write_text(
                    f"{fields[0].upper()} {fields[1]}\n", encoding="utf-8"
                )
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["artifacts"][sidecar.name].update(
                        length=sidecar.stat().st_size,
                        sha256=self._digest(sidecar),
                    ),
                )
                with self.assertRaisesRegex(
                    ValueError, "checksum sidecar digest is invalid"
                ):
                    self._verify(asset_dir)

            with self.subTest("uppercase attestation digest"):
                asset_dir = self._build_release(root / "uppercase-attestation")
                bundle = (
                    asset_dir
                    / preview.PLATFORM_PROVENANCE_ASSET_NAMES["windows-x86_64"]
                )
                payload = asset_dir / "herdr-windows-x86_64.zip"
                bundle.write_text(
                    self._bundle({payload.name: self._digest(payload).upper()}),
                    encoding="utf-8",
                )
                self._rewrite_pair(
                    asset_dir,
                    lambda document: document["evidence"][bundle.name].update(
                        length=bundle.stat().st_size,
                        sha256=self._digest(bundle),
                    ),
                )
                with self.assertRaisesRegex(
                    ValueError, "attestation .* subject digest is invalid"
                ):
                    self._verify(asset_dir)

            with self.subTest("extra asset"):
                asset_dir = self._build_release(root / "extra")
                (asset_dir / "unexpected").write_text("unexpected\n", encoding="utf-8")
                with self.assertRaisesRegex(ValueError, "37-name allow-list"):
                    self._verify(asset_dir)

            with self.subTest("SPDX package inventory"):
                asset_dir = self._build_release(root / "spdx-package")
                spdx = asset_dir / preview.SPDX_ASSET_NAME
                document = json.loads(spdx.read_text(encoding="utf-8"))
                document["packages"] = [
                    package
                    for package in document["packages"]
                    if package["name"] != "left-pad"
                ]
                spdx.write_text(
                    json.dumps(document, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
                self._refresh_spdx_evidence(asset_dir)
                with self.assertRaisesRegex(
                    ValueError, "SPDX dependency inventory mismatch"
                ):
                    self._verify(asset_dir)

            with self.subTest("SPDX dependency relationship"):
                asset_dir = self._build_release(root / "spdx-relationship")
                spdx = asset_dir / preview.SPDX_ASSET_NAME
                document = json.loads(spdx.read_text(encoding="utf-8"))
                document["relationships"] = [
                    relationship
                    for relationship in document["relationships"]
                    if relationship["relationshipType"] != "RUNTIME_DEPENDENCY_OF"
                ]
                spdx.write_text(
                    json.dumps(document, indent=2, sort_keys=True) + "\n",
                    encoding="utf-8",
                )
                self._refresh_spdx_evidence(asset_dir)
                with self.assertRaisesRegex(
                    ValueError, "SPDX dependency inventory mismatch"
                ):
                    self._verify(asset_dir)

    def test_verify_pair_cli_uses_the_contract_arguments(self):
        with tempfile.TemporaryDirectory() as tmp:
            asset_dir = self._build_release(Path(tmp))
            result = subprocess.run(
                [
                    sys.executable,
                    str(Path(preview.__file__).resolve()),
                    "verify-pair",
                    "--asset-dir",
                    str(asset_dir),
                    "--expected-parent",
                    self.parent_commit,
                    "--expected-source",
                    self.herdr_commit,
                    "--expected-omp",
                    OMP_SOURCE["commit"],
                    "--expected-parent-tree",
                    self.parent_tree,
                    "--expected-source-tree",
                    self.herdr_tree,
                    "--expected-omp-tree",
                    OMP_SOURCE["tree"],
                    "--expected-tag",
                    self.tag,
                    "--expected-build-id",
                    self.build_id,
                    "--herdr-root",
                    str(asset_dir.parent / "herdr"),
                    "--omp-root",
                    str(asset_dir.parent / "omp"),
                    "--omp-bazel-graph",
                    str(asset_dir.parent / "omp-bazel-graph.json"),
                    "--cargo-metadata-dir",
                    str(asset_dir.parent / "cargo-metadata"),
                    "--omp-rules-rust-toolchains",
                    str(asset_dir.parent / "omp-rules-rust-toolchains.json"),
                    "--trusted-verifier",
                    str(asset_dir.parent / "trusted-release-verifier.py"),
                    "--source-archive-dir",
                    str(asset_dir.parent / "source-archives"),
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)


class WindowsBridgeInstallerStaticTests(unittest.TestCase):
    def test_phase_a_bridge_is_validated_before_asset_or_identity_selection(self):
        source = INSTALLER_PATH.read_text(encoding="utf-8")
        bridge = source[
            source.index("function Resolve-PhaseABridgeManifest") : source.index(
                "function Resolve-CanonicalPreviewManifest"
            )
        ]
        self.assertIn("function Get-PairedBuildId", source)
        self.assertIn("function Get-BridgeReleaseAssetUrl", source)
        self.assertIn("function Test-PhaseABridgeManifestCandidate", source)
        self.assertIn(
            '$topLevelFields = @("assets", "base_version", "bootstrap", "build_id", "builds", "built_at", "canonical_build_id", "channel", "commit", "notes", "omp", "protocol", "schema_version")',
            bridge,
        )
        for key in (
            '"schema", "paired_build_id", "paired_tag", "paired_manifest", "windows_asset_sha256"',
            "Get-PhaseARetainedBuild",
            "Assert-BridgeOmpMatch",
            'Assert-ExactManifestProperties -Value $topAssets -Expected @("windows-x86_64")',
            "Get-BridgeReleaseAssetUrl -Canonical $canonical",
            "AcceptedBuildIds = @($alias, $canonical.BuildId)",
        ):
            self.assertIn(key, bridge)

        main = source[source.index('Write-Step "Fetching Herdr $Channel manifest"') :]
        bridge_detection = main.index(
            "$isPhaseABridge = Test-PhaseABridgeManifestCandidate -Manifest $manifest"
        )
        selection = main.index("$previewSelection = Resolve-PreviewManifest")
        legacy_asset = main.index("$asset = Get-ManifestAsset -Manifest $manifest")
        legacy_identity = main.index("$versionIdentity = Resolve-HerdrVersion")
        expected_build = main.index("$acceptedBuildIds -cnotcontains $ExpectedBuildId")
        self.assertLess(bridge_detection, selection)
        self.assertLess(selection, expected_build)
        self.assertLess(selection, legacy_asset)
        self.assertLess(selection, legacy_identity)

    def test_canonical_phase_b_accepts_alias_only_after_full_asset_binding(self):
        source = INSTALLER_PATH.read_text(encoding="utf-8")
        canonical = source[
            source.index("function Resolve-CanonicalPreviewManifest") : source.index(
                "function Test-PhaseABridgeManifestCandidate"
            )
        ]
        self.assertIn("Get-PhaseARetainedBuild", canonical)
        self.assertIn("Assert-BridgeOmpMatch", canonical)
        self.assertIn("Get-BridgeHerdrAssets", canonical)
        self.assertIn("Assert-BridgeAssetMatch", canonical)
        self.assertIn(
            '$alias = "bootstrap-$(Get-Sha256Hex -Value $canonical.BuildId)"', canonical
        )
        self.assertIn("AcceptedBuildIds = @($canonical.BuildId, $alias)", canonical)

    def test_retained_history_is_validated_before_phase_aliases(self):
        source = INSTALLER_PATH.read_text(encoding="utf-8")
        retained = source[
            source.index("function Get-RetainedPreviewBuild") : source.index(
                "function Resolve-PhaseABridgeManifest"
            )
        ]
        self.assertIn("function Get-RetainedBuildId", source)
        self.assertIn("function Get-RetainedPreviewBuilds", source)
        self.assertIn("foreach ($property in $Builds.PSObject.Properties)", retained)
        self.assertIn(
            "Get-RetainedPreviewBuild -BuildId $property.Name -Build $property.Value",
            retained,
        )
        self.assertIn(
            "Assert-ExactManifestProperties -Value $Build -Expected $fields",
            retained,
        )
        self.assertLess(
            retained.index("Get-RetainedPreviewBuilds -Builds $Builds"),
            retained.index("$retainedBuilds[$Canonical.BuildId]"),
        )

        for start, end in (
            (
                "function Resolve-PhaseABridgeManifest",
                "function Resolve-CanonicalPreviewManifest",
            ),
            (
                "function Resolve-CanonicalPreviewManifest",
                "function Test-PhaseABridgeManifestCandidate",
            ),
        ):
            resolver = source[source.index(start) : source.index(end)]
            self.assertLess(
                resolver.index("Get-PhaseARetainedBuild"),
                resolver.index("AcceptedBuildIds"),
            )


class ConventionalCommitTests(unittest.TestCase):
    def test_valid_subjects_allow_scopes_and_bang(self):
        self.assertTrue(
            conventional_commits.valid_subject("fix(update): handle preview")
        )
        self.assertTrue(conventional_commits.valid_subject("feat!: change config"))
        self.assertFalse(conventional_commits.valid_subject("update preview channel"))

    def test_commit_message_subject_skips_comments(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "COMMIT_EDITMSG"
            path.write_text(
                "\n# Please enter the commit message\n\nfix(update): switch channel\n",
                encoding="utf-8",
            )
            self.assertEqual(
                conventional_commits.commit_message_subject(path),
                "fix(update): switch channel",
            )


if __name__ == "__main__":
    unittest.main()
