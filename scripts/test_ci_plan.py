from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from scripts import ci_plan

PLANNER = Path(ci_plan.__file__).resolve()
BASE_MATRIX = {
    "include": [
        {"os": "ubuntu-latest", "kind": "unix", "nextest_filter": "all()"},
        {"os": "windows-latest", "kind": "windows"},
    ]
}
FULL_MATRIX = {
    "include": [
        BASE_MATRIX["include"][0],
        {
            "os": "macos-latest",
            "kind": "unix",
            "nextest_filter": (
                "not binary(live_handoff) or "
                "test(=live_handoff_import_exits_when_its_test_owner_dies)"
            ),
        },
        BASE_MATRIX["include"][1],
    ]
}


class CIPlanTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        # All Git writes are fixture-only; the task checkout is read-only to tests.
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith("GIT_")
        }
        self.env.update({"GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull})
        self.git("init", "--quiet", "--template=", "-b", "main")
        self.write("README.md", "initial docs\n")
        self.write("src/main.rs", "initial source\n")
        self.before = self.commit()
        self.counter = 0

    def git(self, *args: str) -> str:
        return subprocess.check_output(
            [
                "git", "-c", "user.name=CI fixture", "-c", "user.email=fixture@example.invalid",
                "-c", "commit.gpgsign=false", "-c", "core.autocrlf=false", *args,
            ],
            cwd=self.repo, env=self.env, stderr=subprocess.PIPE, text=True, timeout=10,
        ).strip()

    def write(self, relative: str, content: str = "changed\n") -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def commit(self) -> str:
        self.git("add", "--all")
        self.git("commit", "--quiet", "--allow-empty", "-m", "test: fixture")
        return self.git("rev-parse", "HEAD")

    def event(self, after: str | None = None, *, queue: bool = False) -> dict:
        return {
            "repository": {"full_name": "Smarty-Pants-Inc/herdr"},
            "ref": "refs/heads/master",
            "before": self.before,
            "after": after or self.before,
            "pull_request": {
                "draft": False,
                "user": {"id": 37929162 if queue else 123},
                "base": {
                    "ref": "master", "sha": self.before,
                    "repo": {"full_name": "Smarty-Pants-Inc/herdr"},
                },
                "head": {
                    "ref": "mergify/merge-queue/master/pr-12" if queue else "feature",
                    "sha": after or self.before,
                    "repo": {"full_name": "Smarty-Pants-Inc/herdr"},
                },
            },
        }

    def plan(self, event: object, name: str = "pull_request", *, cwd: Path | None = None,
             raw: bool = False, missing: bool = False) -> tuple[dict, bool]:
        self.counter += 1
        event_path = self.root / f"event-{self.counter}.json"
        output_path = self.root / f"output-{self.counter}"
        if not missing:
            event_path.write_text(event if raw else json.dumps(event), encoding="utf-8")
        output_path.write_text("existing=keep\n", encoding="utf-8")
        subprocess.run(
            [sys.executable, str(PLANNER), "--event-name", name, "--event-path", str(event_path),
             "--github-output", str(output_path)],
            cwd=cwd or self.repo, env=self.env, capture_output=True, text=True,
            check=True, timeout=15,
        )
        lines = output_path.read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(lines), 3)
        self.assertEqual(lines[0], "existing=keep")
        outputs = dict(line.split("=", 1) for line in lines)
        self.assertIn(outputs["conpty"], ("true", "false"))
        return json.loads(outputs["matrix"]), outputs["conpty"] == "true"

    def test_ordinary_pr_keeps_linux_and_windows_even_for_docs_only(self) -> None:
        self.write("docs/next/website/page.mdx")
        matrix, conpty = self.plan(self.event(self.commit()))
        self.assertEqual(matrix, BASE_MATRIX)
        self.assertFalse(conpty)

    def test_master_push_keeps_full_matrix_even_for_docs_only(self) -> None:
        self.write("docs/page.md")
        matrix, conpty = self.plan(self.event(self.commit()), "push")
        self.assertEqual(matrix, FULL_MATRIX)
        self.assertFalse(conpty)

    def test_windows_push_does_not_select_macos(self) -> None:
        event = self.event()
        event["ref"] = "refs/heads/windows"
        self.assertEqual(self.plan(event, "push"), (BASE_MATRIX, False))

    def test_genuine_queue_selects_macos_on_both_supported_bases_without_draft(self) -> None:
        for base in ("master", "smarty-preview-source"):
            for draft in (False, True, None):
                with self.subTest(base=base, draft=draft):
                    event = self.event(queue=True)
                    event["pull_request"]["base"]["ref"] = base
                    if draft is None:
                        del event["pull_request"]["draft"]
                    else:
                        event["pull_request"]["draft"] = draft
                    self.assertEqual(self.plan(event), (FULL_MATRIX, False))

    def test_queue_identity_requires_all_gates(self) -> None:
        changes = (
            ("user", "id", 123),
            ("user", "id", "37929162"),
            ("user", "id", 37929162.0),
            ("head", "ref", "feature"),
            ("head", "ref", None),
            ("base", "ref", "windows"),
            ("head", "repo", {"full_name": "external/herdr"}),
            ("head", "repo", None),
            ("base", "repo", {}),
        )
        for section, key, value in changes:
            with self.subTest(section=section, key=key, value=value):
                event = self.event(queue=True)
                event["pull_request"][section][key] = value
                self.assertEqual(self.plan(event)[0], BASE_MATRIX)
        event = self.event(queue=True)
        event["ref"] = "refs/heads/windows"
        self.assertEqual(self.plan(event, "push")[0], BASE_MATRIX)

    def test_preview_source_ordinary_pr_and_push_do_not_select_macos(self) -> None:
        event = self.event()
        event["pull_request"]["base"]["ref"] = "smarty-preview-source"
        event["ref"] = "refs/heads/smarty-preview-source"
        self.assertEqual(self.plan(event)[0], BASE_MATRIX)
        self.assertEqual(self.plan(event, "push")[0], BASE_MATRIX)

    def test_all_build_and_package_inputs_enable_conpty(self) -> None:
        paths = (
            "src/main.rs", "src/integration/assets/claude/herdr-agent-state.ps1",
            "assets/sounds/done.mp3", "vendor/portable-pty/src/win/psuedocon.rs",
            "vendor/libghostty-vt/pkg/example.zig", "vendor/libghostty-vt.vendor.json",
            "packaging/windows/conpty.json", "packaging/windows/NOTICE.md",
            ".cargo/config.toml", ".gitattributes",
            "Cargo.toml", "Cargo.lock", "build.rs", "rust-toolchain", "rust-toolchain.toml",
            ".github/workflows/ci.yml", "scripts/ci_plan.py", "scripts/test_ci_plan.py",
            "scripts/package_windows_conpty.py", "scripts/package_windows_conpty.ps1",
            "scripts/windows_smoke_conpty_path.ps1",
            "scripts/windows_conpty_enhanced_input_probe.ps1",
            "scripts/windows_install_conpty_package_test.ps1",
            "distribution/install.ps1", "distribution/install.cmd",
            "skills/herdr/SKILL.md", "docs/next/api/herdr-api.schema.json",
        )
        before = self.before
        for path in paths:
            with self.subTest(path=path):
                self.write(path, f"input: {path}\n")
                after = self.commit()
                event = self.event(after)
                event["before"] = before
                event["pull_request"]["base"]["sha"] = before
                self.assertTrue(self.plan(event)[1])
                self.assertTrue(self.plan(event, "push")[1])
                before = after

    def test_irrelevant_paths_do_not_match_prefixes_or_script_lookalikes(self) -> None:
        for path in (
            "README.md", "docs/next/README.md", "docs/next/website/page.mdx",
            "src-like/file.md", "assets-like/file.md", "packaging-notes.md",
            "scripts/package_windows_conpty.ps1.md", "scripts/unrelated.py",
            "assets/screenshots/example.png", "vendor/libghostty-vt.patches.md",
            "vendor/patches/libghostty-vt/example.patch", "packaging/notes.md",
            "distribution/latest.json", "distribution/preview.json", "distribution/install.sh",
            "skills/herdr/other.md", "docs/next/api/other.json", ".github/workflows/other.yml",
        ):
            self.write(path)
        self.assertFalse(self.plan(self.event(self.commit()))[1])

    def test_deleting_source_enables_conpty(self) -> None:
        (self.repo / "src/main.rs").unlink()
        event = self.event(self.commit())
        self.assertTrue(self.plan(event)[1])
        self.assertTrue(self.plan(event, "push")[1])

    def test_rename_out_of_source_preserves_deleted_input_path(self) -> None:
        destination = self.repo / "docs/source.md"
        destination.parent.mkdir()
        (self.repo / "src/main.rs").rename(destination)
        event = self.event(self.commit())
        self.assertIn("R100", self.git("diff", "--name-status", "--find-renames", self.before, event["after"]))
        self.assertTrue(self.plan(event)[1])
        self.assertTrue(self.plan(event, "push")[1])

    def test_rename_into_source_enables_conpty(self) -> None:
        (self.repo / "README.md").rename(self.repo / "src/new.rs")
        self.assertTrue(self.plan(self.event(self.commit()))[1])

    def test_docs_rename_does_not_enable_conpty(self) -> None:
        (self.repo / "README.md").rename(self.repo / "other-doc.md")
        self.assertFalse(self.plan(self.event(self.commit()))[1])

    def test_path_with_newline_is_not_misread_as_source(self) -> None:
        if os.name == "nt":
            self.skipTest("Windows does not permit newlines in file names")
        self.write("docs/note\nsrc/fake.rs")
        self.assertFalse(self.plan(self.event(self.commit()))[1])

    def test_unicode_source_path_enables_conpty(self) -> None:
        self.write("src/新.rs")
        self.assertTrue(self.plan(self.event(self.commit()))[1])

    def test_pr_merge_result_includes_base_side_source_changes(self) -> None:
        ancestor = self.before
        self.write("src/main.rs", "base advanced\n")
        base = self.commit()
        self.git("checkout", "--quiet", "-b", "feature", ancestor)
        self.write("docs/page.md")
        head = self.commit()
        self.git("merge", "--quiet", "--no-edit", "main")
        event = self.event(head)
        event["pull_request"]["base"]["sha"] = base
        self.assertTrue(self.plan(event)[1])

    def test_pr_merge_resolution_not_in_head_alone_enables_conpty(self) -> None:
        self.write("docs/base.md")
        base = self.commit()
        self.git("checkout", "--quiet", "-b", "feature", self.before)
        self.write("docs/head.md")
        head = self.commit()
        self.git("merge", "--quiet", "--no-commit", "--no-ff", "main")
        self.write("src/main.rs", "merge-only resolution\n")
        self.commit()
        event = self.event(head)
        event["pull_request"]["base"]["sha"] = base
        self.assertTrue(self.plan(event)[1])

    def test_docs_only_pr_merge_result_skips_conpty(self) -> None:
        self.write("docs/base.md")
        base = self.commit()
        self.git("checkout", "--quiet", "-b", "feature", self.before)
        self.write("docs/head.md")
        head = self.commit()
        self.git("merge", "--quiet", "--no-edit", "main")
        event = self.event(head)
        event["pull_request"]["base"]["sha"] = base
        self.assertFalse(self.plan(event)[1])

    def test_push_uses_two_tree_diff_not_merge_base(self) -> None:
        self.write("src/main.rs", "same source in both branches\n")
        base = self.commit()
        self.git("checkout", "--quiet", "-b", "feature", self.before)
        self.write("src/main.rs", "same source in both branches\n")
        self.write("docs/page.md")
        head = self.commit()
        event = self.event(head)
        event["before"] = base
        self.assertFalse(self.plan(event, "push")[1])

    def test_mismatched_checkout_cannot_authorize_skip(self) -> None:
        self.write("docs/base.md")
        base = self.commit()
        self.git("checkout", "--quiet", "-b", "feature", self.before)
        self.write("docs/head.md")
        head = self.commit()
        event = self.event(head)
        event["pull_request"]["base"]["sha"] = base
        self.assertTrue(self.plan(event)[1])
        event["after"] = base
        self.assertTrue(self.plan(event, "push")[1])

    def test_uncommitted_files_do_not_change_event_diff(self) -> None:
        self.write("src/main.rs", "uncommitted source\n")
        self.assertFalse(self.plan(self.event())[1])

    def test_unknown_and_zero_commits_enable_conpty(self) -> None:
        for sha in ("0" * 40, "f" * 40, "not-a-sha", "HEAD", "--help", None, 12):
            for name in ("pull_request", "push"):
                for endpoint in ("before", "after"):
                    with self.subTest(sha=sha, name=name, endpoint=endpoint):
                        event = self.event()
                        event[endpoint] = sha
                        section = "base" if endpoint == "before" else "head"
                        event["pull_request"][section]["sha"] = sha
                        self.assertTrue(self.plan(event, name)[1])

    def test_blob_sha_is_not_complete_commit_evidence(self) -> None:
        event = self.event()
        event["after"] = self.git("rev-parse", "HEAD:README.md")
        self.assertTrue(self.plan(event, "push")[1])

    def test_missing_tree_git_error_enables_conpty(self) -> None:
        self.write("docs/page.md")
        after = self.commit()
        tree = self.git("rev-parse", f"{after}^{{tree}}")
        (self.repo / ".git/objects" / tree[:2] / tree[2:]).unlink()
        self.assertTrue(self.plan(self.event(after), "push")[1])

    def test_not_a_repository_enables_conpty(self) -> None:
        self.assertTrue(self.plan(self.event(), cwd=self.root)[1])

    def test_shallow_checkout_enables_conpty_even_when_endpoints_exist(self) -> None:
        self.write("docs/page.md")
        after = self.commit()
        shallow = self.root / "shallow"
        self.git("clone", "--quiet", "--depth=2", "--no-local", self.repo.as_uri(), str(shallow))
        self.assertTrue(self.plan(self.event(after), cwd=shallow)[1])

    def test_unrelated_pr_histories_enable_conpty(self) -> None:
        self.git("checkout", "--quiet", "--orphan", "unrelated")
        self.write("README.md", "unrelated history\n")
        self.assertTrue(self.plan(self.event(self.commit()))[1])

    def test_malformed_or_missing_event_enables_conpty_and_valid_outputs(self) -> None:
        for event in (None, [], 1, {}, {"pull_request": None}, {"pull_request": {"base": []}}, "bad"):
            with self.subTest(event=event):
                self.assertEqual(self.plan(event), (BASE_MATRIX, True))
        self.assertEqual(self.plan("{broken", raw=True), (BASE_MATRIX, True))
        self.assertEqual(self.plan(None, missing=True), (BASE_MATRIX, True))
        self.assertTrue(self.plan({}, "push")[1])
        self.assertTrue(self.plan(self.event(), "workflow_dispatch")[1])

    def test_many_changed_paths_are_not_truncated(self) -> None:
        for index in range(3001):
            self.write(f"docs/page-{index}.md")
        docs_head = self.commit()
        self.assertFalse(self.plan(self.event(docs_head))[1])
        self.write("src/last.rs")
        self.assertTrue(self.plan(self.event(self.commit()))[1])

    def test_multicommit_push_includes_earlier_source_change(self) -> None:
        self.write("src/main.rs", "first commit source\n")
        self.commit()
        self.write("docs/page.md")
        self.assertTrue(self.plan(self.event(self.commit()), "push")[1])

    def test_ambiguous_merge_base_cannot_authorize_skip(self) -> None:
        sha = self.before.encode()
        with mock.patch.object(ci_plan, "git", side_effect=[
            b"false\n", b"commit\n", b"commit\n", sha + b"\n", sha + b"\n" + sha + b"\n",
        ]):
            self.assertTrue(ci_plan.needs_conpty("pull_request", self.event()))

    def test_git_timeout_and_incomplete_path_output_enable_conpty(self) -> None:
        for side_effect in (
            subprocess.TimeoutExpired("git", 30),
            [b"false\n", b"commit\n", b"commit\n", self.before.encode() + b"\n", b"docs/truncated"],
        ):
            with self.subTest(side_effect=side_effect), mock.patch.object(ci_plan, "git", side_effect=side_effect):
                self.assertTrue(ci_plan.needs_conpty("push", self.event()))


if __name__ == "__main__":
    unittest.main()
