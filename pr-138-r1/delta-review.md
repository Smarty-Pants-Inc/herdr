# ONE-SHOT INDEPENDENT DELTA REVIEW — herdr#138 round 1

**DELTA_REVIEW: PASS** — all five original findings are closed on the frozen candidate. No new round-1 blocker found. This is a local delta verdict, not `review/astra`, a release/installation audit, or a macOS CI result.

## Reviewed identity and scope

- Base: `100703cd93c9a2f104e1e29f808e561a3cc69ebe`.
- Final HEAD: `47bd736357dc890e74987f97953d9039a273df77`.
- Final tree: `ae58082fcb87d3a44f1443a6072c48a22645e7a2`.
- Frozen executable: `.local/4346/repair-138/artifacts/herdr-final`.
- Executable SHA256: `faca066e6f9bd89f410c676e5143ecd9e5a37883c70ba2e973d4e6d83aef8ea9`.

Independently checked HEAD/tree, clean tracked state, all 12 hashes in `frozen-source.json`, and executable hash. The 12 pinned files are exactly the delta's changed-file set. The reusable target's `debug/herdr` also matches the frozen executable byte-for-byte. Read the original `.local/4346/r1-review.md`, repository AGENTS, stack/org AGENTS, factory/current AGENTS, writer handbacks, relevant final source and evidence. No tracked edits, builds, commits, pushes, GitHub actions, agent spawns, or fleet-server operations.

Mechanical evidence: `delta-review-evidence/mechanical.json`. Independent behavioral receipts: `delta-review-evidence/receipt.json` and `integration-receipt.json`.

## Acceptance ledger and whole-path conclusions

### 1. P1 — real API/CLI stop wakes the idle receiver: PASS

`src/cli/server.rs::server_stop` → `src/session.rs::stop_active_server` → the real JSON API handler. `src/api/server.rs::handle_request_with_context` release-stores the shared stop atomic, prepares the immediate successful reply using the original ID, and nonblocking-enqueues the **actual original request and transport context** through the existing unbounded API sender. It does not wait for an App response; the response receiver is discarded. A failed send to a closed App receiver does not change the priority acknowledgement.

Normal bootstrap and handoff import both pair that sender with App's receiver and the same stop flag. Recovery listener creation also uses the existing sender/flag. `HeadlessServer::run` selects `app.api_rx.recv()`, then checks the flag before executing the selected request and at the next loop top. An already queued startup request is retained; a stop before the first check is caught by the atomic. Busy App work does not delay the API acknowledgement. Existing synchronous work is not newly preemptible, which was not the requested repair.

Shutdown still drains/rejects queued work, cleans owned socket identities, preserves save/restore behavior, and follows the existing handoff ownership/persistence rules. The readiness duplicate is run-local and drops on return; it is not a demonstrated cause of the original no-wake failure. No listener/deadline/destructor changes were made in this delta.

`failing-first.log` contains all three exact original integration failures. `p1-repaired.log` records six passes. I independently replayed the three original integration paths: **3/3 passed**, including blocked recovery's unchanged saved bytes and unavailable-restored-pane CWD/recovery behavior. Independent stop-control unit coverage passed for busy/unconsumed App and closed receiver. Two direct frozen-binary API probes, startup and fully idle, acknowledged with the original ID, exited 0, and removed both sockets without SIGINT, client-listener connections, or any post-stop API request: **480.74 ms / 71.50 ms** upper bounds.

### 2. P2 — detected same-path directory replacement/removal gap: PASS

`git_watch.rs` separates relevant events from structural invalidation. Structural callbacks set retained atomic rearm/discovery hints; app-loop `sync` consumes the rearm hint and retires/reinstalls inode-bound registrations even when desired path/mode are unchanged. `GitFilesChanged` consumes the discovery hint and requests identity refresh. This runs through scheduler → worker → normal App application, not a separate status reader.

Exact nonrecursive `.git` parent markers survive an already-applied missing-directory interval for previously discovered consumers. Creation can therefore rediscover/rearm after the gap. Ordinary atomic HEAD/index replacements are relevant but do not satisfy the structural predicate; the repair does not rebuild every watch on those writes. Structural hints do not advance the independent discovery safety timestamp.

The original replacement failure and additional removal-gap failing-first log are present. Independent real-native regressions passed for `.git/refs` and `.git` replacement with retired inodes retained, subsequent real ref/branch changes, and removal → apply missing state → recreate → real commit. Symlinked refs target writes and retargeting also passed.

### 3. P2 — actual external config dependency graph: PASS

`workspace/git/config.rs::ConfigReader` remains the authority. `GitStatusCacheEntry::config_dependency_paths` exports cached effective config inputs, including missing paths and logical/canonical symlink paths; it does not parse or access the filesystem. Worker completion inserts cache updates, applies statuses, then reconciles dependencies and native topology even when roots/dependency names are equal. Root-set changes separately reconcile from **current consumers**, so retired workspace cache entries no longer keep their config watches alive, including after an in-flight completion.

Dependencies use exact-file nonrecursive parent watches, nearest-existing-parent migration for missing hierarchies, and logical file/directory-symlink parent sentinels. `native_target` normalizes registration AND callback-filter directory prefixes before deduplication, without canonicalizing away exact logical leaves. Shared inode registrations cannot be removed by retiring a different alias spelling.

