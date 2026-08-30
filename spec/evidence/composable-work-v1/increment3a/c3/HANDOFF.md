# Increment 3A C3 — Herdr Seam Closure Handoff

## Status and exact identities

- Mission: `smarty-pants-increment3a-corrective-closure`
- Result: C3 Herdr source, compatibility, API, schema, full-suite, and arm64 Darwin binary verification complete; Integration Master landing and root pin work remain pending.
- Review base: `ebb46b51f22505b3fdec447915330754f539de86`
- Assigned frozen base: `6f8f02db469af0f2592b7ba5ab7b9d5b8ce7a811`
- Frozen-base tree: `d222a70f5c22255d02f54de38f0fbaf141d76def`
- Production repair commit: `87c0f9ba919f0a42c1d3347d9c64f9259c9ae889`
- Production source tree: `9be0f1b2ce459962f30a59d66622609e51eb7a6f`
- Branch: `i3a-corrective/c3-seam-closure`
- Evidence commit: the commit containing this file; Git cannot embed its own SHA.
- Worktree: `/Users/paulbettner/.herdr/worktrees/herdr/i3a-corrective-c3`
- No push, pull request, merge, install, release, deployment, or root-pin change occurred.

## Authority custody

Both authority files were read completely before work.

| Authority | SHA-256 | Bytes |
| --- | --- | ---: |
| `/Users/paulbettner/Projects/smarty-net/SMARTY_MISSION.md` | `1240595f21bc3a629b78aa85242f0f61e520ff238d13c06321c420de96fb92f2` | 8082 |
| `/Users/paulbettner/Downloads/SMARTY_PANTS_INCREMENT3A_CORRECTIVE_SOURCE_HANDOFF.md` | `e16a65689b9f6c39ab4ba62bd4aa5c69efa2553fe1f9b9dc4eaf630d5f0d1ec2` | 23694 |

Raw custody commands and results:

```text
shasum -a 256 /Users/paulbettner/Projects/smarty-net/SMARTY_MISSION.md
1240595f21bc3a629b78aa85242f0f61e520ff238d13c06321c420de96fb92f2
wc -c /Users/paulbettner/Projects/smarty-net/SMARTY_MISSION.md
8082

shasum -a 256 /Users/paulbettner/Downloads/SMARTY_PANTS_INCREMENT3A_CORRECTIVE_SOURCE_HANDOFF.md
e16a65689b9f6c39ab4ba62bd4aa5c69efa2553fe1f9b9dc4eaf630d5f0d1ec2
wc -c /Users/paulbettner/Downloads/SMARTY_PANTS_INCREMENT3A_CORRECTIVE_SOURCE_HANDOFF.md
23694
```

## Task, session, and model evidence

- Direct task: `/private/tmp/smarty-i3a-corrective-c3-assignment.txt`
- OMP process PID at verification: `20009`
- Exact process command:

```text
/Users/paulbettner/.local/share/smarty-dev/omp/versions/2764ac65ddbebc4e2db28e62454ade677cc8eb93/runtime/omp -p --model cliproxyapi/gpt-5.6-sol --thinking xhigh --cwd /Users/paulbettner/.herdr/worktrees/herdr/i3a-corrective-c3 @/private/tmp/smarty-i3a-corrective-c3-assignment.txt Execute the attached C3 assignment now.
```

- Model: `cliproxyapi/gpt-5.6-sol`
- Thinking: `xhigh`
- Herdr workspace/tab/pane: `wCN` / `wCN:t1` / `wCN:p1`
- OMP session path: `/Users/paulbettner/.omp/agent/sessions/-.herdr-worktrees-herdr-i3a-corrective-c3/2026-08-30T20-10-13-571Z_01a0544b-42c3-7000-8455-433b6f74aabf.jsonl`
- Native mailbox session: `01a05459-cc6c-7000-b571-8bbb8867b764`
- Native mailbox address: `agent-c8f81f858eb998e530670ca6c97f71254657dc4a3ffe2b6ce5b0b8d4924d3c5c`
- Integration Master mailbox: `agent-13096670e1e1ca9a1cdebab3275f7d41fc03712208a7cbcea32a7efa62c2bd2f`
- Integration Master was verified in Herdr pane `w2G:p2`, cwd `/Users/paulbettner/Projects/smarty-net`.

## Patch ledger

The assigned three-commit generic Herdr patch is unchanged. C3 adds one strictly proved public API repair.

