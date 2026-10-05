"""Behavioral fixtures for GET/CLI evidence; HTTP probes use loopback only."""
from contextlib import contextmanager, redirect_stdout
from copy import deepcopy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

from scripts import ci_plan
from scripts import ci_queue_dedupe as dedupe

REPO = "example/herdr"
REPO_ID = 100
BEFORE, AFTER, HEAD, TREE = (f"{n:040x}" for n in range(1, 5))
CHECKED = f"{8:040x}"
COMMITTED = "2026-10-03T12:00:00Z"
STARTED = "2026-10-03T11:00:00Z"
FINISHED = "2026-10-03T11:30:00Z"
EXPECTED_JOBS = {"smarty-ci", "check (ubuntu-latest)",
                 "check (windows-latest)", "conventional-commits"}
CONPTY_JOB = "windows-conpty-package"
FAKE_TOKEN = "fake-job-log-token"
REAL_POPEN = subprocess.Popen


@contextmanager
def local_log_server(routes):
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            requests.append((self.path, self.headers.get("Authorization")))
            response = routes.get(urlsplit(self.path).path, (404, b"not found", {}))
            if callable(response):
                response = response(self)
            status, body, headers = response
            try:
                self.send_response(status)
                for key, value in headers.items():
                    self.send_header(key, value)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass  # Expected when curl's byte/deadline guard stops a read.

        def log_message(self, *args):
            pass  # Never print HTTP paths (including fake signed query strings).

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01})
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", requests
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


