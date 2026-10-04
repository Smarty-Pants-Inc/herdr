# #4346 live Git behavioral proof — PASS; scope QUIESCENT

This is an implementation real-path behavioral probe, **not the final audit**. Main owns final checks/CPU comparison/audit. No tracked source edits, builds, agent spawning, commits, pushes, GitHub, or fleet-server commands were performed by this task.

## Ownership return

QUIESCENT. Exact write scope returned:

- `/home/paul/herdr-lanes/herdr-4346/.local/4346/live_git_probe.py`
- `/home/paul/herdr-lanes/herdr-4346/.local/4346/live-proof-result.md`
- Explicitly authorized evidence directory `/home/paul/herdr-lanes/herdr-4346/.local/4346/live-proof/`
- Own unique private `mkdtemp` execution/precheck scratch only (all removed).

Base head: `dee2ba2a5e5051258b83f027433392eaf6a35a8a`; Main confirmed frozen uncommitted source changes and successful build before any candidate execution.

Candidate: `/tmp/herdr-4346.Kly4v4/target/debug/herdr`

Candidate SHA256: `5d5ebeeea8d96817ead77be924f2c8243e21eab07fea83e2467352cf17198150` (matches Main's confirmation).

Probe script SHA256: `7a5199734108384b48ddf292fbf095efcec22b8d4e55f6d50ae43a83657ad009`.

## Canonical result

**`.local/4346/live-proof/run-3/proof.json` — PASS.** Actual foreground server PID 1383217; actual PTY thin client PID 1383414; fixed geometry 160x50. Total suite elapsed 16.27s. No API requests, typing, focus changes, artificial refresh, or candidate polling during Git display latency measurements.

Latency is a conservative upper bound: monotonic time at successful Git command completion to observing the expected cells on the current reconstructed client screen. Output is differential ANSI replay, not a stale raw marker. All 10 measured transitions were <1 second:

| Real operation | Repetitions | Maximum displayed latency |
| --- | ---: | ---: |
| `git switch probe-branch` | 3 | 125.32 ms |
| `git switch main` | 3 | 138.63 ms |
| Real `git commit`, displayed ↑1 → ↑2 → ↑3 | 3 | 121.06 ms |
| Real divergent remote commit + origin/main `update-ref`, displayed ↑3 ↓1 | 1 | 124.53 ms |

**Overall maximum: 138.63 ms.** Main/probe-branch upstreams genuinely resolve to refs/remotes/origin/main via an explicit remote fetch refspec. Actual `git rev-list --left-right --count @{upstream}...HEAD` agrees with every ahead count and final behind count. No dirty/index flag exists or was invented; index-trigger coverage remains with the source owner's unit tests.

## Native resources: actual server `/proc`

A retained anchor workspace remained throughout **100 create/close cycles** at distinct private cloned fixture paths. Every cycle observed that path's actual inotify inode registration before closing, then observed its absence after closing. Closed paths stayed on disk until final observation, so deletion could not mask a leaking registration.

| Measurement | Before | After |
| --- | ---: | ---: |
| `/proc/1383217/fd` count | 23 | 23 |
| Native watch lines across `/proc/1383217/fdinfo/*` | 6 | 6 |

Before/after FD target maps are **exactly equal**. Before/after native watch records (including wd, inode, sdev, raw fdinfo line) are **exactly equal**. No removed-path inodes linger. Final API state contains only retained anchor `w1`; final real client screen also contains only the retained anchor, displaying `main ↑3 ↓1`.

## Evidence

Canonical proof JSON: `.local/4346/live-proof/run-3/proof.json`

Terminal bytes: `.local/4346/live-proof/run-3/client.ansi` (53,495 bytes).

Timestamped chunk offsets: `.local/4346/live-proof/run-3/client-trace.json`.

Current-screen text and cumulative ANSI bytes for each transition: `branch-switch-{1,2,3}`, `branch-return-{1,2,3}`, `commit-ahead-{1,2,3}`, `remote-behind-1`, `initial-main`, `final-retained-anchor` under the same directory. Final text proof:

```text
· retained-anchor
  main ↑3 ↓1
```

No unsupported ANSI cell operations were observed. Full per-cycle receipts, Git commands, actual server/client IDs, isolation paths, FD targets, and raw inotify records are in proof JSON.

## Isolation, execution, cleanup

Executed with:

```sh
timeout 45m systemd-run --user --scope -p CPUQuota=800% \
  python3 .local/4346/live_git_probe.py \
  /tmp/herdr-4346.Kly4v4/target/debug/herdr --candidate-built-confirmed \
  --evidence .local/4346/live-proof/run-3
```

Scope: `run-u284497.scope`; invocation ID: `bd6f41348fee4620882a86946eb20089`.

All inherited `HERDR_*`, `PI_CODING_AGENT_DIR`, and Git overrides were removed before assigning private HOME, XDG config/runtime/state/cache, `HERDR_SESSION=live4346`, and private API socket. Derived `api-client.sock` was checked. No saved fleet endpoints or current session were used.

Own server/client were stopped and reaped; `/proc` confirms both PIDs absent. Server exit was 0; client exit was 1 after intentional server shutdown. All own private scratch was removed after preserving evidence.

## Preparation checks and retained iterations

- Python syntax/AST: PASS.
- ANSI differential overwrite, erased stale marker, split UTF-8, SGR/OSC suppression: PASS.
- Real private Git upstream/commit precheck (ahead 0 → 1): PASS.
- Own real native inotify `/proc` parser registration/release precheck: PASS.
- Required timeout/systemd CPUQuota wrapper self-test: PASS.

All iterations are preserved. The first attempt (`live-proof/proof.json`) hit a **probe startup readiness race**: API ping can precede the independent client listener; script repaired to wait for the derived socket. No candidate fault was found. `run-2/proof.json` passed all latency/resource checks, but its final screenshot was captured before draining the final removal frame. Script repaired to explicitly wait for the final anchor-only screen; canonical `run-3` passes this additional real-UI check. Neither probe correction changed tracked source. No further writes or processes remain in flight.
