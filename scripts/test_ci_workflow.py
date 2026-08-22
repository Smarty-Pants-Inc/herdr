from __future__ import annotations

from pathlib import Path
import unittest


WORKFLOW = Path(__file__).resolve().parent.parent / ".github" / "workflows" / "ci.yml"


class ConventionalCommitWorkflowTests(unittest.TestCase):
    def test_only_verified_mergify_speculative_drafts_skip_title_validation(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        self.assertIn(
            """      - name: Validate PR title
        if: >-
          github.event_name == 'pull_request' &&
          !(github.event.pull_request.draft &&
            github.event.pull_request.user.login == 'mergify[bot]' &&
            startsWith(github.event.pull_request.title, 'merge queue: checking '))
""",
            workflow,
        )
        self.assertIn(
            'run: python3 scripts/conventional_commits.py "$PR_TITLE"', workflow
        )


if __name__ == "__main__":
    unittest.main()