@contextmanager
def log_transport(base_url=None, scripts=None):
    """Keep the real Popen/pipe path; redirect curl to loopback or stub a CLI."""
    commands = []

    def start(command, **kwargs):
        commands.append(command)
        method = command[0]
        if scripts and method in scripts:
            rewritten = [sys.executable, "-c", scripts[method]]
        else:
            if method != "curl" or base_url is None:
                raise AssertionError("unexpected subprocess: " + method)
            rewritten = list(command)
            rewritten[-1] = base_url + urlsplit(command[-1]).path
            # Production permits HTTPS only; this test override permits loopback.
            rewritten = ["=http,https" if arg == "=https" else arg for arg in rewritten]
            rewritten.extend(["--noproxy", "*"])
        return REAL_POPEN(rewritten, **kwargs)

    with patch.dict(os.environ, {"GH_TOKEN": FAKE_TOKEN}), patch.object(subprocess, "Popen", side_effect=start):
        yield commands


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
                     for index, name in enumerate(sorted(EXPECTED_JOBS | {CONPTY_JOB}))]
        self.attempts = {(7, 1): self.jobs}
        self.logs = {f"repos/{REPO}/actions/jobs/{job['id']}/logs": checkout_log()
                     for job in self.jobs if job["name"].startswith("check (") or job["name"] == CONPTY_JOB}
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
        self.assertEqual(result["candidates"][0]["checkout_evidence"], "3 selected build checkout logs")
        planned = {f"check ({lane['os']})" for lane in
                   ci_plan.check_matrix("pull_request", {"pull_request": self.api.queue})["include"]}
        self.assertEqual(planned, {"check (ubuntu-latest)", "check (windows-latest)"})
        self.assertEqual({c["job"] for c in result["candidates"][0]["checkouts"]}, planned | {CONPTY_JOB})
        self.assertEqual({c["sha"] for c in result["candidates"][0]["checkouts"]}, {CHECKED})
        self.assertTrue(any("/attempts/1/jobs?" in call for call in self.api.calls))
        self.assertFalse(any("check-runs" in call for call in self.api.calls))

    def test_two_lane_queue_with_explicitly_skipped_conpty_is_reusable(self):
        package = next(job for job in self.api.jobs if job["name"] == CONPTY_JOB)
        package.update(conclusion="skipped", started_at=None, completed_at=None)
        del self.api.logs[f"repos/{REPO}/actions/jobs/{package['id']}/logs"]
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(len(result["candidates"][0]["checkouts"]), 2)
        self.assertEqual(result["candidates"][0]["checkout_evidence"], "2 selected build checkout logs")
        self.assertFalse(any(f"/jobs/{package['id']}/logs" in call for call in self.api.calls))
        self.api.jobs.remove(package)
        self.fallback("missing or duplicate required job: " + CONPTY_JOB)

    def test_conpty_neutral_is_not_selection_evidence(self):
        package = next(job for job in self.api.jobs if job["name"] == CONPTY_JOB)
        package["conclusion"] = "neutral"
        self.fallback("neither successful nor explicitly skipped")

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
        for name in sorted(EXPECTED_JOBS | {CONPTY_JOB}):
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

    def test_distinct_queue_candidate_after_success_is_not_traversed(self):
        second = deepcopy(self.api.queue)
        second["number"] = 101
        second["head"]["sha"] = f"{9:040x}"
        self.api.fail = "commits/" + second["head"]["sha"]
        self.api.prs.append(second)
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(result["queue_pr"], 100)
        self.assertEqual(len(result["candidates"]), 1)
        self.assertFalse(any(self.api.fail in call for call in self.api.calls))

    def test_older_distinct_cancelled_queue_head_does_not_poison_newer_success(self):
        old_head = f"{9:040x}"
        old_pr = deepcopy(self.api.queue)
        old_pr.update(number=99, created_at="2026-10-03T10:00:00Z")
        old_pr["head"].update(sha=old_head, ref="mergify/merge-queue/old")
        self.api.prs = [old_pr, self.api.queue]
        self.api.commits[old_head] = commit(old_head, (BEFORE,))
        old_run = deepcopy(self.api.run)
        old_run.update(id=6, head_sha=old_head, head_branch=old_pr["head"]["ref"],
                       created_at="2026-10-03T10:00:00Z", conclusion="cancelled")
        self.api.details[6] = old_run
        self.api.head_runs = {HEAD: self.api.runs, old_head: [old_run]}
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(result["queue_pr"], 100)
        self.assertIn("not successful", result["candidates"][0]["reason"])

    def crowded_queue(self):
        """Thirty speculative trees, twenty dequeues, then three invalid requeues."""
        candidates = []
        for index in range(53):
            number = 1000 + index
            head_sha = f"{1000 + index:040x}"
            candidate = deepcopy(self.api.queue)
            candidate.update(number=number)
            candidate["head"].update(
                sha=head_sha,
                ref=f"mergify/merge-queue/dequeued-{index}/pr-{number}",
            )
            self.api.commits[head_sha] = commit(
                head_sha, (BEFORE,),
                tree=TREE if index >= 30 and index != 51 else f"{2000 + index:040x}"
            )
            candidates.append(candidate)
            if index < 30 or index == 51:
                continue  # speculative heads never tested this tree.
            run_id = 100 + index
            run = deepcopy(self.api.run)
            run.update(
                id=run_id,
                head_sha=head_sha,
                head_branch=candidate["head"]["ref"],
                created_at="2026-10-03T10:00:00Z",
                updated_at="2026-10-03T11:00:00Z",
                conclusion="failure" if index == 52 else "success" if index == 50 else "cancelled",
            )
            run["pull_requests"] = [{"number": number, "head": deepcopy(candidate["head"]),
                                      "base": deepcopy(candidate["base"])}]
            self.api.details[run_id] = run
            jobs = deepcopy(self.api.jobs)
            for job in jobs:
                job["run_id"] = run_id
                job["head_sha"] = head_sha
            if index == 50:  # Missing required lane remains a hard failure past the old cap.
                jobs = [job for job in jobs if job["name"] != "check (windows-latest)"]
            self.api.attempts[(run_id, 1)] = jobs
            self.api.head_runs = self.api.head_runs or {}
            self.api.head_runs[head_sha] = [run]
        self.api.prs = [*candidates, self.api.queue]
        self.api.head_runs[HEAD] = self.api.runs

    def test_queue_candidates_beyond_fifty_keep_fences_and_find_late_success(self):
        self.crowded_queue()
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(result["queue_pr"], 100)
        self.assertEqual(len(result["candidates"]), 54)
        self.assertEqual(result["candidates"][50]["reason"],
                         "missing or duplicate required job: check (windows-latest)")
        self.assertEqual(result["candidates"][51]["reason"], "queue head tree mismatch")
        self.assertIn("not successful", result["candidates"][52]["reason"])
        self.assertEqual(len(result["candidates"][-1]["checkouts"]), 3)

    def test_queue_candidates_beyond_fifty_without_complete_proof_fall_back(self):
        for case in ["wrong tree", "latest failure", "missing lane"]:
            with self.subTest(case=case):
                self.api = FixtureApi()
                self.crowded_queue()
                if case == "wrong tree":
                    self.api.commits[HEAD]["commit"]["tree"]["sha"] = BEFORE
                    expected = "queue head tree mismatch"
                elif case == "latest failure":
                    newer = deepcopy(self.api.run)
                    newer.update(id=8, created_at="2026-10-03T11:05:00Z", conclusion="failure")
                    self.api.runs.append(newer)
                    self.api.details[8] = newer
                    expected = "latest queue CI run is not successful"
                else:
                    self.api.jobs[:] = [job for job in self.api.jobs
                                        if job["name"] != "check (ubuntu-latest)"]
                    expected = "missing or duplicate required job: check (ubuntu-latest)"
                result = self.fallback()
                self.assertEqual(len(result["candidates"]), 54)
                self.assertEqual(result["candidates"][-1]["reason"], expected)

    def test_future_queue_draft_cannot_poison_premerge_proof(self):
        future = deepcopy(self.api.queue)
        future["created_at"] = "2026-10-03T12:00:01Z"
        future["head"]["sha"] = f"{9:040x}"
        self.api.prs.insert(0, future)
        result = self.inspect()
        self.assertTrue(result["dedupe"], result)
        self.assertEqual(len(result["candidates"]), 1)
        self.assertFalse(any(c.endswith("commits/" + future["head"]["sha"]) for c in self.api.calls))

    def test_all_selected_build_logs_required_and_no_raw_log_output(self):
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
        for flags, expected in [([], 120), (["--last", "1"], 120), (["--last", "10"], 600)]:
            with self.subTest(mode=flags), patch.object(dedupe, "GhApi", return_value=FixtureApi()) as api:
                args = flags or ["--sha", AFTER, "--before", BEFORE]
                with redirect_stdout(io.StringIO()):
                    dedupe.main(["--repo", REPO, "--dry-run", *args])
                api.assert_called_once_with(budget_seconds=expected, compare_log_fetch=False)

    def test_last_one_returns_exact_proof_without_writing_output(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            result = self.cli(FixtureApi(), "--last", "1", "--dry-run", "--github-output", str(output))
            self.assertEqual(len(result["results"]), 1)
            self.assertTrue(result["results"][0]["dedupe"])
            self.assertFalse(output.exists())

    def test_comparison_requires_dry_run_before_any_api_call(self):
        api = FixtureApi()
        with redirect_stdout(io.StringIO()), patch("sys.stderr", new=io.StringIO()):
            with self.assertRaises(SystemExit) as error:
                dedupe.main(["--repo", REPO, "--last", "1", "--compare-log-fetch"], api=api)
        self.assertEqual(error.exception.code, 2)
        self.assertEqual(api.calls, [])

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


@unittest.skipUnless(shutil.which("curl"), "curl is needed for local HTTP probes")
# ponytail: the job-log fetch only ever runs in the ubuntu `plan` job, with the runner's
# curl; Windows curl reports its write-out differently, so these real-curl tests are Unix-only.
@unittest.skipIf(os.name == "nt", "the queue-dedupe log fetch runs only on the ubuntu CI runner")
class LogFetchTests(unittest.TestCase):
    def test_real_http_failure_retains_status_and_first_stderr_not_body(self):
        for status in [401, 403, 404, 500]:
            with self.subTest(status=status), local_log_server({"/logs": (status, b"PRIVATE_LOG_BODY", {})}) as (url, requests):
                with log_transport(url) as commands:
                    api = dedupe.GhApi()
                    with self.assertRaises(dedupe.EvidenceError) as error:
                        api.text("logs")
                    reason = str(error.exception)
                    self.assertIn(f"HTTP {status}", reason)
                    self.assertIn("curl: (22)", reason)
                    self.assertNotIn("PRIVATE_LOG_BODY", reason)
                    self.assertNotIn(FAKE_TOKEN, reason)
                    self.assertNotIn("logs", api.cache)
                    command = commands[0]
                    self.assertEqual(command[:3], ["curl", "--disable", "-fsSL"])
                    self.assertNotIn("--location-trusted", command)
                    self.assertEqual(command[command.index("--proto-redir") + 1], "=https")
                    self.assertNotIn(FAKE_TOKEN, " ".join(command))
                    self.assertEqual(requests[0][1], "Bearer " + FAKE_TOKEN)

    def test_redirect_strips_cross_host_authorization_and_retains_final_status(self):
        with local_log_server({"/signed": (403, b"PRIVATE", {})}) as (target, target_requests):
            signed = target.replace("127.0.0.1", "localhost") + "/signed?sig=PRIVATE_SIGNATURE"
            with local_log_server({"/logs": (302, b"", {"Location": signed})}) as (source, source_requests):
                with log_transport(source), self.assertRaises(dedupe.EvidenceError) as error:
                    dedupe.GhApi().text("logs")
                self.assertIn("HTTP 403", str(error.exception))
                self.assertNotIn("PRIVATE_SIGNATURE", str(error.exception))
                self.assertEqual(source_requests[0][1], "Bearer " + FAKE_TOKEN)
                self.assertIsNone(target_requests[0][1])

    def test_exact_byte_cap_cached_after_deadline_and_uncached_read_rejected(self):
        with local_log_server({"/logs": (200, b"12345678", {}), "/large": (200, b"PRIVATE_LOG_BODY", {})}) as (url, requests):
            with log_transport(url) as commands, patch.object(dedupe, "MAX_LOG_BYTES", 8):
                api = dedupe.GhApi()
                self.assertEqual(api.text("logs"), "12345678")
                with self.assertRaises(dedupe.EvidenceError) as error:
                    api.text("large")
                self.assertIn("byte cap", str(error.exception))
                self.assertNotIn("PRIVATE_LOG_BODY", str(error.exception))
                self.assertNotIn("large", api.cache)
                api.deadline = 0
                self.assertEqual(api.text("logs"), "12345678")
                with self.assertRaises(dedupe.EvidenceError) as error:
                    api.text("uncached")
                self.assertIn("cumulative deadline exhausted", str(error.exception))
                self.assertEqual(len(commands), 2)
                self.assertEqual(len(requests), 2)

    def test_stalled_real_http_request_stops_at_cumulative_deadline(self):
        release = threading.Event()
        def stalled(request):
            release.wait(2)
            return 200, b"late", {}
        try:
            with local_log_server({"/logs": stalled}) as (url, requests), log_transport(url):
                api = dedupe.GhApi(budget_seconds=0.15)
                start = time.monotonic()
                with self.assertRaises(dedupe.EvidenceError) as error:
                    api.text("logs")
                self.assertLess(time.monotonic() - start, 1)
                self.assertIn("deadline exhausted", str(error.exception))
                self.assertNotIn("logs", api.cache)
                release.set()
        finally:
            release.set()

    def test_per_request_30_second_cap_and_budget_remaining_are_not_reset(self):
        real_timer = threading.Timer
        intervals = []
        def bounded_timer(interval, function):
            intervals.append(interval)
            return real_timer(min(interval, 0.05), function)
        script = "import sys, time; sys.stdin.buffer.read(); time.sleep(10)"
        with log_transport(scripts={"curl": script}), patch.object(dedupe.threading, "Timer", side_effect=bounded_timer):
            api = dedupe.GhApi(budget_seconds=120)
            with self.assertRaises(dedupe.EvidenceError):
                api.text("logs")
            self.assertGreater(intervals[0], 29)
            self.assertLessEqual(intervals[0], 30)
            api.deadline = time.monotonic() + 0.02
            with self.assertRaises(dedupe.EvidenceError):
                api.text("second")
            self.assertGreater(intervals[1], 0)
            self.assertLessEqual(intervals[1], 0.02)
            self.assertFalse(api.cache)

    def test_stderr_is_drained_bounded_and_redacted_before_display_truncation(self):
        script = ("import os, sys; sys.stdin.buffer.read(); "
                  "sys.stderr.write('curl error ' + 'a' * 480 + os.environ['GH_TOKEN'] + "
                  "' https://blob.invalid/log?sig=PRIVATE_SIGNATURE\\nSECOND_PRIVATE_LINE\\n' + 'z' * 200000 + "
                  "'\\nHERDR_JOB_LOG_HTTP_STATUS:403\\n'); sys.exit(22)")
        with log_transport(scripts={"curl": script}), self.assertRaises(dedupe.EvidenceError) as error:
            dedupe.GhApi().text("logs")
        reason = str(error.exception)
        self.assertIn("HTTP 403", reason)
        self.assertIn("curl error", reason)
        self.assertNotIn(FAKE_TOKEN, reason)
        self.assertNotIn(FAKE_TOKEN[:5], reason)
        self.assertNotIn("PRIVATE_SIGNATURE", reason)
        self.assertNotIn("SECOND_PRIVATE_LINE", reason)
        self.assertLess(len(reason), 600)
        # Capture boundary through a token: discard the incomplete line, don't
        # expose a partial token that literal redaction could no longer match.
        script = ("import os, sys; sys.stdin.buffer.read(); "
                  "sys.stderr.write('\\n' + 'x' * 4090 + os.environ['GH_TOKEN'] + "
                  "'\\nHERDR_JOB_LOG_HTTP_STATUS:401\\n'); sys.exit(22)")
        with log_transport(scripts={"curl": script}), self.assertRaises(dedupe.EvidenceError) as error:
            dedupe.GhApi().text("logs")
        self.assertIn("HTTP 401", str(error.exception))
        self.assertIn("capture limit", str(error.exception))
        self.assertNotIn(FAKE_TOKEN[:4], str(error.exception))

    def test_invalid_token_no_process_and_spawn_or_decode_failure_not_cached(self):
        for token in ["", "secret\ninjected", "secret\rinjected", "secret\x00injected"]:
            with self.subTest(token=repr(token)), patch.object(os, "environ", {"GH_TOKEN": token}), patch.object(subprocess, "Popen") as popen:
                with self.assertRaises(dedupe.EvidenceError) as error:
                    dedupe.GhApi().text("logs")
                self.assertIn("GH_TOKEN", str(error.exception))
                self.assertNotIn("secret", str(error.exception))
                popen.assert_not_called()
        with patch.dict(os.environ, {"GH_TOKEN": FAKE_TOKEN}), patch.object(subprocess, "Popen", side_effect=OSError(FAKE_TOKEN)):
            with self.assertRaises(dedupe.EvidenceError) as error:
                dedupe.GhApi().text("logs")
            self.assertNotIn(FAKE_TOKEN, str(error.exception))
        with local_log_server({"/logs": (200, b"\xffPRIVATE", {})}) as (url, requests), log_transport(url):
            api = dedupe.GhApi()
            with self.assertRaises(dedupe.EvidenceError) as error:
                api.text("logs")
            self.assertIn("HTTP 200", str(error.exception))
            self.assertNotIn("PRIVATE", str(error.exception))
            self.assertNotIn("logs", api.cache)

    def test_comparison_gh_failure_curl_success_and_both_success_remain_eligible(self):
        fixture = FixtureApi()
        routes = {"/" + endpoint: (200, log.encode(), {}) for endpoint, log in fixture.logs.items()}
        for success in [False, True]:
            script = ("import sys; sys.stdout.write(" + repr("HTTP/2.0 200 OK\r\nLocation: https://blob.invalid?sig=PRIVATE_SIGNATURE\r\n\r\n" + checkout_log()) + ")" if success else
                      "import os, sys; sys.stderr.write('gh: (HTTP 403) ' + os.environ['GH_TOKEN'] + "
                      "' https://blob.invalid?sig=PRIVATE_SIGNATURE\\n'); sys.exit(1)")
            with self.subTest(gh_success=success), local_log_server(routes) as (url, requests):
                with log_transport(url, {"gh": script}) as commands:
                    api = dedupe.GhApi(compare_log_fetch=True)
                    api.get = fixture.get
                    buffer = io.StringIO()
                    with redirect_stdout(buffer):
                        dedupe.main(["--repo", REPO, "--last", "1", "--dry-run", "--compare-log-fetch"], api=api)
                    report = json.loads(buffer.getvalue())
                    self.assertTrue(report["results"][0]["dedupe"], report)
                    comparisons = report["log_fetch_comparison"]
                    self.assertEqual(len(comparisons), 3)
                    for comparison in comparisons:
                        gh, curl = comparison["methods"]
                        self.assertEqual(gh["success"], success)
                        self.assertEqual(gh["method"], "gh")
                        self.assertEqual(gh["http_status"], "200" if success else "403")
                        self.assertEqual(gh["include"], "output-only")
                        self.assertTrue(curl["success"])
                        self.assertEqual(curl["http_status"], "200")
                        if not success:
                            self.assertEqual(gh["http_status"], "403")
                            self.assertIn("gh: (HTTP 403)", gh["first_stderr"])
                    emitted = buffer.getvalue()
                    for private in [FAKE_TOKEN, "PRIVATE_SIGNATURE", "[command]", "https://blob.invalid"]:
                        self.assertNotIn(private, emitted)
                    self.assertEqual(len(commands), 6)
                    for command in commands[::2]:
                        self.assertEqual(command[:4], ["gh", "api", "--method", "GET"])
                        self.assertEqual(command[-1], "--include")
                    api.deadline = 0
                    for endpoint, log in fixture.logs.items():
                        self.assertEqual(api.text(endpoint), log)
                    self.assertEqual(len(commands), 6)

    def test_comparison_failure_never_substitutes_legacy_success_or_caches_failure(self):
        for gh_success in [False, True]:
            script = ("import sys; sys.stdout.write(" + repr(checkout_log()) + ")" if gh_success else
                      "import sys; sys.stderr.write('gh: (HTTP 401) github_pat_PRIVATE\\n'); sys.exit(1)")
            with self.subTest(gh_success=gh_success), local_log_server({"/logs": (403, b"PRIVATE_LOG", {})}) as (url, requests):
                with log_transport(url, {"gh": script}) as commands:
                    api = dedupe.GhApi(compare_log_fetch=True)
                    for _ in range(2):
                        with self.assertRaises(dedupe.EvidenceError) as error:
                            api.text("logs")
                        self.assertIn("HTTP 403", str(error.exception))
                    self.assertNotIn("logs", api.cache)
                    self.assertEqual(len(commands), 4)
                    emitted = json.dumps(api.log_fetch_comparison)
                    self.assertNotIn("github_pat_PRIVATE", emitted)
                    self.assertNotIn("PRIVATE_LOG", emitted)
                    self.assertNotIn("[command]", emitted)
                    self.assertEqual(api.log_fetch_comparison[0]["methods"][0]["success"], gh_success)

    def test_comparison_all_matrix_failures_report_both_methods_without_output_writes(self):
        fixture = FixtureApi()
        routes = {"/" + endpoint: (403, b"PRIVATE_LOG", {}) for endpoint in fixture.logs}
        script = "import sys; sys.stderr.write('gh: failure (HTTP 401)\\n'); sys.exit(1)"
        with tempfile.TemporaryDirectory() as directory, local_log_server(routes) as (url, requests):
            output = Path(directory) / "output"
            with log_transport(url, {"gh": script}) as commands:
                api = dedupe.GhApi(compare_log_fetch=True)
                api.get = fixture.get
                buffer = io.StringIO()
                with redirect_stdout(buffer):
                    dedupe.main(["--repo", REPO, "--last", "1", "--dry-run", "--compare-log-fetch",
                                 "--github-output", str(output)], api=api)
                report = json.loads(buffer.getvalue())
                self.assertFalse(report["results"][0]["dedupe"])
                self.assertIn("HTTP 403", report["results"][0]["reason"])
                self.assertEqual(len(report["log_fetch_comparison"]), 3)
                self.assertEqual(len(commands), 6)
                self.assertFalse(output.exists())
                for comparison in report["log_fetch_comparison"]:
                    gh, curl = comparison["methods"]
                    self.assertFalse(gh["success"] or curl["success"])
                    self.assertEqual((gh["http_status"], curl["http_status"]), ("401", "403"))
                self.assertNotIn("PRIVATE_LOG", buffer.getvalue())

    def test_legacy_include_malformed_utf8_keeps_status_not_headers_or_body(self):
        script = ("import sys; sys.stdout.buffer.write("
                  "b'HTTP/2.0 200 OK\\r\\nLocation: https://blob.invalid?sig=PRIVATE_SIGNATURE\\r\\n\\r\\n\\xffPRIVATE_BODY')")
        with log_transport(scripts={"gh": script}):
            data, diagnostic = dedupe.GhApi().fetch_log("logs", "gh")
        self.assertIsNone(data)
        self.assertFalse(diagnostic["success"])
        self.assertEqual(diagnostic["http_status"], "200")
        self.assertEqual(diagnostic["include"], "output-only")
        self.assertNotIn("PRIVATE", json.dumps(diagnostic))
        self.assertNotIn("Location", json.dumps(diagnostic))

    def test_comparison_shares_deadline_and_does_not_start_curl_after_exhaustion(self):
        script = "import time; time.sleep(10)"
        with log_transport(scripts={"gh": script}) as commands:
            api = dedupe.GhApi(budget_seconds=0.05, compare_log_fetch=True)
            with self.assertRaises(dedupe.EvidenceError) as error:
                api.text("logs")
            self.assertIn("cumulative deadline exhausted", str(error.exception))
            self.assertEqual(len(commands), 1)
            gh, curl = api.log_fetch_comparison[0]["methods"]
            self.assertIn("deadline exhausted", gh["reason"])
            self.assertIn("deadline exhausted", curl["reason"])
            self.assertFalse(gh["success"] or curl["success"])


if __name__ == "__main__":
    unittest.main()
