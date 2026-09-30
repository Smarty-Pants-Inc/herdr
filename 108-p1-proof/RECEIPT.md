# herdr#108 P1 — PASS on second/final attempt; both attempts cleaned
Execution: 2026-09-30 14:49Z–15:36Z, including authorized cooldown; extended box approximately16:05Z.
Original dev-lead GO14:47Z (quoted): 'the candidate herdr 15ea987e running in parallel, with its own 0700 proof dir, sockets, XDG and session under the test-org Node HOME; staged through the Node inbox and driven through the existing forward shell, with 4 nemotron calls on test-org's stand-in key. The installed Herdr release and its units stay untouched. Stop and remove everything after, with a receipt: processes 0, proof dir removed, inbox artifact removed.'
Original net-lead approval (quoted): 'net-lead approves those 4 tiny gateway calls (test-org's stand-in key).'
Renewed net-lead decision (quoted): 'Then ONE fresh complete attempt from `init` in a NEW proof dir, after waiting ~10 min, with the same harness, model and member; I approve 4 more tiny nemotron calls. If the same upstream 503 hits again, stop, clean up, and report BLOCKED on provider availability with both attempts' evidence; don't try a third time or a different model. Keep both attempts' recordings private for my review. Your box extends to ~16:05Z for this.'
Full authorization/capture-format acceptance: AUTHORITY.md. Cooldown completed15:21:18Z; second init15:22:07Z. No third attempt.

