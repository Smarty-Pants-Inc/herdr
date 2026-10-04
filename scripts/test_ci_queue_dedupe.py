"""Behavioral fixtures for the actual GET/CLI evidence path (no network)."""
from contextlib import redirect_stdout
from copy import deepcopy
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

from scripts import ci_queue_dedupe as dedupe

REPO = "example/herdr"
REPO_ID = 100
BEFORE, AFTER, HEAD, TREE = (f"{n:040x}" for n in range(1, 5))
CHECKED = f"{8:040x}"
COMMITTED = "2026-10-03T12:00:00Z"
STARTED = "2026-10-03T11:00:00Z"
FINISHED = "2026-10-03T11:30:00Z"
EXPECTED_JOBS = {"smarty-ci", "check (ubuntu-latest)", "check (macos-latest)",
                 "check (windows-latest)", "conventional-commits"}


def commit(sha, parents=(), subject="fix: repair pane (#42)", tree=TREE):
    return {"sha": sha, "committer": {"id": 19864447, "login": "web-flow"},
            "parents": [{"sha": p} for p in parents],
            "commit": {"message": subject, "tree": {"sha": tree},
                       "committer": {"date": COMMITTED}}}


def checkout_log(sha=CHECKED, git="/usr/bin/git"):
    return "\n".join("2026-10-03T11:00:00.0000000Z " + line for line in [
        "##[group]Run actions/checkout@" + dedupe.CHECKOUT_PIN,
        "##[endgroup]", f"[command]{git} log -1 --format=%H", sha,
        "##[group]Removing auth", "##[endgroup]", "##[group]Run test command",
    ])