| Commit | Files | Delta | Contract |
| --- | ---: | ---: | --- |
| `cce4622f` `fix: support externally owned agent session restore` | 14 | `+276/-21` | Adds product-neutral `AgentResumePolicy::{Native,External}`, public `pane.report_agent_session` input and CLI plumbing, persistence/restore policy, terminal propagation, schema/docs, and External no-native-plan coverage. |
| `10c7b17c` `fix: preserve external resume policy` | 2 | `+87/-5` | Moves session and history snapshots from v3 to v4, proves a v3 reader rejects v4 policy state, and preserves External through detection conflicts and replacement-generation state. |
| `6f8f02db` `fix: accept externally managed omp reporters` | 1 | `+66/-1` | Allows fresh sequenced, recognized External OMP startup/lifecycle reports to anchor without premature native process detection; adds focused coverage. |
| `87c0f9ba` `fix: expose external agent resume policy` | 5 | `+108/-5` | Adds optional public `AgentSessionInfo.resume_policy`, emits only `external` so Native JSON shape stays unchanged, preserves live authority policy projection, regenerates schema, updates draft docs, and adds the fail-first conformance test. |

Review-base through production head:

```text
cce4622f fix: support externally owned agent session restore
10c7b17c fix: preserve external resume policy
6f8f02db fix: accept externally managed omp reporters
87c0f9ba fix: expose external agent resume policy
17 files changed, 535 insertions(+), 30 deletions(-)
```

No Smarty Project, Page, Agent Session, authorization, operation, product-domain, or reconstruction authority was added to Herdr. Smarty remains responsible for authorization and external launch reconstruction; it only emits generic External session reports.

## Proved public API repair

### Problem

`pane.report_agent_session`, persistence, restore state, generated request schema, and documentation carried `resume_policy`, but the shared public `AgentSessionInfo` returned by `pane.get`, `pane.list`, `agent.get`, and `agent.list` omitted it. A caller could not distinguish an externally owned session from the default Native policy.

### Fail-first evidence

Command:

```text
just test-one pane_info_exposes_external_resume_policy_without_changing_native_shape
```

Before repair, compilation failed with exit `101`:

```text
error[E0609]: no field `resume_policy` on type `&schema::agents::AgentSessionInfo`
note: available fields are: `source`, `agent`, `kind`, `value`
```

After adding the Rust field and projection, the same command passed:

```text
Summary [0.075s] 1 test run: 1 passed, 3677 skipped
```

The test exercises the live hook-authority projection. It proves External returns `"resume_policy":"external"` and Native keeps both the typed field `None` and the prior serialized JSON shape with no `resume_policy` key.

The first schema consistency run then failed as expected with `generated API schema artifact is stale`. Regeneration used the repository-prescribed command:

```text
HERDR_UPDATE_API_SCHEMA=1 just test-one generated_protocol_schema_artifact_is_current
```

The regenerated artifact and the subsequent non-update check both passed.

## Extension gap and upstream/removal strategy

A Herdr extension or ordinary public API client can emit `pane.report_agent_session`. It cannot safely own the core invariants required here:

- snapshot and history format versioning;
- v3-to-v4 migration and downgrade rejection;
- cold-restore plan creation and dedupe;
- preservation of policy through lifecycle conflicts and reporter replacement generations;
- admission of recognized External startup/lifecycle reporters before native process detection;
- public session-policy projection shared by pane and agent APIs.

These are server-owned runtime, persistence, and protocol contracts. Implementing them in a Smarty extension would deepen product coupling and could not protect cold restart before the extension runs.

Upstream the neutral contracts, not Smarty behavior. The downstream four-commit stack may be removed only after an upstream release includes all of the following with equivalent tests:

1. `AgentResumePolicy::{Native,External}` and public report input;
2. persistence plus v4 migration/fail-closed rules;
3. External no-native-plan restore behavior and Native dedupe preservation;
4. External reporter admission and policy preservation across lifecycle conflicts;
5. public `AgentSessionInfo.resume_policy` with Native response-shape compatibility;
6. generated schema and draft documentation.

Removal condition: the root pins a released or landed upstream Herdr candidate that passes the same compatibility and exact-binary checks. Do not delete only the enum/API commit while retaining v4 state, or only the state changes while dropping reporter admission.

## Compatibility and rollback ledger

### Snapshot v3 to v4

- Current session and history snapshot version: `4`.
- v3 snapshots omit `resume_policy`; serde defaults the field to `AgentResumePolicy::Native`.
- v4 continues to read v3 fixtures and older versioned/unversioned snapshots through the existing migration path.
- Native policy is omitted during snapshot serialization, preserving the old Native payload shape.
- External policy serializes explicitly as `"resume_policy":"external"`.

