#!/usr/bin/env python3
"""Read-only, fail-closed evidence for reusing Mergify queue CI; never publishes.

Push plan:
  python3 scripts/ci_queue_dedupe.py --repo OWNER/REPO --sha AFTER --before BEFORE
BEFORE is the push event's previous master tip, exclusive (not AFTER's second
parent). Every first-parent entry in BEFORE..AFTER must be a verified Mergify
merge. Only AFTER needs matching queue CI: it is the tree the matrix would test.
Intermediate subjects still need the workflow's separate conventional check.
The caller must also authenticate the current push sender as Mergify (GitHub id
37929162); historical PR provenance cannot identify who replayed an old SHA.

Inspection (no output-file writes):
  python3 scripts/ci_queue_dedupe.py --repo OWNER/REPO --dry-run --last 10
Walks master first parents via REST, not the date-ordered commits listing, and
reports the last one or ten two-parent merges. --sha may pin the starting master tip.

JSON API requests are cached `gh api --method GET`; job logs use `curl -fsSL`
with the caller's read-only GH_TOKEN. Authorization is not forwarded across hosts.
Missing/malformed evidence, errors, and exhausted pagination/history bounds mean
dedupe=false, not success. Push GETs and --last 1 share a 120-second walltime
budget; --last 10 shares 600 seconds.
--compare-log-fetch requires --dry-run and adds bounded gh/curl diagnostics;
legacy probe results never change curl eligibility or extend the shared budget.
The gh probe uses output-only --include for HTTP status; headers are never emitted.
All selected build job checkout logs must identify the same immutable commit
whose tree equals AFTER; run.head_sha alone
is not checkout proof. Log reads are capped at 8 MiB and never printed.
JSON goes to stdout. Unless --dry-run or
--last, --github-output PATH (or GITHUB_OUTPUT) receives dedupe=true/false and
an empty target_url on fallback. Exit 0 includes fallback; argument errors exit 2.
The 14-day candidate window is a conservative optimization, not an exemption.
"""
from __future__ import annotations

import argparse
from datetime import datetime, timedelta, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import time
import threading
from urllib.parse import urlencode

if __package__:
    from .ci_plan import check_matrix
else:
    from ci_plan import check_matrix

MERGIFY_ID = 37929162
WORKFLOW = ".github/workflows/ci.yml"
MATRIX_JOBS = {f"check ({lane['os']})" for lane in check_matrix("pull_request", {})["include"]}
REQUIRED_JOBS = {"smarty-ci", "conventional-commits"} | MATRIX_JOBS
CONPTY_JOB = "Windows ConPTY package"
SUBJECT = re.compile(
    r"^(?:feat|fix|perf|docs|ci|test|refactor|chore|release)"
    r"(?:\([^)\r\n]+\))?!?:\s+\S.* \(#([1-9][0-9]*)\)$"
)
SHA = re.compile(r"[0-9a-f]{40}")
PAGE_SIZE, MAX_PAGES, MAX_HISTORY = 100, 5, 100
API_BUDGET_SECONDS = 120
MAX_LOG_BYTES = 8 * 1024 * 1024
MAX_LOG_STDERR_BYTES = 4096
LOG_HTTP_MARKER = "HERDR_JOB_LOG_HTTP_STATUS:"
CHECKOUT_PIN = "df4cb1c069e1874edd31b4311f1884172cec0e10"


class EvidenceError(Exception):
    pass


class Unavailable(EvidenceError):
    """An incomplete API search cannot be salvaged by another candidate."""


def require(condition, reason):
    if not condition:
        raise EvidenceError(reason)


def date(value):
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    require(parsed.tzinfo is not None, "timestamp has no timezone")
    return parsed.astimezone(timezone.utc)


