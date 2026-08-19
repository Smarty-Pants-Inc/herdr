import json
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path

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
        workflow = (
            Path(__file__).resolve().parents[1] / ".github/workflows/smarty-preview.yml"
        ).read_text(encoding="utf-8")
        source_identity = workflow.split("- name: Verify paired OMP source identity", 1)[1].split(
            "- name: Install Bun for OMP", 1
        )[0]
        build_identity = workflow.split("- name: Verify paired OMP build identity", 1)[1].split(
            "- name: Package artifact", 1
        )[0]

        self.assertIn("packages/coding-agent/package.json", source_identity)
        self.assertIn("needs.preflight.outputs.omp_version", source_identity)
        self.assertLess(
            workflow.index("- name: Verify paired OMP build identity"),
            workflow.index("- name: Upload paired artifact"),
        )
        self.assertIn('omp_id="$("$omp_binary" __build-id)"', build_identity)
        self.assertIn('omp_version="$("$omp_binary" --version)"', build_identity)
        self.assertIn(
            'test "$omp_version" = "omp/${{ needs.preflight.outputs.omp_version }}"',
            build_identity,
        )

    def test_smarty_preview_namespaces_the_tag_without_changing_the_build_id(self):
        root = Path(__file__).resolve().parents[1]
        official = (root / ".github/workflows/preview.yml").read_text(encoding="utf-8")
        smarty = (root / ".github/workflows/smarty-preview.yml").read_text(
            encoding="utf-8"
        )

        self.assertIn('build_id="$day-$short_sha"', official)
        self.assertIn('echo "build_id=$day-$short_sha"', smarty)
        self.assertIn('echo "tag=smarty-preview-$day-$short_sha"', smarty)



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
