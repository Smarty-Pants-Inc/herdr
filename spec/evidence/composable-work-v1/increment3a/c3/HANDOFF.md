# Increment 3A C3 — Herdr Seam Closure Handoff

## Status and exact identities

- Mission: `smarty-pants-increment3a-corrective-closure`
- Result: the corrected C3 source chain, public API/schema compatibility, layout idempotency persistence, live-handoff identity, plugin shutdown, OMP maintenance replay, and exact-source preview handoff are evidenced below. Protected landing and root pin/package work remain Integration Master responsibilities.
- Upstream base: `e205b0354a6e7daa1ba61ef0d42a794c263181e0`.
- Corrected source parent: `15d2284014573c3ed7d17296304b97b5e4e448d6`; tree `10edb0a493e33f79ca084f8cf0bc686aabdbf3d5`.
- Corrected source commit: `400a33af08fa217e78ea435265ba0aaec5c52a23`; tree `c0ba85bc811641e32f06c7d938135d3329dbfe0a`.
- Branch: `i3a-corrective/c3-seam-closure`.
- Evidence commit: the next local commit. Its SHA and tree are reported by the Integration Master because a commit cannot contain its own SHA.
- Worktree: `/Users/paulbettner/.herdr/worktrees/herdr/i3a-corrective-c3`.
- No push, pull request, merge, install, release, deployment, root-pin change, or Increment 3B work occurred in this lane.

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

## Corrective closure addendum

Corrected source commit `400a33af08fa217e78ea435265ba0aaec5c52a23` closes the independent-review findings against the earlier C3 candidate:

- `scripts/preview.py` and its tests require semantic verification v3 to bind both exact Git-tree archives, `herdr-source.tar` and `omp-source.tar`. Historical v2 one-archive verification is accepted only when the independently supplied paired build ID predates the 2026-08-31 v3 cutoff; a current or future manifest cannot select v2 as a downgrade.
- `src/app/api/plugins/runtime.rs` owns and joins stdout/stderr readers. Unix pipes are nonblocking; Windows readers use `PeekNamedPipe`; stop and join remain bounded even when descendants inherit the pipe. Application shutdown cancels every plugin runtime before joining any worker.
- `src/server/handoff.rs` emits handoff format v3 with the active layout idempotency epoch, accepts genuine epoch-free v2 state, and rejects v2/v3 epoch mismatches. A pre-epoch importer rejects v3 before replacing the live server.
- `src/terminal/state.rs` and `src/app/api.rs` accept a matching authorized External ownership report as confirmation of a deferred matching Native resume only when no attempt PID is tracked and the local peer is not retired. Real remote API reports have no local peer PID and retain the confirmation path; Native, live-attempt, mismatched, stale, and retired reports remain fenced.
- `src/server/omp_maintenance.rs` replays completed acquire/release operation identities before evaluating a later lease conflict, so lost responses remain idempotent after ownership advances.
- `src/app/api/layouts.rs` returns a reconcile miss without allocating a durable cancelled receipt. Reconcile-only probes cannot exhaust the 1,024-entry layout receipt capacity or fence a later real apply.

Exact corrected-source verification:

```text
cargo fmt --check
PASS

cargo clippy --all-targets --locked -- -D warnings
PASS

python3 -m unittest scripts.test_preview scripts.test_preview_publisher
PASS — 82 tests

python3 -m unittest \
  scripts.test_agent_detection_manifest_check \
  scripts.test_changelog \
  scripts.test_config_reference_check \
  scripts.test_docs_translation_parity \
  scripts.test_hermes_integration_asset \
  scripts.test_package_windows_conpty \
  scripts.test_preview \
  scripts.test_preview_publisher \
  scripts.test_unix_installer \
  scripts.test_vendor_libghostty_vt \
  scripts.test_vendor_portable_pty
PASS — 172 tests

cargo nextest run --locked --no-fail-fast \
  --status-level fail --final-status-level fail \
  --failure-output final --success-output never
BASELINE-ATTRIBUTED — 4,073 tests ran: 4,069 passed, 4 failed, 1 skipped.
The four failures were `events_subscribe_streams_output_and_agent_status_events`,
`live_server_holds_one_pty_master_fd_per_pane`,
`multi_client_broadcasts_frame_updates_to_all_clients`, and
`multi_client_keeps_navigation_and_input_routing_independent`.
An earlier isolated run against clean upstream base `e205b0354` reproduced the same four failures; none touches the corrected paths above.

Focused corrected contracts:
fresh_reconcile_keys_do_not_exhaust_idempotency_capacity — PASS
reconcile_without_receipt_does_not_fence_later_apply — PASS
app_shutdown_does_not_wait_for_setsid_descendant_output_pipes — PASS
accepted_external_report_confirms_deferred_native_resume_without_attempt_pid — PASS
matching_resume_report_requires_live_attempt — PASS
live_handoff_preserves_layout_apply_idempotency_epoch — PASS
lost_response_acquire_retry_replays_after_later_lease_operation — PASS
lost_response_release_retry_replays_after_later_lease_operation — PASS
ownership_capability_is_private_idempotent_and_replay_safe — PASS
plugin_completion_kills_descendants_that_outlive_the_direct_child — PASS

git diff --check
PASS
```