def log_diagnostics(stderr, token):
    """Expose a bounded first stderr line, never credentials or redirect URLs."""
    match = re.search(r"\n" + LOG_HTTP_MARKER + r"([0-9]{3})\n$",
                      stderr["tail"].decode("utf-8", errors="replace"))
    prefix = stderr["prefix"].decode("utf-8", errors="replace")
    legacy_status = re.search(r"\bHTTP[ :]+([0-9]{3})\b", prefix)
    http_status = match[1] if match else legacy_status[1] if legacy_status else "unknown"
    truncated = len(stderr["prefix"]) == MAX_LOG_STDERR_BYTES and not prefix.endswith("\n")
    # Redact before selecting/truncating a line. Discard an incomplete captured
    # line altogether so a token cut at the byte boundary cannot leak a prefix.
    if token:
        prefix = prefix.replace(token, "[redacted token]")
    prefix = re.sub(r"https?://[^\s<>\"']+", "[redacted URL]", prefix, flags=re.IGNORECASE)
    prefix = re.sub(r"\b(?:gh[pousr]_[A-Za-z0-9_]+|github_pat_[A-Za-z0-9_]+)\b",
                    "[redacted token]", prefix)
    lines = prefix.splitlines()
    line = next((line for line in (lines[:-1] if truncated else lines)
                 if line.strip() and not line.startswith(LOG_HTTP_MARKER)),
                "stderr first line exceeds capture limit" if truncated else "no stderr")
    line = re.sub(r"[\x00-\x1f\x7f-\x9f]", " ", line)
    return {"http_status": http_status, "first_stderr": line[:500]}


def log_failure(diagnostic):
    return (f"{diagnostic['reason']} ({diagnostic['method']} exit {diagnostic['status']}; "
            f"HTTP {diagnostic['http_status']}): {diagnostic['first_stderr']}")