class FixtureApi:
    """REST-shaped responses; endpoint routing makes missing requests fail closed."""

    def __init__(self):
        repo = {"id": REPO_ID, "full_name": REPO}
        self.commits = {BEFORE: commit(BEFORE), AFTER: commit(AFTER, (BEFORE, HEAD)),
                        HEAD: commit(HEAD, (BEFORE,)), CHECKED: commit(CHECKED, (BEFORE, HEAD))}
        self.originals = {42: {"number": 42, "title": "fix: repair pane", "state": "closed",
                              "merged": True, "merged_at": COMMITTED,
                              "merged_by": {"id": dedupe.MERGIFY_ID, "login": "mergify[bot]"},
                              "merge_commit_sha": AFTER,
                              "base": {"ref": "master", "repo": repo}}}
        self.queue = {"number": 100, "user": {"id": dedupe.MERGIFY_ID}, "draft": True,
                      "state": "closed", "updated_at": COMMITTED, "created_at": STARTED,
                      "head": {"sha": HEAD, "ref": "mergify/merge-queue/master/pr-42", "repo": repo},
                      "base": {"sha": BEFORE, "ref": "master", "repo": repo}}
        self.prs = [self.queue]
        self.run = {"id": 7, "workflow_id": 99, "path": dedupe.WORKFLOW,
                    "event": "pull_request", "head_sha": HEAD,
                    "head_branch": self.queue["head"]["ref"], "head_repository": repo,
                    "repository": repo, "status": "completed", "conclusion": "success",
                    "run_attempt": 1, "created_at": STARTED, "updated_at": FINISHED,
                    "pull_requests": [{"number": 100, "head": deepcopy(self.queue["head"]),
                                       "base": deepcopy(self.queue["base"])}]}
        self.runs = [self.run]
        self.head_runs = None
        self.details = {7: self.run}
        self.jobs = [{"id": 1000 + index, "name": name, "run_id": 7, "run_attempt": 1, "head_sha": HEAD,
                      "status": "completed", "conclusion": "success",
                      "started_at": STARTED, "completed_at": FINISHED}
                     for index, name in enumerate(sorted(EXPECTED_JOBS))]
        self.attempts = {(7, 1): self.jobs}
        self.logs = {f"repos/{REPO}/actions/jobs/{job['id']}/logs": checkout_log()
                     for job in self.jobs if job["name"].startswith("check (")}
        self.calls = []
        self.fail = None

    def text(self, endpoint):
        self.calls.append(endpoint)
        if self.fail and self.fail in endpoint:
            raise dedupe.EvidenceError("fixture API failure")
        if endpoint not in self.logs:
            raise dedupe.EvidenceError("missing checkout log API evidence")
        return self.logs[endpoint]

    def get(self, endpoint):
        self.calls.append(endpoint)
        if self.fail and self.fail in endpoint:
            raise dedupe.EvidenceError("fixture API failure")
        url = urlsplit(endpoint)
        path = url.path.removeprefix("repos/" + REPO + "/")
        query = parse_qs(url.query)
        if path.startswith("commits/"):
            return deepcopy(self.commits[AFTER if path == "commits/master" else path.split("/")[1]])
        if path.startswith("pulls/"):
            return deepcopy(self.originals[int(path.split("/")[1])])
        if path == "pulls":
            assert query["state"] == ["closed"] and query["base"] == ["master"]
            rows, key = self.prs, None
        elif path == "actions/workflows/ci.yml/runs":
            assert query["event"] == ["pull_request"]
            requested_head = query["head_sha"][0]
            if self.head_runs is None:
                assert requested_head == HEAD
                rows = self.runs
            else:
                rows = self.head_runs[requested_head]
            key = "workflow_runs"
        elif path.startswith("actions/runs/"):
            parts = path.split("/")
            run_id = int(parts[2])
            if len(parts) == 3:
                return deepcopy(self.details[run_id])
            assert parts[3] == "attempts" and parts[5] == "jobs"
            rows, key = self.attempts[(run_id, int(parts[4]))], "jobs"
        else:
            raise AssertionError("unexpected API endpoint: " + endpoint)
        size, page = int(query["per_page"][0]), int(query["page"][0])
        batch = deepcopy(rows[(page - 1) * size:page * size])
        return {key: batch, "total_count": len(rows)} if key else batch


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.api = FixtureApi()

    def inspect(self, before=BEFORE):
        return dedupe.Inspector(self.api, REPO).inspect(AFTER, before)

    def fallback(self, reason=None):
        result = self.inspect()
        self.assertIs(result["dedupe"], False, result)
        self.assertEqual(result["target_url"], "")
        if reason:
            self.assertIn(reason, result["reason"])
        return result

    def test_exact_queue_proof(self):
        result = self.inspect()
        self.assertIs(result["dedupe"], True, result)
        self.assertEqual(result["target_url"], f"https://github.com/{REPO}/actions/runs/7")
        self.assertEqual(result["queue_head_sha"], HEAD)
        self.assertEqual(result["tree"], TREE)
        self.assertEqual(result["range"][0]["original_pr"], 42)
        self.assertEqual(result["range"][0]["merged_by_id"], dedupe.MERGIFY_ID)
        self.assertEqual(result["range"][0]["committer_id"], 19864447)
        self.assertEqual(dedupe.REQUIRED_JOBS, EXPECTED_JOBS)
        self.assertEqual(result["candidates"][0]["checkout_evidence"], "three matrix checkout logs")
        self.assertEqual({c["sha"] for c in result["candidates"][0]["checkouts"]}, {CHECKED})
        self.assertTrue(any("/attempts/1/jobs?" in call for call in self.api.calls))
        self.assertFalse(any("check-runs" in call for call in self.api.calls))

    def test_direct_push_spoofed_email_or_login_not_identity(self):
        self.api.commits[AFTER]["committer"] = {"id": 1, "login": "mergify[bot]"}
        self.api.commits[AFTER]["commit"]["committer"]["email"] = "37929162+mergify[bot]@users.noreply.github.com"
        self.api.originals[42]["merged_by"] = {"id": 1, "login": "mergify[bot]"}
        result = self.fallback("not merged by Mergify")
        self.assertEqual(result["range"][0]["committer_id"], 1)
        self.assertEqual(len(self.api.calls), 2)

    def test_merge_shape_and_conventional_pr_title(self):
        for field, value in [("parents", [{"sha": BEFORE}]),
                             ("commit", {"message": "Merge pull request #42", "tree": {"sha": TREE},
                                         "committer": {"date": COMMITTED}})]:
            with self.subTest(field=field):
                self.api = FixtureApi()
                self.api.commits[AFTER][field] = value
                self.fallback()
        for subject in ["fix: repair pane", "random: repair pane (#42)", "fix: (#42)"]:
            with self.subTest(subject=subject):
                self.api = FixtureApi()
                self.api.commits[AFTER]["commit"]["message"] = subject
                self.fallback("subject")

    def test_original_pr_metadata_blocks_similar_direct_merge(self):
        for field, value in [("merged", False), ("merge_commit_sha", HEAD),
                             ("title", "fix: other pane"), ("merged_at", None),
                             ("number", 43), ("state", "open"),
                             ("base", {"ref": "windows", "repo": {"full_name": REPO}}),
                             ("base", {"ref": "master", "repo": {"full_name": "fork/herdr"}})]:
            with self.subTest(field=field):
                self.api = FixtureApi()
                self.api.originals[42][field] = value
                self.fallback("original PR")

    def test_merged_by_identity_not_commit_identity(self):
        for identity in [{"id": 1}, None, {}, {"id": "37929162"}]:
            with self.subTest(merged_by=identity):
                self.api = FixtureApi()
                self.api.originals[42]["merged_by"] = identity
                self.fallback("not merged by Mergify")
        self.api = FixtureApi()
        del self.api.originals[42]["merged_by"]
        self.fallback("not merged by Mergify")
        self.api = FixtureApi()
        self.api.commits[AFTER]["committer"] = None
        self.assertTrue(self.inspect()["dedupe"])  # Exact PR API association is authority.

    def test_actual_merge_time_not_queue_commit_clock_is_fence(self):
        self.api.commits[AFTER]["commit"]["committer"]["date"] = "2026-10-03T11:10:00Z"
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(result["merged_at"], COMMITTED)
        self.api.originals[42]["merged_at"] = "2026-10-03T11:25:00Z"
        self.fallback("before original PR merge")
        self.api.originals[42]["merged_at"] = COMMITTED
        self.api.commits[AFTER]["commit"]["committer"]["date"] = "2026-10-03T13:00:00Z"
        self.api.run["updated_at"] = "2026-10-03T12:00:01Z"
        self.fallback("before original PR merge")

    def test_missing_tree_proof(self):
        self.api.commits[HEAD]["commit"]["tree"]["sha"] = BEFORE
        result = self.fallback("no matching")
        self.assertEqual(result["candidates"][0]["reason"], "queue head tree mismatch")

    def test_nonqueue_and_spoof_candidates(self):
        for mutation in [lambda p: p.update(user={"id": 1}),
                         lambda p: p.update(draft=False), lambda p: p.update(state="open"),
                         lambda p: p["head"].update(ref="feature/mergify"),
                         lambda p: p["head"].update(repo=None),
                         lambda p: p["head"].update(repo={"id": 101, "full_name": "fork/herdr"}),
                         lambda p: p["base"].update(ref="windows")]:
            self.api = FixtureApi()
            mutation(self.api.queue)
            self.fallback("no matching")

    def test_no_run_and_no_snapshot(self):
        self.api.runs = []
        self.fallback("no queue CI")
        self.api = FixtureApi()
        self.api.run["pull_requests"] = []
        self.api.logs.clear()
        result = self.fallback("checkout log")
        self.assertEqual(result["candidates"][0]["runs"][0]["pull_requests"], [])

    def test_snapshot_exact_queue_head_and_base(self):
        for mutation in [lambda p: p.update(number=42),
                         lambda p: p["head"].update(sha=AFTER),
                         lambda p: p["head"].update(ref="feature"),
                         lambda p: p["head"].update(repo={"id": 101}),
                         lambda p: p["base"].update(ref="windows"),
                         lambda p: p["base"].update(repo={"id": 101})]:
            self.api = FixtureApi()
            mutation(self.api.run["pull_requests"][0])
            self.fallback("snapshot")

    def test_failed_cancelled_pending_or_late_run(self):
        for status, conclusion in [("completed", "failure"), ("completed", "cancelled"),
                                   ("in_progress", None), ("completed", "skipped")]:
            with self.subTest(conclusion=conclusion):
                self.api = FixtureApi()
                self.api.run.update(status=status, conclusion=conclusion)
                self.fallback("not successful")
        self.api = FixtureApi()
        self.api.run["updated_at"] = "2026-10-03T12:00:01Z"
        self.fallback("before original PR merge")

    def test_latest_run_failure_overrides_old_success(self):
        newer = deepcopy(self.api.run)
        newer.update(id=8, created_at="2026-10-03T11:05:00Z", conclusion="failure")
        self.api.runs.append(newer)
        self.api.details[8] = newer
        self.fallback("latest queue CI")
        self.assertTrue(any(call.endswith("actions/runs/8") for call in self.api.calls))

    def test_retry_uses_latest_attempt_not_old_jobs(self):
        self.api.run["run_attempt"] = 2
        jobs = deepcopy(self.api.jobs)
        for job in jobs:
            job["run_attempt"] = 2
        self.api.attempts[(7, 2)] = jobs
        self.assertTrue(self.inspect()["dedupe"])
        self.assertFalse(any("/attempts/1/jobs?" in call for call in self.api.calls))
        jobs[0]["conclusion"] = "cancelled"
        self.fallback("unsuccessful")
        jobs[0]["conclusion"] = "success"
        jobs[0]["run_attempt"] = 1
        self.fallback("latest exact")

    def test_jobs_same_run_and_head_required(self):
        for field, value in [("run_id", 9), ("head_sha", AFTER), ("run_attempt", 2),
                             ("status", "queued"), ("conclusion", "skipped"),
                             ("completed_at", "2026-10-03T12:00:01Z")]:
            with self.subTest(field=field):
                self.api = FixtureApi()
                self.api.jobs[0][field] = value
                self.fallback()

    def test_required_job_missing_duplicate_or_conflicting_failure(self):
        for name in sorted(EXPECTED_JOBS):
            with self.subTest(missing=name):
                self.api = FixtureApi()
                self.api.jobs[:] = [j for j in self.api.jobs if j["name"] != name]
                self.fallback("missing")
        self.api = FixtureApi()
        self.api.jobs.append(deepcopy(self.api.jobs[0]))
        self.fallback("duplicate")
        self.api = FixtureApi()
        extra = deepcopy(self.api.jobs[0])
        extra.update(name="other", conclusion="failure")
        self.api.jobs.append(extra)
        self.fallback("conflicting")

    def test_wrong_workflow_or_repository_not_a_trusted_producer(self):
        for field, value in [("path", ".github/workflows/spoof.yml"), ("event", "push"),
                             ("repository", {"full_name": "fork/herdr"}),
                             ("head_repository", {"full_name": "fork/herdr"}),
                             ("head_sha", AFTER), ("head_branch", "feature")]:
            with self.subTest(field=field):
                self.api = FixtureApi()
                self.api.run[field] = value
                self.fallback("workflow/repository/head")

    def test_base_advanced_synthetic_tree_cannot_reuse_head_tree(self):
        self.api.run["pull_requests"] = []
        self.api.commits[CHECKED]["commit"]["tree"]["sha"] = BEFORE
        self.fallback("tested checkout tree mismatch")
        self.api.commits[CHECKED]["commit"]["tree"]["sha"] = TREE
        self.api.commits[CHECKED]["parents"] = [{"sha": BEFORE}, {"sha": AFTER}]
        self.fallback("not the queue head or its synthetic merge")

    def test_current_pr_base_does_not_override_exact_checkout_tree(self):
        self.api.queue["base"]["sha"] = AFTER
        self.api.run["pull_requests"][0]["base"]["sha"] = AFTER
        self.assertTrue(self.inspect()["dedupe"])
        self.api.run["pull_requests"] = []
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(len(result["candidates"][0]["checkouts"]), 3)
        self.assertFalse(any("compare/" in call for call in self.api.calls))

    def test_api_errors_and_missing_shapes_fall_back(self):
        for endpoint in ["commits/", "pulls/42", "pulls?", "workflows/ci.yml/runs",
                         "actions/runs/7", "/actions/jobs/", "/attempts/1/jobs"]:
            self.api = FixtureApi()
            self.api.fail = endpoint
            self.fallback("API failure")
        self.api = FixtureApi()
        del self.api.run["pull_requests"]
        self.fallback("missing/malformed")

    def test_full_first_parent_push_range(self):
        intermediate = f"{5:040x}"
        self.api.commits[intermediate] = commit(intermediate, (BEFORE, HEAD), "ci: tighten jobs (#43)")
        self.api.originals[43] = deepcopy(self.api.originals[42])
        self.api.originals[43].update(number=43, title="ci: tighten jobs", merge_commit_sha=intermediate)
        self.api.commits[AFTER]["parents"][0]["sha"] = intermediate
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual([row["sha"] for row in result["range"]], [AFTER, intermediate])
        self.api.originals[43]["merged_by"]["id"] = 1
        self.fallback("not merged by Mergify")
        self.api.originals[43]["merged_by"]["id"] = dedupe.MERGIFY_ID
        self.api.commits[intermediate]["commit"]["message"] = "invalid title (#43)"
        self.fallback("subject")

    def test_before_must_be_first_parent_ancestor_and_bounds_fail_closed(self):
        result = self.inspect(before=HEAD)  # The second parent is not a push-range boundary.
        self.assertFalse(result["dedupe"])
        self.assertFalse(self.inspect(before="0" * 40)["dedupe"])
        self.assertFalse(self.inspect(before=AFTER)["dedupe"])
        with patch.object(dedupe, "MAX_HISTORY", 1):
            self.fallback("history cap")

    def test_paging_and_cache(self):
        with patch.object(dedupe, "PAGE_SIZE", 2):
            inspector = dedupe.Inspector(self.api, REPO)
            self.assertTrue(inspector.inspect(AFTER, BEFORE)["dedupe"])
            calls = list(self.api.calls)
            self.assertTrue(inspector.inspect(AFTER, BEFORE)["dedupe"])
            self.assertEqual(self.api.calls, calls)
            self.assertTrue(any("jobs?per_page=2&page=3" in call for call in calls))

    def test_pagination_cap_or_incomplete_total_never_uses_partial_success(self):
        with patch.object(dedupe, "PAGE_SIZE", 1), patch.object(dedupe, "MAX_PAGES", 1):
            self.fallback("pagination cap")
        original = self.api.get
        def truncated(endpoint):
            payload = original(endpoint)
            if "/jobs?" in endpoint:
                payload["total_count"] += 1
            return payload
        self.api.get = truncated
        self.fallback("incomplete")

    def test_old_closed_pr_boundary_avoids_unbounded_history(self):
        old = deepcopy(self.api.queue)
        old["updated_at"] = "2026-09-01T12:00:00Z"
        self.api.prs.append(old)
        with patch.object(dedupe, "PAGE_SIZE", 2), patch.object(dedupe, "MAX_PAGES", 5):
            self.assertTrue(self.inspect()["dedupe"])
            self.assertFalse(any("pulls?" in c and parse_qs(urlsplit(c).query)["page"] == ["2"]
                                 for c in self.api.calls))

    def test_distinct_queue_candidate_does_not_poison_exact_success(self):
        second = deepcopy(self.api.queue)
        second["number"] = 101
        self.api.prs.append(second)
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(result["queue_pr"], 100)
        self.assertIn("snapshot", result["candidates"][1]["reason"])

    def test_older_distinct_cancelled_queue_head_does_not_poison_newer_success(self):
        old_head = f"{9:040x}"
        old_pr = deepcopy(self.api.queue)
        old_pr.update(number=99, created_at="2026-10-03T10:00:00Z")
        old_pr["head"].update(sha=old_head, ref="mergify/merge-queue/old")
        self.api.prs.append(old_pr)
        self.api.commits[old_head] = commit(old_head, (BEFORE,))
        old_run = deepcopy(self.api.run)
        old_run.update(id=6, head_sha=old_head, head_branch=old_pr["head"]["ref"],
                       created_at="2026-10-03T10:00:00Z", conclusion="cancelled")
        self.api.details[6] = old_run
        self.api.head_runs = {HEAD: self.api.runs, old_head: [old_run]}
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(result["queue_pr"], 100)
        self.assertIn("not successful", result["candidates"][1]["reason"])

    def test_future_queue_draft_cannot_poison_premerge_proof(self):
        future = deepcopy(self.api.queue)
        future["created_at"] = "2026-10-03T12:00:01Z"
        future["head"]["sha"] = f"{9:040x}"
        self.api.prs.insert(0, future)
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(len(result["candidates"]), 1)
        self.assertFalse(any(c.endswith("commits/" + future["head"]["sha"]) for c in self.api.calls))

    def test_all_three_matrix_logs_required_and_no_raw_log_output(self):
        result = self.inspect()
        self.assertNotIn("[command]", json.dumps(result))
        for endpoint in list(self.api.logs):
            with self.subTest(missing=endpoint):
                saved = self.api.logs.pop(endpoint)
                self.fallback("checkout log")
                self.api.logs[endpoint] = saved
        endpoint = next(iter(self.api.logs))
        self.api.logs[endpoint] = checkout_log(sha=HEAD)
        self.fallback("same exact commit")

    def test_populated_snapshot_never_bypasses_checkout_evidence(self):
        self.api.logs = {endpoint: "unanchored SHA " + CHECKED for endpoint in self.api.logs}
        self.fallback("pinned checkout log")

    def test_last_ten_walks_first_parents_and_keeps_failure_diagnostics(self):
        # History contains ordinary commits between merges and unvisited second parents.
        merge_shas = [f"{n:040x}" for n in range(20, 30)]
        for index, sha in enumerate(merge_shas):
            direct = f"{100 + index:040x}"
            next_sha = merge_shas[index + 1] if index + 1 < len(merge_shas) else BEFORE
            self.api.commits[sha] = commit(sha, (direct, HEAD))
            self.api.commits[sha]["committer"]["id"] = 1
            self.api.commits[direct] = commit(direct, (next_sha,))
        report = dedupe.Inspector(self.api, REPO).last(merge_shas[0], 10)
        self.assertEqual([r["sha"] for r in report["results"]], merge_shas)
        self.assertEqual(report["results"][0]["range"][0]["committer_id"], 1)
        self.assertEqual(report["sha"], merge_shas[0])
        self.assertFalse(any(call.endswith("commits/" + HEAD) for call in self.api.calls))