### Native policy

- `AgentResumePolicy::default()` remains `Native`.
- Omitted request and snapshot policy remains Native.
- Native restore planning, allowlisting, history suppression, and dedupe are unchanged.
- Public `AgentSessionInfo` omits `resume_policy` for Native sessions.

Focused Native checks passed:

```text
capture_contract_tracks_hook_authority_agent_session
restore_plan_respects_opt_in_and_allowlist
restore_plan_selection_suppresses_duplicates
pane_restore_startup_suppresses_history_for_duplicate_native_agent_session
```

### External policy and duplicate prevention

- External session metadata persists across snapshot capture/parse and lifecycle conflict/replacement paths.
- `restore_plan_for_snapshot` creates a native resume plan only when `resume_policy.is_native()` is true.
- Therefore an External session cannot create or reserve an `omp --resume` native plan during cold restart or replay. Repeated restore selection has no External native effect to duplicate.
- A fresh recognized sequenced External OMP report can anchor state without waiting for Herdr to detect a native OMP foreground process.

Focused External checks passed:

```text
capture_contract_versions_external_resume_policy_under_hook_authority
external_omp_session_is_persisted_without_native_resume_plan
external_session_report_anchors_wrapped_omp_without_detected_process
detected_conflict_preserves_matching_external_resume_policy
pane_info_exposes_external_resume_policy_without_changing_native_shape
```

### Future version, downgrade, and rollback

- `parse_snapshot` rejects any snapshot version newer than the running binary's supported version.
- `parse_history_snapshot` applies the same future-version rejection to history state.
- The v4 External capture test simulates a v3 reader and proves it rejects the v4 snapshot rather than silently treating External as Native.
- A real v3 binary therefore ignores/refuses v4 state instead of prematurely resuming an externally owned OMP session.
- There is no automatic v4-to-v3 down-conversion and no new backup mechanism in this patch. Rollback requires preserving the v4 state file and re-upgrading to a v4-capable binary, or intentionally starting without that state. Do not overwrite the only v4 state with a v3 writer when rollback evidence matters.
- `future_version_is_rejected` passed.

## Public API, schema, and documentation consistency

The shared `terminal_agent_session_info` projection feeds pane APIs. Agent APIs construct `AgentInfo` from the same pane projection. The four public reads therefore share one policy source:

- `pane.get`
- `pane.list`
- `agent.get`
- `agent.list`

Generated `docs/next/api/herdr-api.schema.json` now includes optional `AgentResumePolicy` references for `AgentSessionInfo` in both event and success-response schemas. `docs/next/website/src/content/docs/socket-api.mdx` shows an External example and documents Native omission. Published version docs, root README, root changelog, and stable skill files were not changed.

## Verification

### Focused commands

All final focused commands passed on production commit `87c0f9ba919f0a42c1d3347d9c64f9259c9ae889`:

```text
cargo fmt --check
just test-one pane_info_exposes_external_resume_policy_without_changing_native_shape
just test-one generated_protocol_schema_artifact_is_current
just test-one capture_contract_tracks_hook_authority_agent_session
just test-one capture_contract_versions_external_resume_policy_under_hook_authority
just test-one external_omp_session_is_persisted_without_native_resume_plan
just test-one external_session_report_anchors_wrapped_omp_without_detected_process
just test-one detected_conflict_preserves_matching_external_resume_policy
just test-one restore_plan_respects_opt_in_and_allowlist
just test-one restore_plan_selection_suppresses_duplicates
just test-one pane_restore_startup_suppresses_history_for_duplicate_native_agent_session
just test-one future_version_is_rejected
```

Each named nextest filter returned `1 passed` and no failure.

### Canonical full check

Command:

```text
just check
```

Raw final outcomes:

```text
cargo fmt --check: passed
cargo clippy --all-targets --locked -- -D warnings: passed
cargo nextest run --locked -E "all()": 3677 passed, 1 skipped
scripts.test_ui_hot_path_architecture: 6 tests, OK
integration assets: 18 passed; OpenCode state 6 passed; OpenCode TUI session 5 passed
plugin marketplace: 31 passed, 0 failed, 119 expect() calls
x86_64-pc-windows-msvc clippy with LIBGHOSTTY_VT_SIMD=false: passed
maintenance Python suites: 111 tests, OK
```

The first clean-build command completed successfully in `120.22s`. After commit `87c0f9ba` was created with the same source bytes, the exact command was rerun from that clean commit and completed successfully in `34.96s`; its nextest phase reported `3677 passed, 1 skipped` in `23.227s`. The only repeated warning was the repository's external-contributor policy reminder; no lint or test warning was promoted into a failure.