class GhApi:
    """GET only; gh JSON errors stay private, curl log errors are sanitized."""

    def __init__(self, budget_seconds=API_BUDGET_SECONDS, compare_log_fetch=False):
        self.cache = {}
        self.deadline = time.monotonic() + budget_seconds
        self.compare_log_fetch = compare_log_fetch
        self.log_fetch_comparison = []

    def get(self, endpoint):
        if endpoint not in self.cache:
            remaining = self.deadline - time.monotonic()
            require(remaining > 0, "GitHub GET cumulative deadline exhausted")
            try:
                response = subprocess.run(
                    ["gh", "api", "--method", "GET", endpoint,
                     "-H", "Accept: application/vnd.github+json"],
                    text=True, capture_output=True, timeout=min(remaining, 30), check=True,
                )
                self.cache[endpoint] = json.loads(response.stdout)
            except (OSError, subprocess.SubprocessError, ValueError) as error:
                raise EvidenceError("GitHub GET failed: " + endpoint.split("?")[0]) from error
        return self.cache[endpoint]

    def fetch_log(self, endpoint, method):
        """One bounded subprocess path for curl and the optional legacy probe."""
        remaining = self.deadline - time.monotonic()
        stderr = {"prefix": b"", "tail": b""}
        token = os.environ.get("GH_TOKEN", "")
        diagnostic = {"method": method, "success": False, "status": "unavailable"}
        data, raw = None, b""
        if method == "gh":
            diagnostic["include"] = "output-only"
        try:
            require(remaining > 0, "GitHub GET cumulative deadline exhausted")
            request_deadline = min(self.deadline, time.monotonic() + 30)
            require(token and not any(c in token for c in "\r\n\x00"),
                    "valid GH_TOKEN is required for job-log GET")
            if method == "curl":
                # --disable ignores curlrc (which could enable location-trusted). Feed
                # the header on stdin, not argv; curl strips it on cross-host
                # redirects by default. Only HTTPS is allowed in production.
                command = ["curl", "--disable", "-fsSL", "--proto", "=https",
                           "--proto-redir", "=https", "--max-time", str(min(remaining, 30)),
                           "--header", "@-", "--write-out",
                           "%{stderr}\n" + LOG_HTTP_MARKER + "%{http_code}\n",
                           "https://api.github.com/" + endpoint]
            else:
                command = ["gh", "api", "--method", "GET", endpoint, "--include"]
            timed_out = threading.Event()
            stdin = subprocess.PIPE if method == "curl" else subprocess.DEVNULL
            with subprocess.Popen(command, stdin=stdin, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE) as process:
                def expire():
                    timed_out.set()
                    process.kill()

                def drain_stderr():
                    # Drain concurrently so noisy failures cannot block stdout.
                    # Only the prefix and final HTTP marker are retained.
                    while chunk := process.stderr.read(4096):
                        stderr["prefix"] = (stderr["prefix"] + chunk)[:MAX_LOG_STDERR_BYTES]
                        stderr["tail"] = (stderr["tail"] + chunk)[-128:]

                timer = threading.Timer(max(0, request_deadline - time.monotonic()), expire)
                reader = threading.Thread(target=drain_stderr)
                timer.start()
                reader.start()
                try:
                    if method == "curl":
                        process.stdin.write(("Authorization: Bearer " + token + "\n").encode("utf-8"))
                        process.stdin.close()
                    raw = process.stdout.read(MAX_LOG_BYTES + 1)
                    if len(raw) > MAX_LOG_BYTES:
                        process.kill()
                    diagnostic["status"] = process.wait()
                finally:
                    if process.poll() is None:
                        process.kill()
                    process.wait()
                    timer.cancel()
                    timer.join()
                    reader.join()
            require(len(raw) <= MAX_LOG_BYTES, "checkout log byte cap exceeded")
            require(not timed_out.is_set() and not (method == "curl" and diagnostic["status"] == 28),
                    "GitHub job-log GET deadline exhausted")
            require(diagnostic["status"] == 0, "GitHub job-log GET failed")
            data = raw.decode("utf-8")
            diagnostic["success"] = True
        except EvidenceError as error:
            diagnostic["reason"] = str(error)
        except (OSError, subprocess.SubprocessError, ValueError):
            diagnostic["reason"] = "GitHub job-log GET failed"
        diagnostic.update(log_diagnostics(stderr, token))
        # --include only changes gh's output, not its GET/redirect behavior. Read
        # just the initial status line; never publish response headers/Location.
        if method == "gh" and diagnostic["http_status"] == "unknown":
            status_line = re.match(rb"HTTP/\S+\s+([0-9]{3})(?:\s|$)", raw[:MAX_LOG_STDERR_BYTES])
            if status_line:
                diagnostic["http_status"] = status_line[1].decode("ascii")
        return data, diagnostic

    def text(self, endpoint):
        if endpoint not in self.cache:
            if self.compare_log_fetch:
                _, legacy = self.fetch_log(endpoint, "gh")
            data, diagnostic = self.fetch_log(endpoint, "curl")
            if self.compare_log_fetch:
                self.log_fetch_comparison.append({"endpoint": endpoint, "methods": [legacy, diagnostic]})
            require(diagnostic["success"], log_failure(diagnostic) if not diagnostic["success"] else "")
            self.cache[endpoint] = data
        return self.cache[endpoint]


def checkout_sha(log):
    lines = [re.sub(r"^\d{4}-\d\d-\d\dT\S+Z\s+", "", line) for line in log.splitlines()]
    marker = "##[group]Run actions/checkout@" + CHECKOUT_PIN
    starts = [i for i, line in enumerate(lines) if line == marker]
    require(len(starts) == 1, "missing/ambiguous pinned checkout log")
    start = starts[0] + 1
    end = next((i for i in range(start, len(lines)) if lines[i].startswith("##[group]Run ")), len(lines))
    commands = [i for i in range(start, end) if re.fullmatch(
        r'\[command\].*\bgit(?:\.exe)?"? log -1 --format=%H', lines[i])]
    require(len(commands) == 1 and commands[0] + 1 < end,
            "missing/ambiguous checkout git-log command")
    sha = lines[commands[0] + 1]
    require(SHA.fullmatch(sha), "missing exact checkout git-log SHA")
    return sha