class LogParserTests(unittest.TestCase):
    def test_observed_unix_shapes_and_quoted_windows_git_exe(self):
        observed_sha = "80db28d5093d771d26f2efc8d38baa7a30fd8f69"
        self.assertEqual(dedupe.CHECKOUT_PIN, "df4cb1c069e1874edd31b4311f1884172cec0e10")
        for git in ["/usr/bin/git", "/opt/homebrew/bin/git", '"C:\\Program Files\\Git\\bin\\git.exe"']:
            self.assertEqual(dedupe.checkout_sha(checkout_log(observed_sha, git)), observed_sha)

    def test_spoof_later_commands_bare_hash_and_ambiguous_commands_rejected(self):
        good = checkout_log()
        cases = [CHECKED, good.replace(dedupe.CHECKOUT_PIN, "v6"),
                 good.replace("[command]/usr/bin/git log -1 --format=%H", "fake command"),
                 good.replace(CHECKED, "prefix " + CHECKED),
                 good.replace("[command]/usr/bin/git", "##[group]Run malicious\n[command]/usr/bin/git"),
                 good.replace("##[group]Run test command", "[command]/usr/bin/git log -1 --format=%H\n" + CHECKED)]
        for log in cases:
            with self.subTest(log=log[:30]), self.assertRaises(dedupe.EvidenceError):
                dedupe.checkout_sha(log)


