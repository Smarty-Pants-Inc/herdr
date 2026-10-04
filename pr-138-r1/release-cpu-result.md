# Release CPU landing gate — herdr#138

**RELEASE_CPU_GATE: PASS** — repaired head is below its agreed PR base on the requested fixture. The reduction is modest, not a solution to the remaining idle cost.

| Release build | 60 s run 1 | Run 2 | Run 3 | Median (% of one core) |
|---|---:|---:|---:|---:|
| Base `dee2ba2a5e5051258b83f027433392eaf6a35a8a` | 8.5644% | 9.0304% | 8.6482% | **8.6482%** |
| Head `47bd736357dc890e74987f97953d9039a273df77` | 8.3311% | 8.3805% | 8.3141% | **8.3311%** |

Head minus base: **−0.3171 percentage points / −3.6668%** relative CPU. The earlier debug result remains an increase: reviewed `100703cd` 28.0830% → repaired head 30.4997% (+8.61%); it is not hidden or substituted for this release pair.

## Method and provenance

- Same physical scratch repository set, 100 real Git workspaces, real 160×50 idle PTY client. Identical config, isolated HOME/XDG/session/socket state; no fleet server or API polling during measured intervals.
- Six 60 s intervals in one session, alternating base/head within each round, after the independent delta verdict. Server process utime+stime / actual elapsed time, percent of one core; three-run median per build.
- Pinned Cargo/Zig, sanitized PI_CODING_AGENT_DIR and inherited HERDR_*; each build/probe inside `timeout 45m systemd-run --user --scope -p CPUQuota=800%`.
- Base is the previously tested retained release artifact, SHA256 `96bacde4b1e494ce7c7583e6b6c264c1869ac150048cfb47a1716633f02eff1a`. `git get-tar-commit-id` verifies its retained source archive names `dee2ba2a5e5051258b83f027433392eaf6a35a8a`; the original release-build log remains under `.local/4346/cpu-evidence/baseline-release-build.log`.
- Head was freshly built from clean frozen `47bd7363`, release build exit 0 (1m36s). SHA256 `cc5d43181c7eb5c64af9b73a5d905710cdeef61d737ca80ed12d7703e0397963`, retained at `artifacts/herdr-final-release`.
- Raw case JSON, PTY bytes, workspace lists and thread snapshots: `release-cpu-evidence/`. Canonical full receipt: `release-cpu-evidence/release-pair-summary.json`. Recipes: `release-build.sh`, `release-pair-run.sh`, `release_pair_probe.py`; logs/exits: `release-build.*`, `release-pair.*`.

## Read-only thread sample

At interval boundaries, native `/proc/<pid>/task/*/stat` snapshots recorded live-thread CPU ticks. No sampling loop ran during the interval. Main-thread ticks over 60 s: base **15/16/15**, head **4/4/2**. Head's notify thread used **5/3/4** ticks. Tokio workers dominate the retained-thread sample: base **429/453/438** ticks, head **459/460/460** ticks. PTY readers used zero recorded ticks.

This is not a stack profile or complete per-thread CPU attribution: departed refresh workers are absent from the final live-thread snapshot, and tick quantization matters. Total process CPU, including departed workers, is the gate metric. The unchanged per-pane 300 ms detector timers remain the scoped follow-up Smarty-Pants-Inc/smarty-dev#4366; no detector change is included here. The original C9-main-cost premise remains unsupported.

All six server/client pairs were stopped and reaped. No source edit, push, GitHub action or install occurred. macOS CI, review/astra, queue and installation remain herdr-lead's gates.