class Inspector:
    def __init__(self, api, repo):
        self.api, self.repo = api, repo
        self.root = "repos/" + repo
        self.cache = {}

    def get(self, path):
        endpoint = self.root + "/" + path
        if endpoint not in self.cache:
            try:
                self.cache[endpoint] = self.api.get(endpoint)
            except EvidenceError as error:
                raise Unavailable(str(error)) from error
        return self.cache[endpoint]

    def pages(self, path, key=None, params=None, cutoff=None):
        rows = []
        for page in range(1, MAX_PAGES + 1):
            query = dict(params or {}, per_page=PAGE_SIZE, page=page)
            payload = self.get(path + "?" + urlencode(query))
            batch = payload[key] if key else payload
            require(isinstance(batch, list), "malformed API page")
            rows.extend(batch)
            # Closed PRs are sorted by updated_at; everything older is irrelevant.
            if cutoff and any(date(row["updated_at"]) < cutoff for row in batch):
                return [row for row in rows if date(row["updated_at"]) >= cutoff]
            if len(batch) < PAGE_SIZE:
                if key:
                    if payload["total_count"] != len(rows):
                        raise Unavailable("incomplete API pagination")
                return rows
        raise Unavailable("pagination cap reached: " + path)

    def commit(self, sha):
        require(sha == "master" or SHA.fullmatch(sha), "invalid commit lookup SHA")
        commit = self.get("commits/" + sha)
        require(commit["sha"] == sha or sha == "master", "commit SHA mismatch")
        require(SHA.fullmatch(commit["sha"]), "invalid commit SHA")
        return commit

    def merge(self, commit, evidence):
        subject = evidence["subject"]
        require(len(evidence["parents"]) == 2, "not a two-parent merge")
        match = SUBJECT.fullmatch(subject)
        require(match is not None, "not a conventional pr-title merge subject")
        number = int(match[1])
        original = self.get("pulls/" + str(number))
        require(original["number"] == number and original["state"] == "closed"
                and original["merged"] is True and original["merged_at"]
                and original["merge_commit_sha"] == commit["sha"],
                "original PR does not confirm this merge")
        require(original["base"]["ref"] == "master"
                and original["base"]["repo"]["full_name"] == self.repo
                and original["title"] + f" (#{number})" == subject,
                "original PR base/title mismatch")
        # GitHub API merges use web-flow as committer. The original PR's
        # authenticated merged_by, not a spoofable commit identity, is authority.
        evidence["merged_by_id"] = (original.get("merged_by") or {}).get("id")
        require(evidence["merged_by_id"] == MERGIFY_ID, "original PR not merged by Mergify")
        date(original["merged_at"])
        evidence.update(original_pr=number, merged_at=original["merged_at"])
        return evidence

    def queue_proof(self, commit, inspected, merged_at):
        merged = date(merged_at)
        cutoff = merged - timedelta(days=14)
        prs = self.pages("pulls", params={"state": "closed", "base": "master",
                                         "sort": "updated", "direction": "desc"}, cutoff=cutoff)
        candidates = [pr for pr in prs if
                      (pr.get("user") or {}).get("id") == MERGIFY_ID
                      and pr.get("draft") is True and pr.get("state") == "closed"
                      and date(pr["created_at"]) <= merged
                      and pr["base"]["ref"] == "master"
                      and pr["base"]["repo"]["full_name"] == self.repo
                      and pr["head"]["repo"] is not None
                      and pr["head"]["repo"]["full_name"] == self.repo
                      and pr["head"]["ref"].startswith("mergify/merge-queue/")]
        # Closed-PR pagination and the shared GET deadline already bound discovery.
        for pr in candidates:
            entry = {"queue_pr": pr["number"], "head_sha": pr["head"]["sha"],
                     "head_ref": pr["head"]["ref"], "author_id": pr["user"]["id"],
                     "draft": pr["draft"], "head_repo": pr["head"]["repo"]["full_name"],
                     "base_ref": pr["base"]["ref"], "base_repo": pr["base"]["repo"]["full_name"]}
            inspected.append(entry)
            head = self.commit(pr["head"]["sha"])
            entry["head_tree"] = head["commit"]["tree"]["sha"]
            if entry["head_tree"] != commit["commit"]["tree"]["sha"]:
                entry["reason"] = "queue head tree mismatch"
                continue
            # Each exact queue head must pass its latest run/attempt. Older
            # abandoned distinct heads do not invalidate a successful tree proof.
            try:
                proof = self.run_proof(pr, merged, entry)
            except Unavailable:
                raise
            except EvidenceError as error:
                entry["reason"] = str(error)
                continue  # An abandoned distinct queue head does not invalidate a tested tree.
            # A complete proof is sufficient. Do not spend the shared API budget
            # traversing unrelated speculative or later queue candidates.
            return proof
        reasons = [entry["reason"] for entry in inspected
                   if entry.get("head_tree") == commit["commit"]["tree"]["sha"] and "reason" in entry]
        raise EvidenceError(reasons[0] if reasons else "no matching closed queue draft with successful CI")

    def run_proof(self, pr, merged, entry):
        head = pr["head"]
        runs = self.pages("actions/workflows/ci.yml/runs", "workflow_runs",
                          {"event": "pull_request", "head_sha": head["sha"]})
        require(runs, "no queue CI workflow run")
        runs.sort(key=lambda r: (date(r["created_at"]), r["id"]), reverse=True)
        entry["run_count"] = len(runs)
        entry["runs"] = [{"id": r["id"], "attempt": r.get("run_attempt"),
                          "status": r.get("status"), "conclusion": r.get("conclusion"),
                          "head_sha": r.get("head_sha"), "path": r.get("path"),
                          "pull_requests": r.get("pull_requests")} for r in runs[:5]]
        latest = runs[0]
        run = self.get("actions/runs/" + str(latest["id"]))
        entry["latest_run"] = {key: run.get(key) for key in (
            "id", "workflow_id", "path", "event", "head_sha", "head_branch",
            "status", "conclusion", "run_attempt", "created_at", "updated_at")}
        require(isinstance(run["id"], int) and run["id"] > 0
                and run["id"] == latest["id"] and run["workflow_id"] == latest["workflow_id"]
                and run["path"] == WORKFLOW and run["event"] == "pull_request"
                and run["repository"]["full_name"] == self.repo
                and run["head_repository"]["full_name"] == self.repo
                and run["head_sha"] == head["sha"] and run["head_branch"] == head["ref"],
                "CI workflow/repository/head mismatch")
        require(run["status"] == "completed" and run["conclusion"] == "success",
                "latest queue CI run is not successful")
        require(date(run["created_at"]) <= date(run["updated_at"]) <= merged,
                "queue CI did not complete before original PR merge")
        attempt = run["run_attempt"]
        require(isinstance(attempt, int) and attempt >= latest["run_attempt"] >= 1,
                "invalid/latest run attempt mismatch")
        jobs = self.pages(f"actions/runs/{run['id']}/attempts/{attempt}/jobs", "jobs")
        entry["jobs"] = [{key: j.get(key) for key in (
            "name", "run_id", "run_attempt", "head_sha", "status", "conclusion",
            "started_at", "completed_at")} for j in jobs]
        require(all(j["run_id"] == run["id"] and j["run_attempt"] == attempt
                    and j["head_sha"] == head["sha"] for j in jobs),
                "jobs do not belong to the latest exact run/head/attempt")
        require(all(j["status"] == "completed"
                    and j["conclusion"] in {"success", "skipped", "neutral"} for j in jobs),
                "conflicting unsuccessful or incomplete workflow job")
        # GitHub includes the conditionally skipped job in the latest attempt.
        # A successful aggregate plus an explicit skip proves it was not selected;
        # missing package evidence must never masquerade as a docs-only plan.
        packages = [j for j in jobs if j["name"] == CONPTY_JOB]
        require(len(packages) == 1, "missing or duplicate required job: " + CONPTY_JOB)
        require(packages[0]["conclusion"] in {"success", "skipped"},
                "ConPTY package job is neither successful nor explicitly skipped")
        checkout_jobs = MATRIX_JOBS | ({CONPTY_JOB} if packages[0]["conclusion"] == "success" else set())
        required_jobs = REQUIRED_JOBS | (checkout_jobs - MATRIX_JOBS)
        for name in sorted(required_jobs):
            matches = [j for j in jobs if j["name"] == name]
            require(len(matches) == 1, "missing or duplicate required job: " + name)
            job = matches[0]
            require(job["status"] == "completed" and job["conclusion"] == "success"
                    and date(job["started_at"]) <= date(job["completed_at"]) <= merged,
                    "required job not successful before original PR merge: " + name)
        snapshots = run["pull_requests"]
        require(isinstance(snapshots, list), "missing run PR snapshot list")
        if snapshots:
            self.snapshot_proof(pr, snapshots, entry)
        self.log_proof(pr, jobs, entry, checkout_jobs)
        entry["reason"] = "verified queue tree and latest CI attempt"
        return {"queue_pr": pr["number"], "queue_head_sha": head["sha"],
                "tree": entry["head_tree"], "run_id": run["id"], "run_attempt": attempt,
                "target_url": f"https://github.com/{self.repo}/actions/runs/{run['id']}"}

    def snapshot_proof(self, pr, snapshots, entry):
        require(len(snapshots) == 1, "ambiguous run PR snapshot")
        snapshot, head = snapshots[0], pr["head"]
        entry["snapshot"] = snapshot
        require(snapshot["number"] == pr["number"]
                and snapshot["head"]["sha"] == head["sha"]
                and snapshot["head"]["ref"] == head["ref"]
                and snapshot["head"]["repo"]["id"] == head["repo"]["id"]
                and snapshot["base"]["ref"] == "master"
                and snapshot["base"]["repo"]["id"] == pr["base"]["repo"]["id"],
                "queue PR snapshot head/base mismatch")

    def log_proof(self, pr, jobs, entry, checkout_jobs):
        if getattr(self.api, "compare_log_fetch", False):
            # Inspection collects both methods for all selected logs even when an
            # early curl read fails. Fail closed after collecting the probes.
            failures = []
            for job in jobs:
                if job["name"] not in checkout_jobs:
                    continue
                require(type(job["id"]) is int and job["id"] > 0, "invalid workflow job ID")
                endpoint = self.root + f"/actions/jobs/{job['id']}/logs"
                if endpoint not in self.cache:
                    try:
                        self.cache[endpoint] = self.api.text(endpoint)
                    except EvidenceError as error:
                        failures.append(error)
            if failures:
                raise Unavailable(str(failures[0])) from failures[0]
        entry["checkouts"] = []
        for job in jobs:
            if job["name"] not in checkout_jobs:
                continue
            require(type(job["id"]) is int and job["id"] > 0, "invalid workflow job ID")
            endpoint = self.root + f"/actions/jobs/{job['id']}/logs"
            if endpoint not in self.cache:
                try:
                    self.cache[endpoint] = self.api.text(endpoint)
                except EvidenceError as error:
                    raise Unavailable(str(error)) from error
            sha = checkout_sha(self.cache[endpoint])
            checked = self.commit(sha)
            require(sha == pr["head"]["sha"] or
                    (len(checked["parents"]) == 2 and
                     pr["head"]["sha"] in [p["sha"] for p in checked["parents"]]),
                    "checkout commit is not the queue head or its synthetic merge")
            tree = checked["commit"]["tree"]["sha"]
            entry["checkouts"].append({"job": job["name"], "job_id": job["id"],
                                       "sha": sha, "tree": tree})
            require(tree == entry["head_tree"], "tested checkout tree mismatch")
        require(len(entry["checkouts"]) == len(checkout_jobs)
                and len({c["sha"] for c in entry["checkouts"]}) == 1,
                "selected build jobs did not check out the same exact commit")
        entry["checkout_evidence"] = f"{len(checkout_jobs)} selected build checkout logs"

    def inspect(self, sha, before=None):
        result = {"dedupe": False, "target_url": "", "sha": sha, "before": before,
                  "range": [], "candidates": []}
        try:
            require(SHA.fullmatch(sha), "invalid after SHA")
            require(before is None or (SHA.fullmatch(before) and before != "0" * 40),
                    "invalid/initial push before SHA")
            commit = self.commit(sha)
            result["tree"] = commit["commit"]["tree"]["sha"]
            result["committed_at"] = commit["commit"]["committer"]["date"]
            current = commit
            for _ in range(MAX_HISTORY):
                if before is not None and current["sha"] == before:
                    break
                evidence = {"sha": current["sha"],
                            "subject": current["commit"]["message"].splitlines()[0],
                            "committer_id": (current.get("committer") or {}).get("id"),
                            "parents": [p["sha"] for p in current["parents"]]}
                result["range"].append(evidence)
                self.merge(current, evidence)
                if before is None:
                    break  # --last inspection checks one merge independently
                require(current["parents"], "before not on first-parent history")
                current = self.commit(current["parents"][0]["sha"])
            else:
                raise EvidenceError("first-parent history cap reached")
            require(result["range"], "empty push range")
            result["merged_at"] = result["range"][0]["merged_at"]
            proof = self.queue_proof(commit, result["candidates"], result["merged_at"])
            result.update(proof, dedupe=True, reason="verified queue CI for pushed tree")
        except (EvidenceError, KeyError, TypeError, ValueError, IndexError, AttributeError) as error:
            result["reason"] = str(error) if isinstance(error, EvidenceError) else "missing/malformed API evidence"
        return result

    def last(self, start, count):
        results = []
        try:
            current = self.commit(start)
            tip = current["sha"]
            for _ in range(MAX_HISTORY):
                if len(current["parents"]) == 2:
                    results.append(self.inspect(current["sha"]))
                    if len(results) == count:
                        return {"mode": "last", "sha": tip, "results": results}
                require(current["parents"], "history ended before requested merge count")
                current = self.commit(current["parents"][0]["sha"])
            raise EvidenceError("first-parent history cap reached")
        except (EvidenceError, KeyError, TypeError, ValueError, IndexError, AttributeError) as error:
            return {"mode": "last", "dedupe": False, "target_url": "", "results": results,
                    "reason": str(error) if isinstance(error, EvidenceError) else "missing/malformed API evidence"}