class CliTests(unittest.TestCase):
    def cli(self, api, *args):
        buffer = io.StringIO()
        with redirect_stdout(buffer):
            code = dedupe.main(["--repo", REPO, *args], api=api)
        self.assertEqual(code, 0)
        return json.loads(buffer.getvalue())

    def test_json_github_output_and_readonly_dry_run(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            api = FixtureApi()
            api.run["pull_requests"] = []  # Actual closed queue run shape.
            result = self.cli(api, "--sha", AFTER, "--before", BEFORE, "--github-output", str(output))
            self.assertTrue(result["dedupe"])
            self.assertEqual(output.read_text(), f"dedupe=true\ntarget_url=https://github.com/{REPO}/actions/runs/7\n")
            output.unlink()
            with patch.dict("os.environ", {"GITHUB_OUTPUT": str(output)}):
                self.cli(api, "--sha", AFTER, "--before", BEFORE,
                         "--dry-run", "--github-output", str(output))
            self.assertFalse(output.exists())
            api.fail = "commits/"
            with patch.dict("os.environ", {"GITHUB_OUTPUT": str(output)}):
                result = self.cli(api, "--sha", AFTER, "--before", BEFORE)
            self.assertFalse(result["dedupe"])
            self.assertEqual(output.read_text(), "dedupe=false\ntarget_url=\n")

    def test_real_cli_initial_push_fallback_without_api(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            command = [sys.executable, str(Path(dedupe.__file__)), "--repo", REPO,
                       "--sha", AFTER, "--before", "0" * 40, "--github-output", str(output)]
            dry_run = subprocess.run([*command, "--dry-run"], text=True, capture_output=True,
                                     check=True, timeout=10)
            self.assertFalse(json.loads(dry_run.stdout)["dedupe"])
            self.assertFalse(output.exists())
            fallback = subprocess.run(command, text=True, capture_output=True, check=True, timeout=10)
            self.assertEqual(json.loads(fallback.stdout)["reason"], "invalid/initial push before SHA")
            self.assertEqual(output.read_text(), "dedupe=false\ntarget_url=\n")

    def test_last_is_inspection_only_even_with_output_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            with patch.dict("os.environ", {"GITHUB_OUTPUT": str(output)}):
                self.cli(FixtureApi(), "--last", "10", "--dry-run")
            self.assertFalse(output.exists())

    def test_cumulative_deadline_falls_back_without_get_and_cached_reads_remain(self):
        with patch.object(dedupe.time, "monotonic", return_value=10), patch.object(subprocess, "run") as run:
            api = dedupe.GhApi()
            api.deadline = 9
            result = self.cli(api, "--sha", AFTER, "--before", BEFORE, "--dry-run")
            self.assertFalse(result["dedupe"])
            self.assertIn("deadline exhausted", result["reason"])
            run.assert_not_called()
            api.cache["cached"] = {"success": True}
            self.assertEqual(api.get("cached"), {"success": True})
            run.assert_not_called()
            api.deadline = 15
            run.return_value = subprocess.CompletedProcess([], 0, "{}", "")
            self.assertEqual(api.get("new"), {})
            self.assertEqual(run.call_args.kwargs["timeout"], 5)

    def test_last_and_push_choose_separate_finite_api_budgets(self):
        for flags, expected in [([], 120), (["--last", "10"], 600)]:
            with self.subTest(mode=flags), patch.object(dedupe, "GhApi", return_value=FixtureApi()) as api:
                args = flags or ["--sha", AFTER, "--before", BEFORE]
                with redirect_stdout(io.StringIO()):
                    dedupe.main(["--repo", REPO, "--dry-run", *args])
                api.assert_called_once_with(budget_seconds=expected)

    def test_log_get_is_bounded_cached_deadlined_and_does_not_leak(self):
        with patch.object(subprocess, "Popen") as popen, patch.object(dedupe.threading, "Timer"):
            process = popen.return_value.__enter__.return_value
            process.stdout = io.BytesIO(b"safe log")
            process.wait.return_value = 0
            api = dedupe.GhApi()
            self.assertEqual(api.text("logs"), "safe log")
            api.deadline = 0
            self.assertEqual(api.text("logs"), "safe log")
            popen.assert_called_once()
            self.assertEqual(popen.call_args.args[0][:4], ["gh", "api", "--method", "GET"])
            with self.assertRaises(dedupe.EvidenceError):
                api.text("another")
            popen.assert_called_once()
            api = dedupe.GhApi()
            process.stdout = io.BytesIO(b"SECRET_TOKEN" * 10)
            with patch.object(dedupe, "MAX_LOG_BYTES", 8), self.assertRaises(dedupe.EvidenceError) as error:
                api.text("too large")
            self.assertNotIn("SECRET_TOKEN", str(error.exception))
            process.kill.assert_called_once()

    def test_gh_adapter_get_only_cached_and_errors_do_not_leak(self):
        with patch.object(subprocess, "run") as run:
            run.return_value = subprocess.CompletedProcess([], 0, '{"sha":"ok"}', "")
            api = dedupe.GhApi()
            self.assertEqual(api.get("repos/example/herdr/commits/master"), {"sha": "ok"})
            api.get("repos/example/herdr/commits/master")
            run.assert_called_once()
            command = run.call_args.args[0]
            self.assertEqual(command[:4], ["gh", "api", "--method", "GET"])
            self.assertNotIn("POST", command)
            run.side_effect = subprocess.CalledProcessError(1, command, stderr="SECRET_TOKEN")
            with self.assertRaises(dedupe.EvidenceError) as error:
                api.get("repos/example/herdr/pulls")
            self.assertNotIn("SECRET_TOKEN", str(error.exception))
            run.side_effect = subprocess.TimeoutExpired(command, 30, stderr="SECRET_TOKEN")
            with self.assertRaises(dedupe.EvidenceError):
                api.get("repos/example/herdr/pulls")
            run.side_effect = None
            run.return_value = subprocess.CompletedProcess([], 0, "not JSON SECRET_TOKEN", "")
            with self.assertRaises(dedupe.EvidenceError) as error:
                api.get("repos/example/herdr/pulls")
            self.assertNotIn("SECRET_TOKEN", str(error.exception))


if __name__ == "__main__":
    unittest.main()
