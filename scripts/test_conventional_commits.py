from __future__ import annotations

import contextlib
import io
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from scripts.conventional_commits import (
    ALLOWED_TYPES,
    MERGIFY_AUTHOR_EMAIL,
    commit_message_subject,
    git_subjects,
    main,
    valid_subject,
)

# Actual historical queue merges: 5a6dd2be, a19cd1e9, and fd89458d.
LEGACY_MERGES = (
    (
        "Merge pull request #89 from Smarty-Pants-Inc/perf/input-guard-cost",
        "perf(api): check the method before attributing the caller in the cross-pane guard",
    ),
    (
        "Merge pull request #88 from Smarty-Pants-Inc/ci/mergify-merge-subject",
        "ci(mergify): merge commits take the PR title as their subject",
    ),
    (
        "Merge pull request #43 from Smarty-Pants-Inc/i3a-corrective/c3-seam-closure",
        "fix: close Increment 3A seam",
    ),
)


def git_record(
    subject: str,
    body: str = "",
    parents: str = "a b",
    author_email: str = MERGIFY_AUTHOR_EMAIL,
) -> str:
    return "\0".join((parents, author_email, subject, body)) + "\0"


class ConventionalCommitTests(unittest.TestCase):
    def subjects_from(self, output: str) -> list[str]:
        with patch(
            "scripts.conventional_commits.subprocess.check_output", return_value=output
        ) as git:
            subjects = git_subjects("before..after")
        git.assert_called_once_with(
            ["git", "log", "-z", "--format=%P%x00%ae%x00%s%x00%b", "before..after"],
            text=True,
        )
        return subjects

    def run_main(self, *args: str) -> tuple[int, str]:
        output = io.StringIO()
        with patch("sys.argv", ["conventional_commits.py", *args]):
            with contextlib.redirect_stdout(output):
                status = main()
        return status, output.getvalue()

    def test_conventional_subject_policy_is_unchanged(self) -> None:
        for kind in ALLOWED_TYPES:
            for suffix in (": describe the change", "(api)!: describe the change"):
                with self.subTest(kind=kind, suffix=suffix):
                    self.assertTrue(valid_subject(kind + suffix))
        for subject in (
            "Re-run review for the round-4 real-client proof (#98)",
            "style: format pull refusal regression",
            "Fix: handle input",
            "fix: ",
            "fix(api):",
            "fix(): handle input",
            "fix handle input",
        ):
            with self.subTest(subject=subject):
                self.assertFalse(valid_subject(subject))

    def test_branch_sync_subject_must_be_conventional(self) -> None:
        self.assertFalse(valid_subject("Merge master-latest into ci/4346"))
        self.assertTrue(valid_subject("merge: master into ci/4346"))
        self.assertEqual(self.run_main("merge: master into ci/4346"), (0, ""))

    def test_diagnostics_teach_branch_sync_subject(self) -> None:
        status, diagnostics = self.run_main("Merge master-latest into ci/4346")
        self.assertEqual(status, 1)
        self.assertIn("merge: master into <branch>", diagnostics)

    def test_current_pr_title_subjects_pass_without_special_treatment(self) -> None:
        subjects = [
            "feat(attribution): expose client sender names per pane (#130)",
            "ci(mergify): upgrade configuration to current format (#92)",
        ]
        self.assertEqual(
            self.subjects_from("".join(git_record(subject) for subject in subjects)),
            subjects,
        )
        self.assertEqual(self.run_main(*subjects), (0, ""))

    def test_legacy_queue_merges_validate_the_actual_pr_title(self) -> None:
        output = "".join(git_record(subject, title) for subject, title in LEGACY_MERGES)
        self.assertEqual(self.subjects_from(output), [title for _, title in LEGACY_MERGES])

    def test_raw_merge_subjects_are_not_conventional(self) -> None:
        for subject, _ in LEGACY_MERGES:
            with self.subTest(subject=subject):
                self.assertFalse(valid_subject(subject))
                self.assertEqual(self.run_main(subject)[0], 1)

    def test_non_mergify_authors_are_not_exempt(self) -> None:
        subject, title = LEGACY_MERGES[0]
        for email in (
            "human@example.com",
            "noreply@github.com",
            "mergify[bot]@example.com",
            "other-bot@users.noreply.github.com",
        ):
            with self.subTest(email=email):
                self.assertEqual(
                    self.subjects_from(git_record(subject, title, author_email=email)),
                    [subject],
                )

    def test_ordinary_root_and_octopus_commits_are_not_exempt(self) -> None:
        subject, title = LEGACY_MERGES[0]
        for parents in ("", "a", "a b c"):
            with self.subTest(parents=parents):
                self.assertEqual(
                    self.subjects_from(git_record(subject, title, parents=parents)),
                    [subject],
                )

    def test_other_merge_text_is_not_exempt(self) -> None:
        for subject in (
            "Merge remote-tracking branch 'origin/master' into fix/report-agent-render-on-change",
            "Merge branch 'master'",
            "Merge pull request #89",
            "Merge pull request #89 from perf",
            "Merge pull request #0 from Smarty-Pants-Inc/perf/input-guard-cost",
            "Merge pull request #89 from Smarty-Pants-Inc/perf/input-guard-cost extra",
            "arbitrary nonconventional subject",
        ):
            with self.subTest(subject=subject):
                self.assertEqual(
                    self.subjects_from(git_record(subject, "fix: valid body title")),
                    [subject],
                )

    def test_missing_or_invalid_pr_titles_are_not_exempt(self) -> None:
        subject, _ = LEGACY_MERGES[0]
        for body in (
            "",
            "\n\n",
            "not conventional",
            "style: format code",
            "fix: ",
            "invalid title\n\nfix: valid later paragraph",
            "invalid title\nfix: valid later line",
        ):
            with self.subTest(body=body):
                self.assertEqual(self.subjects_from(git_record(subject, body)), [subject])

    def test_body_lines_do_not_become_separate_commit_subjects(self) -> None:
        subject, title = LEGACY_MERGES[0]
        ordinary = "test: cover input"
        output = git_record(subject, title + "\n\nPR details\nrefs #931\n")
        output += git_record(ordinary, "ordinary details\n", parents="a")
        self.assertEqual(self.subjects_from(output), [title, ordinary])

    def test_empty_git_range(self) -> None:
        self.assertEqual(self.subjects_from(""), [])

    def test_cli_range_still_rejects_invalid_ordinary_commits(self) -> None:
        subject, title = LEGACY_MERGES[0]
        invalid = "Re-run review for the round-4 real-client proof (#98)"
        output = git_record(subject, title) + git_record(invalid, "fix: body", parents="a")
        with patch("scripts.conventional_commits.subprocess.check_output", return_value=output):
            status, diagnostics = self.run_main("--range", "before..after")
        self.assertEqual(status, 1)
        self.assertIn(invalid, diagnostics)
        self.assertNotIn(subject, diagnostics)

    def test_cli_range_accepts_only_valid_queue_titles(self) -> None:
        subject, title = LEGACY_MERGES[0]
        with patch(
            "scripts.conventional_commits.subprocess.check_output",
            return_value=git_record(subject, title),
        ):
            self.assertEqual(self.run_main("--range", "before..after"), (0, ""))
        with patch(
            "scripts.conventional_commits.subprocess.check_output",
            return_value=git_record(subject, "not conventional"),
        ):
            status, diagnostics = self.run_main("--range", "before..after")
        self.assertEqual(status, 1)
        self.assertIn(subject, diagnostics)

    def test_message_files_do_not_get_a_merge_text_exemption(self) -> None:
        subject, title = LEGACY_MERGES[0]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "COMMIT_EDITMSG"
            path.write_text(f"# comment\n\n{subject}\n\n{title}\n", encoding="utf-8")
            self.assertEqual(commit_message_subject(path), subject)
            self.assertEqual(self.run_main("--message-file", str(path))[0], 1)
            path.write_text(f"# comment\n\n{title}\n", encoding="utf-8")
            self.assertEqual(self.run_main("--message-file", str(path)), (0, ""))


if __name__ == "__main__":
    unittest.main()