## Exact arm64 Darwin binary provenance

| Field | Value |
| --- | --- |
| Source commit | `87c0f9ba919f0a42c1d3347d9c64f9259c9ae889` |
| Source tree | `9be0f1b2ce459962f30a59d66622609e51eb7a6f` |
| Cargo.lock SHA-256 | `dd23898939daaf8b5bf1562d5025e26f57b14ed2d4769b34f8d6f80587116627` |
| `rustc` | `1.96.1 (31fca3adb 2026-06-26)` |
| `cargo` | `1.96.1 (356927216 2026-06-26)` |
| Rust host | `aarch64-apple-darwin` |
| LLVM | `22.1.2` |
| OS | Darwin `25.6.0`, arm64 |
| Cargo package | `herdr 0.8.2` |
| Cargo features | `{}`; no `[features]` table, default feature set only |
| Profile | `release` |
| Target | `aarch64-apple-darwin` |
| Binary path | `/Users/paulbettner/.herdr/worktrees/herdr/i3a-corrective-c3/target/aarch64-apple-darwin/release/herdr` |
| Binary format | `Mach-O 64-bit executable arm64` |
| Binary version | `herdr 0.8.2` |
| Binary bytes | `20458304` |
| Binary SHA-256 | `daba0a008765f8ad52465c1f8aebe66c447e00757d9c7d6eae4836e3212822d5` |

Reproducible command:

```text
cargo build --release --locked --target aarch64-apple-darwin
```

Initial clean-target outcome:

```text
Finished `release` profile [optimized] target(s) in 1m 30s
```

The exact command was rerun without source changes and returned:

```text
Finished `release` profile [optimized] target(s) in 0.81s
daba0a008765f8ad52465c1f8aebe66c447e00757d9c7d6eae4836e3212822d5  target/aarch64-apple-darwin/release/herdr
```

`Cargo.toml` and `Cargo.lock` are byte-identical to assigned frozen base `6f8f02db`; no dependency, feature, build-option, or lockfile change was made.

## Precise Integration Master proposals

C3 does not own root configuration, shared fixtures, package production, root pins, or landing. Proposed Integration Master actions only:

1. Integrate the Herdr commit sequence `cce4622f`, `10c7b17c`, `6f8f02db`, `87c0f9ba` through the repository's protected PR and Mergify workflow.
2. Rebase or reconstruct that sequence on the then-current Herdr upstream before PR if required; rerun this handoff's focused checks, `just check`, and exact release build on the resulting head.
3. After Herdr lands, update the root Herdr pin to the exact landed commit and bind the package producer to a binary rebuilt from that landed commit/tree. Do not pin this unlanded local commit as if it were landed.
4. Include this handoff and the exact landed replacement binary provenance in canonical root evidence. No C3 shared protocol fixture change is proposed.
5. Keep Smarty authorization and reconstruction in Smarty. Its only Herdr-facing behavior should be a generic External session report.

## Patch deletion and task-owned cleanup plan

- Keep this worktree, branch, handoff, and release binary until the Integration Master has consumed the commit and binary provenance.
- After protected integration is confirmed, update the shared Herdr checkout, then remove only `/Users/paulbettner/.herdr/worktrees/herdr/i3a-corrective-c3` and its local task branch under maintainer control.
- No remote branch was pushed by C3. Do not delete an unrelated remote branch.
- No agent-detection override, plugin override, external service, daemon, temporary worktree, or unrelated repository state was created by C3.
- `target/` is ignored build output. It may be removed only after the Integration Master records or reproduces the exact binary.
- Do not remove unrelated Herdr, Smarty Net, Smarty Pages, OMP, or owner worktrees.
- Remove the downstream patch stack only under the upstream release conditions in the upstream/removal section; preserve it otherwise.

## Limitations and nonclaims

- No live user-owned Herdr daemon or OMP process was restarted. Cold-restore and replay claims are bounded to the tested core snapshot/restore/state contracts.
- The exact release binary was built and hashed but not installed, signed, notarized, packaged, or activated.
- The release build target was arm64 Darwin. Windows compile/clippy passed through `just check`; no Windows or Intel macOS release binary was produced by C3.
- No automatic downgrade conversion, rollback backup, or state-file copy was added.
- No performance path was widened: the repair adds only a small optional API projection when public pane/agent info is constructed. It does not touch render, parse, detection, resize, or client-frame fanout loops.
- No root regression, package producer, root pin, shared fixture, canonical evidence, protected landing, release, deployment, or owner acceptance is claimed.