## Acceptance ledger
PASS inputs: all five supplied files read; unchanged harness sha256852eede92fbb55feff511a37e63bad3a2983b0b0469f8de4dfdd61894888b82b.
PASS source/build: detached15ea987e984aa939d8a5a6bb74abf67444dfdba3, cargo1.98.0, Zig0.16.0, nice10, cargo build --locked --bin herdr exit0.
Build env: ZIG=/home/paul/.local/bin/zig; task-private CARGO_TARGET_DIR=<scratch>/target and ZIG_GLOBAL_CACHE_DIR=<scratch>/zig-cache.
Second binary sha256: c3b9adbc0d75f60a34d5bdf611f306fbb4176027735ca2700f06d2a5f1c7f1b1. Fresh rebuild differs from first binary hash; source SHA is identical.
PASS init/attach: existing test-org forward to idle outer shellwD:p1, UID999 HOME/home/test-org; root/home/test-org/.herdr-p108-proof.b71l3md6 mode0700; own XDG/session/socket pair; originalA=w1 seeded exactly once.
PASS pre-split: actual candidate TUI Ctrl+A then v; memberpaul; new w1:p2; real installed Node Pi call and one successful usage row.
PASS pre-tab: actual candidate TUI Ctrl+A then c, naming dialog, Return; memberpaul; new w1:p3; real Pi call and one successful row.
PASS restart: harness restart exit0; same binary; originalA=w1 labelp108-member restored without reseeding; private saved defaults assertiontrue and session mode0600.
PASS identity: server prePID3962885/start31516315 -> postPID4024838/start31535767; same candidate executable. Restored w1:p3 shellPID4024881 retainedpaul (pre-tab shellPID4010698).
PASS post-split: actual Ctrl+A then v; new w1:p4, outside restored baseline; memberpaul; real Pi call and one successful row.
PASS post-tab: actual Ctrl+A then c, Return; new w1:p5, outside restored baseline; memberpaul; real Pi call and one successful row.
PASS verify: unmodified harness exit0, FOUR CALL RECEIPTS PASS; four distinct paneIDs, terminalIDs and traces; same originalworkspace and candidate hash throughout.
PASS negativeB: originalw2 created without --env once; actual TUI split/tab pre w2:p2/p3 and post w2:p4/p5 all member=<unset>. No paid calls inB.
PASS capture: util-linux script actual terminal input/output/timing plus exact visible ANSI frames, accepted by net-lead. No raster screenshots/MP4 claimed.
PASS input join: original recording has Kitty-protocol Ctrl+A (ESC[97;5:1u), v/c/Return actions; no candidate API/CLI split/tab creation. Exact key event times preserved in recorded-key-events.json and JOIN.json.

## Four second-attempt usage receipts (all test-org / paul / nemotron-3-super / HTTP200 / endcomplete)
pre-split 15:24:10.550Z tokens469/28 trace20260930152409-4f1b40b159bd0cd8-00000005; pane w1:p2 terminal term_65cb4e3f53c325 shell4002130.
pre-tab 15:24:40.022Z tokens468/30 trace20260930152438-4f1b40b159bd0cd8-00000006; pane w1:p3 terminal term_65cb4e5ad48c86 shell4010698.
post-split 15:27:24.380Z tokens469/184 trace20260930152718-4f1b40b159bd0cd8-00000007; pane w1:p4 terminal term_65cb4ef3c6bcf9 shell4062566.
post-tab 15:27:52.247Z tokens468/60 trace20260930152751-4f1b40b159bd0cd8-00000008; pane w1:p5 terminal term_65cb4f12cdf40a shell4072522.
Second attempt: exactly4 Pi invocations/4 gateway HTTP requests, no automatic retries; usage offsets26178–27255 contain exactly these4 rows. No joins borrowed from attempt1.

## Private evidence
Second attempt: <private evidence dir>/ (0700); full archive evidence.tar; individual phase/join receipts evidence/{pre,post}-{split,tab}.json and JOIN.json.
Recordings: evidence/pre-output.ansi + evidence/pre-timing.log + evidence/pre-input.raw; evidence/post-output.ansi + evidence/post-timing.log + evidence/post-input.raw.
Replay each: scriptreplay --log-out evidence/<pre|post>-output.ansi --log-timing evidence/<pre|post>-timing.log --stream out.
ANSI keyframes: pre-split-created.ansi, pre-split-pass.ansi, pre-tab-dialog.ansi, pre-tab-pass.ansi, restored-member.ansi, post-split-created.ansi, post-split-pass.ansi, post-tab-dialog.ansi, post-tab-pass.ansi, negative-{pre,post}-{split,tab}.ansi.
Positive split/control times: pre95.917895/96.175320s; post115.530238/115.602226s. New-tab prefix/c/Return: pre124.434788/124.527626/125.014373s; post147.789751/147.877418/148.139266s.
Phase begin/end UTC, approximate Pi begin timecodes, shell/pane/terminal, argv, model, usage offsets and exact receipts are in JOIN.json plus phase.json. Pi0.87.1 launcher /var/lib/smarty-org-sandbox/test-org/hostd/current/bin/pi.
First attempt preserved privately at <private attempt-1 dir>/RECEIPT.md and its evidence/recording files; upstream failure and original receipt unchanged.
First attempt: one successful row + four failed HTTP rows from automatic retries (2 Pi invocations/5 requests). Both attempts total6 Pi invocations/9 HTTP requests. Initial four-HTTP-request limit was exceeded by retries and explicitly disclosed before retry authorization; do not call the combined run eight HTTP requests.

## Cleanup
PASS processes0: exact recorded candidate server stopped through its own socket; both server generations, candidate client, script recorder and candidate PTY processes ended; postserverPID absent.
PASS proof dir removed: /home/test-org/.herdr-p108-proof.b71l3md6 removed by test-org after private evidence export; first proof dir also removed.
PASS inbox artifact removed: /var/lib/smarty-org-sandbox/test-org/p108-artifact2.fqes2er7; first-attempt stage and failed staging artifact also removed.
PASS scratch/source worktrees/build directories removed; build/cooldown completed. Only both private evidence directories remain.
PASS installed test-org original12 process/PIDs matched before/after, including server3243772 and forward3243783. Candidate-specific zero-process export check passed. One exact-baseline snapshot assertion transiently differed; immediate diagnostic and bounded recheck matched original12, recorded in cleanup-final.json.
No installed Herdr release/unit/socket mutation, other Node control, unrelated process kill, sudo/shared permission change, credential/token-file reads, GitHub writes or round-answer.md edits.
Nonclaims: no pin/install/deployment, release/merge approval, #209 installed-path acceptance, or independent ACCEPTANCE_AUDIT. net-lead reviews private evidence before publication.