Independent regressions passed for config-only include atomic replacement and graph growth, user fetch-refspec changes, first XDG ancestor creation and later migration, file/directory symlink retarget then new-target edits, alias partial removal, and the final removed-workspace cached-dependency counterexample. Tests assert unchanged repository config bytes in config-only cases. The normalization-disabled diagnostic correctly fails at the first XDG-directory creation; final code uses normalization and the independent case passes.

### 4. P2 — growing demand gets its initial read: PASS

`App::apply_live_config` compares old/new demand and marks newly required fields due. Both direct reload and `HeadlessServer::reload_server_config` reach this shared function. Watch reconciliation handles new roots and consumer reactivation. In-flight expansion retains `git_refresh_due_after_in_flight`; completion leaves an immediate rerun due rather than treating branch-only output as sufficient for the new field.

Consumer/watch roots use stable resolved workspace CWD, while config/cache lookup still uses canonical cache-key hints. Learning the cache key therefore does not manufacture a new consumer and phantom initial read. Empty-root synchronization returns before demand-growth marking and retains the completion timestamp; demand removal releases watches/deadlines.

Independent actual disk-config reload regression passed for populated branch-only → Branch + GitStatus on a quiet repository, both idle and with a real branch-only worker in flight. New-root/reactivated-consumer regression passed. Final targeted/ALL evidence also covers the retained 59/60-second safety behavior and empty-consumer completion path.

### 5. P2 — native common-directory identity: PASS

Discovery's existing `GitWorktreeInfo` is re-exported through the workspace module. Watch construction uses its native `repo_root`, `git_dir`, and `git_common_dir` PathBuf values directly. No `space.key` reconstruction remains in the watcher. Canonicalization stays native; display strings remain presentation/grouping data only.

Independent discovery and App/native-watch tests passed under a non-UTF-8 ancestor with relative gitdir/commondir markers. A common upstream ref update from the other checkout changes the linked checkout's applied ahead/behind result before the safety refresh.

## Evidence, attribution and limits

- Independently executed **17/17** repair/substrate tests from the already compiled final unit artifact, then **3/3** exact original CI integration paths. Logs are `delta-review-evidence/case-*.log` and `integration-*.log`. No recompilation or full-suite rerun.
- Independent probes used isolated HOME/XDG/session/socket state, a real short UID-owned 0700 `/tmp` root, no inherited `PI_CODING_AGENT_DIR`/`PI_CONFIG_DIR`/`HERDR_*`, pinned PATH/Zig, finite subprocess deadlines, and `timeout 45m systemd-run --user --scope -p CPUQuota=800%`. Direct server children were reaped and their PIDs are absent. Private scratch is retained, with its original path in the receipt, for attribution; no process remains there.
- One reviewer setup attempt selected the production executable instead of the libtest executable. It rejected the test filter before starting any server. This is retained as `test-0.log`, not counted as a candidate failure or pass. Corrected runs require nonzero test counts and successful libtest output.
- Read final validation logs/exits: fmt, clippy, targeted, ALL, build all exit 0; **60/60 across 17 binaries**, then **4125/4125 across 17 binaries**, with 9 existing ignored tests. Earlier fd-alias input-log refusals do not justify weakening security; final runner uses a real owned short TMPDIR and no input-security source is changed.
- Final real PTY proof pins match. Captures show branch/ahead/behind changes and the final anchor-only screen. Maximum recorded latency is **126.30 ms**. Mechanically checked 100 distinct workspace IDs/paths, exact unchanged FD and native-watch maps (**23→23 / 9→9**), and no retired-path watches. This ordinary real-PTY proof supports lifecycle/presentation; it is not substituted for the counterexample regressions above.
- CPU raw case JSON agrees with summaries: median **28.0830% → 30.4997% of one core**, **+8.6054%**, in paired 3×60-second 100-workspace/actual-client samples. No performance-win claim is supported. Detector cost remains outside this review in #4366. Also, the writer's one-demand-scan description is imprecise: nonempty-root sync computes demand before root collection and again in `request_git_demand_growth`; this is a small optional optimization, not an unclosed finding.
- Still allowed by the existing contract: an initially non-Git root, dropped native hint/full channel, or unavailable backend can rely on the unchanged 60-second safety net. Branch-only initial reads need not compute upstream config dependencies; expansion requests that computation. Clientless headless operation still suppresses Git watches/refresh work. None is a substitute for, or contradiction of, the repaired detected-event/demand paths.
- No production timer/sleep/timeout increase, protocol/codec/API-shape change, detector change, or new dependency is present. macOS CI, publication, repository `review/astra`, merge queue and installation remain integrator-owned external gates, **not locally certified**.
- Lead's later update adds a separate release CPU gate comparing master `dee2` with `47bd`: build underway, no measurement yet, separate build + six runs deadline 03:35 UTC. Source/head/pins remain frozen. This delta PASS neither completes that gate nor establishes final READY or a performance win.

**QUIESCENT** — review writes complete; all own commands/probes finished. Final HEAD `47bd736357dc890e74987f97953d9039a273df77`; report `.local/4346/repair-138/delta-review.md`.
