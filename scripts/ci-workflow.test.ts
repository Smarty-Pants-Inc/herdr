import { describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

type Step = { name: string; if?: string; env?: Record<string, string>; run: string; "continue-on-error"?: boolean; "timeout-minutes"?: number };
type Job = {
  name?: string;
  if?: string;
  needs?: string | string[];
  permissions?: Record<string, string>;
  strategy?: { matrix: string };
  outputs?: Record<string, string>;
  steps: Step[];
};
const workflow = Bun.YAML.parse(
  readFileSync(new URL("../.github/workflows/ci.yml", import.meta.url), "utf8"),
) as { permissions: Record<string, string>; jobs: Record<string, Job> };
const jobs = workflow.jobs;
const aggregate = jobs["smarty-ci"];
function step(name: string): Step {
  const matches = aggregate.steps.filter((candidate) => candidate.name === name);
  expect(matches).toHaveLength(1);
  expect(typeof matches[0].run).toBe("string");
  return matches[0];
}
const gate = step("Require every selected CI lane to succeed");
const publisher = step("Publish queue-tested install checks");

// Match GitHub's explicit bash shell: a failed test must stop the step immediately.
function runShell(source: string, env: Record<string, string>, cwd?: string) {
  return spawnSync("bash", ["--noprofile", "--norc", "-e", "-o", "pipefail", "-c", source], {
    cwd,
    env: { PATH: process.env.PATH ?? "/usr/bin:/bin", ...env },
    encoding: "utf8",
    timeout: 5_000,
  });
}
const full = {
  GITHUB_EVENT_NAME: "pull_request",
  GITHUB_REF: "refs/pull/42/merge",
  PLAN_RESULT: "success",
  CONVENTIONAL_RESULT: "success",
  CHECK_RESULT: "success",
  CONPTY_RESULT: "success",
  CONPTY_SELECTED: "true",
  DEDUPE: "false",
};
const deduped = {
  ...full,
  GITHUB_EVENT_NAME: "push",
  GITHUB_REF: "refs/heads/master",
  DEDUPE: "true",
  CHECK_RESULT: "skipped",
  CONPTY_RESULT: "skipped",
  PUSH_SENDER_ID: "37929162",
};

describe("CI aggregate runs the real workflow gate", () => {
  const cases: [string, Record<string, string>, boolean][] = [
    ["all selected lanes succeed", full, true],
    ["explicitly unaffected ConPTY package may skip", { ...full, CONPTY_SELECTED: "false", CONPTY_RESULT: "skipped" }, true],
    ["selected ConPTY skip rejects", { ...full, CONPTY_RESULT: "skipped" }, false],
    ["selected ConPTY failure rejects", { ...full, CONPTY_RESULT: "failure" }, false],
    ["missing ConPTY flag cannot authorize a skip", { ...full, CONPTY_SELECTED: "", CONPTY_RESULT: "skipped" }, false],
    ["unaffected ConPTY must actually skip", { ...full, CONPTY_SELECTED: "false" }, false],
    ["check failure rejects", { ...full, CHECK_RESULT: "failure" }, false],
    ["check cancellation rejects", { ...full, CHECK_RESULT: "cancelled" }, false],
    ["ordinary check skip rejects", { ...full, CHECK_RESULT: "skipped" }, false],
    ["plan failure rejects", { ...full, PLAN_RESULT: "failure" }, false],
    ["queue-evidence dedupe with plan and conventional success accepts", deduped, true],
    ["dedupe does not mask conventional failure", { ...deduped, CONVENTIONAL_RESULT: "failure" }, false],
    ["dedupe does not mask plan failure", { ...deduped, PLAN_RESULT: "failure" }, false],
    ["a human replay of a proven queue SHA rejects dedupe", { ...deduped, PUSH_SENDER_ID: "123" }, false],
    ["missing push attribution rejects dedupe", { ...deduped, PUSH_SENDER_ID: "" }, false],
    ["dedupe on a nonmaster push rejects", { ...deduped, GITHUB_REF: "refs/heads/feature" }, false],
    ["dedupe on a PR rejects", { ...deduped, GITHUB_EVENT_NAME: "pull_request" }, false],
    ["dedupe requires skipped check", { ...deduped, CHECK_RESULT: "success" }, false],
    ["dedupe requires skipped package", { ...deduped, CONPTY_RESULT: "success" }, false],
    ["windows push accepts conventional skip", { ...full, GITHUB_EVENT_NAME: "push", GITHUB_REF: "refs/heads/windows", CONVENTIONAL_RESULT: "skipped" }, true],
    ["ordinary PR rejects conventional skip", { ...full, CONVENTIONAL_RESULT: "skipped" }, false],
    ["master push rejects conventional skip", { ...full, GITHUB_EVENT_NAME: "push", GITHUB_REF: "refs/heads/master", CONVENTIONAL_RESULT: "skipped" }, false],
  ];
  for (const [name, env, succeeds] of cases) {
    test.skipIf(process.platform === "win32")(name, () => {
      const result = runShell(gate.run, env);
      expect(result.error).toBeUndefined();
      expect(result.signal).toBeNull();
      expect(result.status === 0).toBe(succeeds);
    });
  }
});

describe("queue-tested status publisher runs without GitHub or network access", () => {
  const publishEnv = {
    GITHUB_EVENT_NAME: "push",
    GITHUB_REF: "refs/heads/master",
    GITHUB_REPOSITORY: "example/herdr",
    GITHUB_SHA: "0123456789abcdef0123456789abcdef01234567",
    QUEUE_RUN_URL: "https://github.com/example/herdr/actions/runs/987654321",
    DEDUPE: "true",
    PUSH_SENDER_ID: "37929162",
  };
  function publish(overrides: Record<string, string> = {}) {
    const dir = mkdtempSync(join(process.env.TMPDIR || tmpdir(), "herdr-ci-workflow-"));
    try {
      const log = join(dir, "gh-args");
      // NUL framing preserves each argument, including the spaced check context.
      writeFileSync(join(dir, "gh"), `#!/bin/bash
printf '%s\\0' "$@" >> "$GH_ARGS_LOG"
printf '\\0' >> "$GH_ARGS_LOG"
`, { mode: 0o755 });
      const result = runShell(publisher.run, {
        ...publishEnv,
        ...overrides,
        GH_ARGS_LOG: log,
        PATH: `${dir}:${process.env.PATH ?? "/usr/bin:/bin"}`,
      }, dir);
      const calls = existsSync(log)
        ? readFileSync(log, "utf8").split("\0\0").filter(Boolean).map((call) => call.split("\0"))
        : [];
      return { result, calls };
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  }

  test.skipIf(process.platform === "win32")("publishes exactly the two install contexts on the exact SHA and queue run", () => {
    const { result, calls } = publish();
    expect(result.error).toBeUndefined();
    expect(result.status).toBe(0);
    expect(calls).toEqual(["smarty-ci", "check (ubuntu-latest)"].map((context) => [
      "api", "--method", "POST",
      `repos/${publishEnv.GITHUB_REPOSITORY}/statuses/${publishEnv.GITHUB_SHA}`,
      "-f", "state=success", "-f", `context=${context}`,
      "-f", `target_url=${publishEnv.QUEUE_RUN_URL}`,
      "-f", "description=Queue tested the identical master tree",
    ]));
  });

  for (const [name, overrides] of [
    ["human direct push", { PUSH_SENDER_ID: "123" }],
    ["missing sender", { PUSH_SENDER_ID: "" }],
    ["dedupe false", { DEDUPE: "false" }],
    ["missing dedupe", { DEDUPE: "" }],
    ["nonmaster push", { GITHUB_REF: "refs/heads/windows" }],
    ["nonpush event", { GITHUB_EVENT_NAME: "pull_request" }],
  ] as [string, Record<string, string>][]) {
    test.skipIf(process.platform === "win32")(`rejects ${name} before any status call`, () => {
      const { result, calls } = publish(overrides);
      expect(result.error).toBeUndefined();
      expect(result.signal).toBeNull();
      expect(result.status).not.toBe(0);
      expect(calls).toEqual([]);
    });
  }
});

describe("push planning never dedupes direct pushes", () => {
  const planner = jobs.plan.steps.find((candidate) => candidate.name === "Select CI lanes and verify queue evidence")!;
  for (const [name, event, ref, sender, fails, dedupe] of [
    ["Mergify master push", "push", "refs/heads/master", "37929162", "0", "true"],
    ["human replay of the same SHA", "push", "refs/heads/master", "123", "0", "false"],
    ["missing sender evidence", "push", "refs/heads/master", "", "0", "false"],
    ["Windows push", "push", "refs/heads/windows", "37929162", "0", "false"],
    ["PR", "pull_request", "refs/pull/42/merge", "37929162", "0", "false"],
    ["inspection error after partial output", "push", "refs/heads/master", "37929162", "1", "false"],
  ]) {
    test.skipIf(process.platform === "win32")(name, () => {
      const dir = mkdtempSync(join(process.env.TMPDIR || tmpdir(), "herdr-ci-plan-shell-"));
      try {
        const output = join(dir, "output");
        writeFileSync(join(dir, "python3"), `#!/bin/bash
if [ "$1" = scripts/ci_queue_dedupe.py ]; then
  echo dedupe=true >> "$GITHUB_OUTPUT"
  exit "$INSPECTION_FAILS"
fi
`, { mode: 0o755 });
        const result = runShell(planner.run, {
          PATH: `${dir}:${process.env.PATH ?? "/usr/bin:/bin"}`,
          GITHUB_OUTPUT: output, GITHUB_EVENT_PATH: join(dir, "event.json"),
          GITHUB_EVENT_NAME: event, GITHUB_REF: ref, PUSH_SENDER_ID: sender,
          GITHUB_REPOSITORY: "example/herdr", GITHUB_SHA: "a".repeat(40), BEFORE_SHA: "b".repeat(40),
          INSPECTION_FAILS: fails,
        }, dir);
        expect(result.status).toBe(0);
        const decisions = readFileSync(output, "utf8").trim().split("\n");
        expect(decisions.at(-1)).toBe(`dedupe=${dedupe}`);
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    });
  }
});

describe("workflow-token log probe cannot change planning", () => {
  const probe = jobs.plan.steps.find((candidate) => candidate.name === "Probe queue log fetch with the workflow token")!;
  test("is a bounded, nonblocking PR-only read with the job token", () => {
    expect(probe.if).toBe("github.event_name == 'pull_request'");
    expect(probe["continue-on-error"]).toBe(true);
    expect(probe["timeout-minutes"]).toBe(3);
    expect(probe.env).toEqual({ GH_TOKEN: "${{ github.token }}" });
    expect(probe.run).toContain("--dry-run --last 1 --compare-log-fetch");
    expect(probe.run).not.toContain("--github-output");
  });

  for (const eligible of [true, false]) {
    test.skipIf(process.platform === "win32")(`prints evidence and eligibility ${eligible} without writing plan outputs`, () => {
      const dir = mkdtempSync(join(process.env.TMPDIR || tmpdir(), "herdr-ci-probe-"));
      try {
        const output = join(dir, "output");
        writeFileSync(output, "dedupe=false\n");
        const python = spawnSync("python3", ["-c", "import sys; print(sys.executable)"], { encoding: "utf8" }).stdout.trim();
        for (const command of ["gh", "curl"]) {
          writeFileSync(join(dir, command), "#!/bin/bash\nexit 0\n", { mode: 0o755 });
        }
        writeFileSync(join(dir, "python3"), `#!/bin/bash
if [ "$1" = scripts/ci_queue_dedupe.py ]; then
  printf '%s\\n' "$PROBE_REPORT"
else
  exec "$REAL_PYTHON" "$@"
fi
`, { mode: 0o755 });
        const report = { results: [{ dedupe: eligible, candidates: [{ checkouts: eligible ? [{}, {}, {}] : [] }] }] };
        const result = runShell(probe.run, {
          PATH: `${dir}:${process.env.PATH ?? "/usr/bin:/bin"}`,
          GITHUB_OUTPUT: output, GITHUB_REPOSITORY: "example/herdr",
          PROBE_REPORT: JSON.stringify(report), REAL_PYTHON: python,
        }, dir);
        expect(result.status).toBe(0);
        const lines = result.stdout.trim().split("\n").map((line) => JSON.parse(line));
        expect(lines).toEqual([report, { eligible, checkouts: eligible ? 3 : 0 }]);
        expect(readFileSync(output, "utf8")).toBe("dedupe=false\n");
      } finally {
        rmSync(dir, { recursive: true, force: true });
      }
    });
  }
});

describe("CI workflow job boundaries", () => {
  test("status writes are confined to the aggregate; the planner is read-only", () => {
    expect(workflow.permissions).toEqual({ contents: "read" });
    expect(aggregate.permissions).toEqual({ statuses: "write" });
    expect(jobs.plan.permissions).toEqual({ contents: "read", "pull-requests": "read", actions: "read" });
    for (const [name, job] of Object.entries(jobs)) {
      if (name !== "smarty-ci") {
        expect(job.permissions?.statuses === "write").toBe(false);
        expect(Object.values(job.permissions ?? {}).every((permission) => permission === "read")).toBe(true);
      }
    }
  });

  test("matrix and package selection are gated by the planner", () => {
    expect(jobs.check.needs).toBe("plan");
    expect(jobs.check.strategy?.matrix).toBe("${{ fromJSON(needs.plan.outputs.matrix) }}");
    expect(jobs.check.if).toBe("needs.plan.outputs.dedupe != 'true'");
    expect(jobs["windows-conpty-package"].needs).toBe("plan");
    expect(jobs["windows-conpty-package"].if).toBe("needs.plan.outputs.dedupe != 'true' && needs.plan.outputs.conpty == 'true'");
    expect(jobs.plan.outputs).toEqual({
      matrix: "${{ steps.plan.outputs.matrix }}",
      conpty: "${{ steps.plan.outputs.conpty }}",
      dedupe: "${{ steps.plan.outputs.dedupe }}",
      target_url: "${{ steps.plan.outputs.target_url }}",
    });
  });

  test("aggregate observes failed and skipped dependencies and binds gate evidence", () => {
    expect(aggregate.if).toBe("${{ always() }}");
    expect(aggregate.needs).toEqual(["plan", "conventional-commits", "check", "windows-conpty-package"]);
    expect(gate.env).toEqual({
      PLAN_RESULT: "${{ needs.plan.result }}",
      CONVENTIONAL_RESULT: "${{ needs.conventional-commits.result }}",
      CHECK_RESULT: "${{ needs.check.result }}",
      CONPTY_RESULT: "${{ needs.windows-conpty-package.result }}",
      CONPTY_SELECTED: "${{ needs.plan.outputs.conpty }}",
      DEDUPE: "${{ needs.plan.outputs.dedupe }}",
      PUSH_SENDER_ID: "${{ github.event.sender.id }}",
    });
    expect(aggregate.name).toBe("${{ needs.plan.outputs.dedupe == 'true' && 'queue-tested CI' || 'smarty-ci' }}");
  });

  test("publisher is limited to successful aggregate execution on deduped master pushes", () => {
    expect(publisher.if).toBe("needs.plan.outputs.dedupe == 'true' && github.event_name == 'push' && github.ref == 'refs/heads/master' && github.event.sender.id == 37929162");
    expect(publisher.env).toEqual({
      GH_TOKEN: "${{ github.token }}",
      QUEUE_RUN_URL: "${{ needs.plan.outputs.target_url }}",
      DEDUPE: "${{ needs.plan.outputs.dedupe }}",
      PUSH_SENDER_ID: "${{ github.event.sender.id }}",
    });
    expect(aggregate.steps.indexOf(publisher)).toBeGreaterThan(aggregate.steps.indexOf(gate));
  });
});
