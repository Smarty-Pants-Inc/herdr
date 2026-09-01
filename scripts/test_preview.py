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
CI_WORKFLOW_PATH = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"
PR_GATE_PATH = Path(__file__).resolve().parents[1] / ".github/workflows/pr-gate.yml"
INSTALLER_PATH = Path(__file__).resolve().parents[1] / "website/install.ps1"


def workflow_source() -> str:
    return WORKFLOW_PATH.read_text(encoding="utf-8")

def ci_workflow_source() -> str:
    return CI_WORKFLOW_PATH.read_text(encoding="utf-8")

def pr_gate_source() -> str:
    return PR_GATE_PATH.read_text(encoding="utf-8")


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


class CiWorkflowTests(unittest.TestCase):
    def test_title_and_concurrency_guards_exempt_only_mergify_queue_prs(self):
        source = ci_workflow_source()
        self.assertIn("types: [opened, synchronize, reopened, edited]", source)
        title_step = workflow_steps(workflow_jobs(source)["conventional-commits"])[
            "Validate PR title"
        ]
        self.assertIn("github.event.pull_request.user.id != 37929162", title_step)
        self.assertIn(
            "startsWith(github.event.pull_request.head.ref, 'mergify/merge-queue/') == false",
            title_step,
        )
        self.assertIn("github.event.pull_request.user.id == 37929162", source)
        self.assertIn(
            "startsWith(github.event.pull_request.head.ref, 'mergify/merge-queue/') && github.run_id",
            source,
        )
        self.assertIn("cancel-in-progress: ${{ github.event_name != 'pull_request'", source)
        self.assertFalse(
            conventional_commits.valid_subject(
                "merge queue: checking main (abc1234) and #1 together"
            )
        )



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

    def test_smarty_channel_binds_legacy_source_date_before_utc_normalization(self):
        legacy_id = "2026-08-09-eeeeeeeeeeee"
        tag = f"smarty-preview-{legacy_id}"
        legacy = {
            "base_version": "0.8.1",
            "commit": "e" * 40,
            "built_at": "2026-08-09T03:00:00Z",
            "protocol": 12,
            "tag": tag,
            "assets": preview.asset_objects(
                preview.default_asset_urls("Smarty-Pants-Inc/herdr", tag),
                HERDR_SHAS,
            ),
        }
        self.assertEqual(
            preview.validate_retained_channel_build(legacy_id, legacy), legacy
        )

        offset_legacy = json.loads(json.dumps(legacy))
        offset_legacy["built_at"] = "2026-08-09T23:30:00-04:00"
        self.assertEqual(
            preview.validate_retained_channel_build(legacy_id, offset_legacy),
            offset_legacy,
        )
        wrong_source_day = json.loads(json.dumps(offset_legacy))
        wrong_source_day["built_at"] = "2026-08-08T23:30:00-04:00"
        with self.assertRaises(ValueError):
            preview.validate_retained_channel_build(legacy_id, wrong_source_day)

        invalid_timestamp = json.loads(json.dumps(offset_legacy))
        invalid_timestamp["built_at"] = "2026-02-30T23:30:00-04:00"
        with self.assertRaisesRegex(ValueError, "ISO-8601"):
            preview.validate_retained_channel_build(legacy_id, invalid_timestamp)

        for malformed in (
            "2026-02-30-eeeeeeeeeeee",
            "2026-08-09-EEEEEEEEEEEE",
            "2026-08-09-eeeeeeeeeee",
        ):
            with self.assertRaises(ValueError):
                preview.validate_retained_channel_build(malformed, legacy)

        for mutate in (
            lambda value: value.update(commit="f" * 40),
            lambda value: value.update(built_at="2026-08-10T03:00:00Z"),
            lambda value: value["assets"]["linux-x86_64"].update(
                url="https://attacker.invalid/herdr-linux-x86_64",
                sha256="a" * 64,
            ),
        ):
            tampered = json.loads(json.dumps(legacy))
            mutate(tampered)
            with self.assertRaises(ValueError):
                preview.validate_retained_channel_build(legacy_id, tampered)


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
            self.assertEqual(set(bridge["assets"]), set(preview.ASSET_TARGETS))
            for target in preview.ASSET_TARGETS:
                self.assertEqual(bridge["assets"][target], canonical["assets"][target])
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

    def test_smarty_preview_is_a_passive_attempt_scoped_producer(self):
        source = workflow_source()
        jobs = workflow_jobs(source)
        self.assertEqual(list(jobs), ["preflight", "candidate-checks", "build", "candidate-handoff"])
        self.assertIn("on:\n  push:\n    tags:\n      - 'smarty-preview-*'", source)
        for forbidden in ("secrets.", "environment:", "id-token:", "contents: write", "attestations: write", "gh api", "gh release", "git push", "workflow_call"):
            self.assertNotIn(forbidden, source, forbidden)
        self.assertIn("ref: ${{ github.sha }}", source)
        self.assertIn("github.run_attempt", source)
        self.assertIn("smarty-candidate-handoff-${{ needs.preflight.outputs.artifact_attempt }}", source)
        self.assertIn("candidate-*-${{ needs.preflight.outputs.artifact_attempt }}", source)
        self.assertIn("source-archives/herdr-source.tar", source)
        self.assertNotIn("scripts/smarty_preview_trusted.py", source)
        build = jobs["build"]
        self.assertEqual(
            tuple(re.findall(r"^            os: (.+)$", build, flags=re.MULTILINE)),
            (
                "ubuntu-22.04",
                "ubuntu-24.04-arm",
                "macos-15-intel",
                "macos-14",
                "windows-latest",
            ),
        )
        self.assertNotIn("if: runner.os == 'Linux'", build)
        self.assertLess(build.index("- name: Install Zig"), build.index("- name: Build candidate Herdr"))
        self.assertIn("Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6", source)
        self.assertIn("workspaces: candidate-source", source)

    def test_release_workflows_use_available_runners(self):
        root = Path(__file__).resolve().parents[1] / ".github/workflows"
        for name in (
            "build-artifacts-manual.yml",
            "ci.yml",
            "preview.yml",
            "release.yml",
            "windows-arm64.yml",
        ):
            source = (root / name).read_text(encoding="utf-8")
            self.assertNotRegex(source, r"\bsmarty-(?:linux|macos|windows)", name)

    def test_smarty_preview_prepares_source_archive_and_windows_dependency(self):
        source = workflow_source()
        self.assertLess(
            source.index('Path("source-archives").mkdir()'),
            source.index('with tarfile.open("source-archives/herdr-source.tar", "w")'),
        )
        self.assertIn('Get-Content -Raw -LiteralPath "candidate-source\\packaging\\windows\\conpty.json"', source)
        self.assertIn("Invoke-WebRequest -Uri $metadata.package.url -OutFile $package", source)
        self.assertIn("Get-FileHash -Algorithm SHA256 -LiteralPath $package", source)
        self.assertIn("-PackagePath $package", source)
        self.assertNotIn("-Package $package", source)


    def test_smarty_preview_binds_exact_paired_tag_to_source_timestamp(self):
        source = workflow_source()
        self.assertIn("EVENT_REF_NAME", source)
        self.assertIn("EVENT_SHA", source)
        self.assertIn("git", source)
        self.assertIn("datetime.fromisoformat", source)
        self.assertIn('match["day"] != built_at[:10]', source)
        self.assertIn("build_id = f\"{match['day']}-p{match['parent']}-r{match['source']}-o{match['omp']}\"", source)
        self.assertIn('"legacy_day_binding": "literal-built_at-prefix"', source)
        self.assertIn('"workflow_path": ".github/workflows/smarty-preview.yml"', source)

    def test_smarty_preview_artifacts_are_attempt_scoped_and_consumed_by_identity(self):
        source = workflow_source()
        uploads = re.findall(r"^          name: ([^\n]+)$", source, re.MULTILINE)
        self.assertTrue(uploads)
        for name in uploads:
            if "needs." not in name:
                self.assertIn("${{ github.run_attempt }}", name, name)
        self.assertIn("name: smarty-release-plan-${{ needs.preflight.outputs.artifact_attempt }}", source)
        self.assertIn("name: smarty-candidate-sources-${{ needs.preflight.outputs.artifact_attempt }}", source)
        self.assertIn("pattern: candidate-*-${{ needs.preflight.outputs.artifact_attempt }}", source)
        self.assertIn("merge-multiple: true", source)

    def test_trusted_publisher_is_default_branch_workflow_run_gate(self):
        root = Path(__file__).resolve().parents[1]
        trusted = (root / ".github/workflows/smarty-preview-publish.yml").read_text(encoding="utf-8")
        jobs = workflow_jobs(trusted)
        self.assertIn("workflow_run:\n    workflows: [\"Smarty Preview\"]\n    types: [completed]", trusted)
        self.assertEqual(
            list(jobs),
            [
                "validate-seal",
                "trusted-source",
                "trusted-build",
                "trusted-omp-build",
                "trusted-assemble",
                "attest-and-seal",
                "publish-release",
                "phase-a-channel",
                "phase-a-commit",
                "phase-b-promotion",
                "phase-b-commit",
            ],
        )
        for required in (
            "github.event.workflow_run.id",
            "actions/runs/{run_id}",
            "actions/workflows/smarty-preview.yml",
            "actions/runs/{run_id}/artifacts?per_page=100",
            "validate-run",
            "validate-tag",
            "validate-producer",
            "publisher-branch.json",
            "publisher-revision.json",
            "ref: ${{ needs.validate-seal.outputs.publisher_commit }}",
        ):
            self.assertIn(required, trusted)
        self.assertIn("contents/.github/workflows/smarty-preview-publish.yml?ref=", trusted)
        self.assertIn("ref: ${{ github.workflow_sha }}", trusted)
        self.assertIn(
            "trusted-publisher-anchor-${{ steps.identity.outputs.tag }}-${{ steps.identity.outputs.run_attempt }}-${{ github.run_attempt }}",
            trusted,
        )
        self.assertIn(
            "trusted-channel-bridge-${{ needs.validate-seal.outputs.tag }}-${{ needs.validate-seal.outputs.run_attempt }}-${{ github.run_attempt }}",
            trusted,
        )
        self.assertIn(
            "trusted-channel-promotion-authorization-${{ needs.validate-seal.outputs.tag }}-${{ needs.validate-seal.outputs.run_attempt }}-${{ github.run_attempt }}",
            trusted,
        )
        for pool in (
            "ubuntu-22.04",
            "ubuntu-24.04-arm",
            "macos-15-intel",
            "macos-14",
            "windows-latest",
        ):
            self.assertIn(pool, trusted)
        self.assertIn("actions/cache@55cc8345863c7cc4c66a329aec7e433d2d1c52a9", trusted)
        self.assertIn("workspaces: source", trusted)

    def test_trusted_publisher_separates_source_release_and_attestation_authority(self):
        root = Path(__file__).resolve().parents[1]
        trusted = (root / ".github/workflows/smarty-preview-publish.yml").read_text(encoding="utf-8")
        jobs = workflow_jobs(trusted)
        self.assertIn("secrets.SMARTY_SOURCE_READ_TOKEN", jobs["trusted-source"])
        self.assertNotIn("secrets.SMARTY_SOURCE_READ_TOKEN", jobs["trusted-build"])
        for job in ("trusted-omp-build", "trusted-assemble", "attest-and-seal"):
            self.assertEqual(jobs[job].count("secrets.SMARTY_SOURCE_READ_TOKEN"), 1)
        self.assertNotIn("secrets.SMARTY_RELEASE_TOKEN", jobs["validate-seal"])
        self.assertNotIn("secrets.SMARTY_RELEASE_TOKEN", jobs["trusted-build"])
        self.assertNotIn("secrets.SMARTY_RELEASE_TOKEN", jobs["trusted-omp-build"])
        self.assertNotIn("secrets.SMARTY_RELEASE_TOKEN", jobs["trusted-assemble"])
        self.assertNotIn("secrets.SMARTY_RELEASE_TOKEN", jobs["attest-and-seal"])
        self.assertIn("secrets.SMARTY_RELEASE_TOKEN", jobs["publish-release"])
        self.assertNotIn("secrets.SMARTY_RELEASE_TOKEN", jobs["phase-a-channel"])
        self.assertNotIn("secrets.SMARTY_RELEASE_TOKEN", jobs["phase-b-promotion"])
        self.assertIn("id-token: write", jobs["attest-and-seal"])
        self.assertIn("attestations: write", jobs["attest-and-seal"])
        self.assertIn("environment: smarty-release", jobs["publish-release"])
        self.assertIn("smarty-preview-promotion", jobs["phase-b-promotion"])
        self.assertNotIn("contents: write", jobs["validate-seal"])
        self.assertNotIn("contents: write", jobs["trusted-build"])
        self.assertNotIn("contents: write", jobs["trusted-omp-build"])
        self.assertNotIn("contents: write", jobs["trusted-assemble"])

    def test_trusted_publisher_builds_and_seals_the_canonical_release_contract(self):
        root = Path(__file__).resolve().parents[1]
        trusted = (root / ".github/workflows/smarty-preview-publish.yml").read_text(encoding="utf-8")
        self.assertIn("mlugg/setup-zig@d1434d08867e3ee9daa34448df10607b98908d29", trusted)
        self.assertIn("version: ${{ env.ZIG_VERSION }}", trusted)
        self.assertNotIn("if: runner.os == 'Linux'", workflow_jobs(trusted)["trusted-build"])
        self.assertIn("setup-bun@0c5077e51419868618aeaa5fe8019c62421857d6", trusted)
        self.assertIn("bun-version: 1.4.0", trusted)
        self.assertIn("Build trusted OMP and native assets", trusted)
        self.assertIn("scripts/preview.py spdx", trusted)
        self.assertIn("scripts/preview.py pair-manifest", trusted)
        self.assertIn("actions/attest@1e69f48acb82d1966a394da916b4c1698aa569d6", trusted)
        self.assertIn("smarty-provenance-", trusted)
        self.assertIn("trusted-final-release-", trusted)
        self.assertIn("release(tagName: $tag) { databaseId }", trusted)
        self.assertIn('gh api "repos/$REPOSITORY/releases/$release_id"', trusted)
        self.assertIn("gh release upload", trusted)
        self.assertIn("smarty_preview_trusted.py seal", trusted)
        self.assertIn('export HERDR_BUILD_UPDATE_MANIFEST_URL="https://raw.githubusercontent.com/Smarty-Pants-Inc/herdr/smarty-channel/preview.json"', trusted)
        self.assertIn("export HERDR_BUILD_AUTO_UPDATE=true", trusted)
        self.assertIn("setup-bazel@5bab119910beb57b5848d5090ee6d35c031fb26e", trusted)
        for platform in ("linux-x86_64", "linux-aarch64", "macos-x86_64", "macos-aarch64"):
            self.assertIn("trusted-herdr-" + platform + "-${{ needs.validate-seal.outputs.run_attempt }}", trusted)
            self.assertIn("trusted-omp-" + platform + "-${{ needs.validate-seal.outputs.run_attempt }}", trusted)
        self.assertIn("trusted-herdr-windows-x86_64-${{ needs.validate-seal.outputs.run_attempt }}", trusted)
        self.assertEqual(len(preview.FULL_RELEASE_ASSET_NAMES), 37)
        self.assertEqual(len(set(preview.FULL_RELEASE_ASSET_NAMES)), 37)
        self.assertEqual(preview.SEMANTIC_VERIFICATION_SCHEMA, "smarty.semantic-verification.v2")
        self.assertEqual(preview.SOURCE_ARCHIVE_NAMES, ("herdr-source.tar",))
        jobs = workflow_jobs(trusted)
        self.assertNotIn("omp-source.tar", trusted)
        self.assertNotIn("tarfile.open", jobs["trusted-source"])
        self.assertIn("trusted-source-record.json", jobs["trusted-source"])
        self.assertNotIn("publisher-source", jobs["trusted-build"])
        self.assertIn("source\\scripts\\package_windows_conpty.ps1", jobs["trusted-build"])
        for job in ("trusted-omp-build", "trusted-assemble", "attest-and-seal"):
            self.assertIn("Checkout exact private OMP source", jobs[job])
            self.assertIn("ref: ${{ needs.trusted-source.outputs.omp_commit }}", jobs[job])
            self.assertIn("token: ${{ secrets.SMARTY_SOURCE_READ_TOKEN }}", jobs[job])
            self.assertIn('git -C omp-source rev-parse HEAD^{commit}', jobs[job])
        self.assertNotIn("extract-tar", jobs["trusted-omp-build"])
        self.assertNotIn("HERDR_BUILD_OMP", jobs["trusted-omp-build"])

    def test_trusted_and_candidate_embedded_python_blocks_compile(self):
        root = Path(__file__).resolve().parents[1]
        for label, path in (("candidate", WORKFLOW_PATH), ("trusted", root / ".github/workflows/smarty-preview-publish.yml")):
            source = path.read_text(encoding="utf-8")
            lines = source.splitlines()
            blocks = []
            index = 0
            while index < len(lines):
                if lines[index].rstrip().endswith("<<'PY'"):
                    body = []
                    index += 1
                    while index < len(lines) and lines[index].strip() != "PY":
                        body.append(lines[index][10:])
                        index += 1
                    blocks.append("\n".join(body))
                index += 1
            self.assertGreaterEqual(len(blocks), 1, label)
            for block in blocks:
                compile(block, f"<{label}>", "exec")

    def test_workflows_never_interpolate_expressions_into_shell_blocks(self):
        root = Path(__file__).resolve().parents[1]
        for path in (WORKFLOW_PATH, root / ".github/workflows/smarty-preview-publish.yml"):
            inside = False
            for line in path.read_text(encoding="utf-8").splitlines():
                if re.fullmatch(r" {8}run: \|", line):
                    inside = True
                    continue
                if re.fullmatch(r" {8}run: (?!\|)(.*)", line):
                    inside = False
                    self.assertNotIn("${{", line)
                    continue
                if inside:
                    if line.strip() and not line.startswith(" " * 10):
                        inside = False
                    else:
                        self.assertNotIn("${{", line)

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
                bun_version="1.4.0",
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
            self.assertEqual(document["toolchains"]["omp"]["bun"], "1.4.0")
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
                {
                    target: platform["runner"]
                    for target, platform in document["platforms"].items()
                },
                {
                    "linux-x86_64": "ubuntu-22.04",
                    "linux-aarch64": "ubuntu-24.04-arm",
                    "macos-x86_64": "macos-15-intel",
                    "macos-aarch64": "macos-14",
                    "windows-x86_64": "windows-latest",
                },
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
            "Get-BridgeHerdrAssets",
            "AcceptedBuildIds = @($alias, $canonical.BuildId)",
        ):
            self.assertIn(key, bridge)
        self.assertIn("Get-BridgeReleaseAssetUrl -Canonical $Canonical", source)
        self.assertIn('"windows-x86_64" = "herdr-windows-x86_64.zip"', source)
        self.assertIn('ExpectedSha256 $asset.Sha256', source)
        self.assertIn('identity.sha256', source)

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

    def test_legacy_retained_date_binding_uses_literal_source_day(self):
        source = INSTALLER_PATH.read_text(encoding="utf-8")
        retained = source[
            source.index("function Get-RetainedPreviewBuild") : source.index(
                "function Get-RetainedPreviewBuilds"
            )
        ]
        legacy = retained[
            retained.index('if ($identity.Kind -eq "legacy")') : retained.index(
                "    } else {", retained.index('if ($identity.Kind -eq "legacy")')
            )
        ]
        self.assertIn("$sourceBuiltAt = Get-RequiredManifestProperty", retained)
        self.assertIn(
            "$builtAt = Get-RequiredManifestTimestamp -Value $sourceBuiltAt", retained
        )
        self.assertIn("$identity.Day -cne $sourceBuiltAt.Substring(0, 10)", legacy)
        self.assertNotIn("$builtAt.Substring(0, 10)", legacy)
        self.assertLess(
            retained.index("Get-RequiredManifestTimestamp"),
            retained.index("$sourceBuiltAt.Substring(0, 10)"),
        )

        fixture = (INSTALLER_PATH.parent.parent / "scripts/windows_install_conpty_package_test.ps1").read_text(
            encoding="utf-8"
        )
        for built_at in (
            'built_at = "2026-08-09T23:30:00-04:00"',
            '"2026-08-08T23:30:00-04:00"',
            '"2026-02-30T23:30:00-04:00"',
        ):
            self.assertIn(built_at, fixture)

    def test_preview_contract_routes_exact_manifest_urls(self):
        source = INSTALLER_PATH.read_text(encoding="utf-8")
        resolver = source[
            source.index("function Resolve-PreviewManifest") : source.index(
                "function ConvertTo-ManifestObject"
            )
        ]
        self.assertIn(
            '$smartyManifestUrl = "https://raw.githubusercontent.com/Smarty-Pants-Inc/herdr/smarty-channel/preview.json"',
            resolver,
        )
        self.assertIn("Resolve-UpstreamPreviewManifest -Manifest $Manifest -Target $Target", resolver)
        self.assertIn("Resolve-CustomPreviewManifest -Manifest $Manifest -Target $Target", resolver)
        self.assertIn(
            'throw "Smarty Phase A bridge manifests require the exact Smarty channel manifest URL."',
            resolver,
        )
        main = source[source.index('Write-Step "Fetching Herdr $Channel manifest"') :]
        self.assertIn(
            "$previewSelection = Resolve-PreviewManifest -Manifest $manifest -Target $target -ManifestUrl $ManifestUrl",
            main,
        )
        self.assertIn("function Get-UpstreamHerdrAssets", source)
        self.assertIn("function Get-CustomPreviewAssets", source)


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