### Final corrected-source verification

These final gates ran against source commit `400a33af08fa217e78ea435265ba0aaec5c52a23` and tree `c0ba85bc811641e32f06c7d938135d3329dbfe0a` before the evidence-only commit:

```text
cargo fmt --all --check
PASS

cargo clippy --all-targets --locked -- -D warnings
PASS

python3 -m unittest scripts.test_preview
PASS — 59 tests

cargo nextest run --locked -E '<nine final resume/plugin/handoff/epoch regressions>'
PASS — 9 tests, 0 failures

just windows-lint
PASS — x86_64-pc-windows-msvc production clippy

git diff --check
PASS
```

Independent read-only repair reviews found and drove closure of remote confirmation, reader/thread ownership, Windows pipe polling, shutdown ordering, handoff epoch/version compatibility, and semantic-verification downgrade gaps. Final task `herdr-fourth-independent-review` returned `CLEAN` against the complete diff from evidence parent `15d2284014573c3ed7d17296304b97b5e4e448d6` to source commit `400a33af08fa217e78ea435265ba0aaec5c52a23`. It changed no files. The repository-wide nextest baseline remains the same four unrelated failures documented above.

The implementation task was delegated to OMP task agent `herdr-correctness-fixes`, pinned to `terra-high`; the Integration Master re-read the material changes and reran the exact committed-source gates above. The later final root review and package bind the evidence commit and rebuilt binary to the complete landed constellation.

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

The generic Herdr patch and its public API repair were rebased unchanged in behavior. Reflog/history supplies the following old-to-rebased mapping:

| Pre-rebase commit | Rebased commit | Contract |
| --- | --- | --- |
| `cce4622faabc48617ca22bc0d24cbf707a42f7b5` | `861dffd2568946ea6fb63e564ebdd69e23dd2c84` | `fix: support externally owned agent session restore` — adds product-neutral `AgentResumePolicy::{Native,External}`, public `pane.report_agent_session` input and CLI plumbing, persistence/restore policy, terminal propagation, schema/docs, and External no-native-plan coverage. |
| `10c7b17c890b7a8c955d6315a007959117b6f89e` | `734af1d16d077811d96ca2eea1ff0c5ad8e48a28` | `fix: preserve external resume policy` — moves session and history snapshots from v3 to v4, proves a v3 reader rejects v4 policy state, and preserves External through detection conflicts and replacement-generation state. |
| `6f8f02db469af0f2592b7ba5ab7b9d5b8ce7a811` | `67d5f2a3b81fcf4558404826081bb4d9c2f6e30c` | `fix: accept externally managed omp reporters` — allows fresh sequenced recognized External OMP reports to anchor without premature native process detection. |
| `87c0f9ba919f0a42c1d3347d9c64f9259c9ae889` | `633b9d66c446998fd5f1532f05c15748f64cb070` | `fix: expose external agent resume policy` — adds optional public `AgentSessionInfo.resume_policy`, emits only `external` so Native JSON shape stays unchanged, preserves live policy projection, and updates schema and draft docs. |
| `88dc1ea5406cbc63a1fbbf210d647b904a2df731` | `81ff9b2fbc82f3f8a7288c1d979a8d299927a0f7` | `docs: record C3 Herdr verification evidence`. |

The production source parent is the fourth rebased commit. The fifth commit is the evidence candidate; its exact commit/tree identity is recorded above and bound externally, not asserted as the identity of this update commit.

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

The pre-rebase fail-first record then showed the same command passing:

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

## Earlier C3 verification record

The sections below preserve the earlier rebased-candidate verification and patch provenance. They are historical evidence, not the current corrected-source identity. The authoritative corrected-source results are in the addendum above; final binary identity is rebound by the Integration Master after the evidence commit and protected landing.

### Earlier focused commands

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

### Canonical full check

The final rebased check log is `/private/tmp/smarty-i3a-corrective-logs-0ym3mh/section7-herdr-final.log`. Its `just check` output records:

```text
cargo fmt --check: passed
cargo clippy --all-targets --locked -- -D warnings: passed
cargo nextest run --locked -E "all()": 3383 passed, 1 skipped
scripts.test_ui_hot_path_architecture: 6 tests, OK
integration assets: 16 passed; OpenCode state 6 passed; OpenCode TUI session 5 passed
plugin marketplace: 31 passed, 0 failed, 119 expect() calls
x86_64-pc-windows-msvc clippy with LIBGHOSTTY_VT_SIMD=false: passed
maintenance Python suites: 127 tests, OK
```

The log's locked arm64 release build completed successfully and emitted the candidate binary hash recorded below. Its only warning was the repository's external-contributor policy reminder; no lint or test warning became a failure.

## Earlier arm64 Darwin binary provenance — superseded