def main(argv=None, api=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True)
    parser.add_argument("--sha")
    parser.add_argument("--before")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--last", type=int, choices=[1, 10])
    parser.add_argument("--compare-log-fetch", action="store_true",
                        help="compare bounded gh/curl job-log diagnostics (requires --dry-run)")
    parser.add_argument("--github-output", default=os.environ.get("GITHUB_OUTPUT"))
    args = parser.parse_args(argv)
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        parser.error("--repo must be OWNER/REPO")
    if not args.last and (not args.sha or not args.before):
        parser.error("push mode requires --sha and --before")
    if args.last and args.before:
        parser.error("--before is only used in push mode")
    if args.compare_log_fetch and not args.dry_run:
        parser.error("--compare-log-fetch requires --dry-run")
    api = api or GhApi(budget_seconds=600 if args.last == 10 else API_BUDGET_SECONDS,
                       compare_log_fetch=args.compare_log_fetch)
    inspector = Inspector(api, args.repo)
    result = (inspector.last(args.sha or "master", args.last) if args.last
              else inspector.inspect(args.sha, args.before))
    if args.compare_log_fetch:
        result["log_fetch_comparison"] = api.log_fetch_comparison
    print(json.dumps(result, sort_keys=True))
    if args.github_output and not args.dry_run and not args.last:
        with Path(args.github_output).open("a", encoding="utf-8") as output:
            output.write(f"dedupe={str(result['dedupe']).lower()}\ntarget_url={result['target_url']}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