This binary predates corrected source commit `b9d8582a5b24271a4eb62963a789921ae960e0b0` and must not be used or claimed as the corrected or landed artifact. The Integration Master rebuilds and records the exact corrected and landed binary separately.

| Field | Value |
| --- | --- |
| Production source parent | `633b9d66c446998fd5f1532f05c15748f64cb070` |
| Production source tree | `7d3041fa202af2db63c2321d2bf06190183e2552` |
| Candidate evidence HEAD | `81ff9b2fbc82f3f8a7288c1d979a8d299927a0f7` |
| Candidate evidence tree | `9529e5808de55f2e9a760b39b5f86e07fcc40d96` |
| Cargo.toml SHA-256 | `49a3903d4e98b13db6913b52227f8400db63fa135007f3f39c4103fb6ba3b627` |
| Cargo.lock SHA-256 | `a827ec0ed9dd4593ad9328fe9447edbbcd7eb5e002274d3b7dab14644da7b3fe` |
| `rustc` | `1.96.1 (31fca3adb 2026-06-26)` |
| `cargo` | `1.96.1 (356927216 2026-06-26)` |
| Rust host / target | `aarch64-apple-darwin` |
| OS | Darwin `25.6.0`, arm64 |
| Cargo package | `herdr 0.8.2` |
| Profile | locked `release` |
| Verified binary path | `/private/tmp/smarty-i3a-corrective-c3-candidate/herdr-rebased-81ff9b2f` |
| Binary format | `Mach-O 64-bit executable arm64` |
| Binary version | `herdr 0.8.2` |
| Binary bytes | `19329904` |
| Binary SHA-256 | `8d45b97197a1114a73c53a008ee46d4fddc91334a307f091e5305c86ba509288` |
| Final verification log | `/private/tmp/smarty-i3a-corrective-logs-0ym3mh/section7-herdr-final.log` |

The verified artifact filename carries the full eight-character candidate prefix (`81ff9b2f`). The supplied seven-character spelling `herdr-rebased-81ff9b2` was not a filesystem path; its verified candidate is the file above.

Reproducible locked release command:

```text
cargo build --release --locked --target aarch64-apple-darwin
```

Direct verification recomputed both manifest hashes from candidate commit `81ff9b2` and confirmed they match this worktree. It also recomputed the candidate binary SHA-256 and byte count, confirmed the Mach-O arm64 format and `herdr 0.8.2` version, and confirmed the local `rustc` and `cargo` 1.96.1 toolchain. The source-dependent identity remains the externally bound `81ff9b2` candidate above; this documentation update does not substitute its own commit identity.

## Precise Integration Master proposals

C3 does not own root configuration, shared fixtures, package production, root pins, or landing. Proposed Integration Master actions only:

1. Integrate the rebased source sequence `861dffd`, `734af1d`, `67d5f2a`, and `633b9d6` through the repository's protected PR and Mergify workflow; retain `81ff9b2` as the externally bound evidence candidate.
2. If the protected landing requires another rebase, rebuild and rebind the landed source commit/tree, rerun the focused checks and `just check`, and produce a new locked arm64 release binary; do not reuse the `81ff9b2` binary as if it came from a different tree.
3. After Herdr lands, update the root Herdr pin to the exact landed commit and bind the package producer to a binary rebuilt from that landed commit/tree. Do not pin this unlanded local candidate as if it were landed.
4. Include the root proof/package binding for exact candidate `81ff9b2` and the exact landed replacement binary provenance in canonical root evidence. No C3 shared protocol fixture change is proposed.
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

## Protected-landing reconciliation addendum (2026-09-02)

The delivery branch was reconciled with current protected `origin/master` (`18b56893e1c329c5e2ca9bd7083bf8858879e0a5`) before landing. The candidate retains the C3 source chain and incorporates the intervening immutable-release, merge-queue, and protected two-phase preview-promotion fixes. Reconciled source commit `cb2a36871944a1355dbefeae01328d40fc61838b` has tree `d5cd384e51e54aa769d7c0a6941e5aa2a786c1d5`; `origin/master` is its ancestor and PR `Smarty-Pants-Inc/herdr#43` is mergeable.

The reconciliation used an ancestry merge plus ordered replay of all 13 protected-master commits. Candidate preview tests remain the superset; their superseded release lookup and promotion assertions now check the current immutable-release and protected Phase A/Phase B promotion contracts.

Verification against `cb2a36871944a1355dbefeae01328d40fc61838b`:

```text
cargo fmt --all -- --check
PASS

cargo check --workspace --all-targets
PASS

cargo clippy --workspace --all-targets -- -D warnings
PASS

python3 -m unittest scripts.test_preview scripts.test_preview_publisher scripts.test_preview_promotion
PASS — 117 tests, 0 failures

git diff --check origin/master...HEAD
PASS
```

This handoff-only evidence commit follows the reconciled source commit. Protected landing, root pinning, landed proof, packaging, and Increment 3B state remain Integration Master responsibilities.
