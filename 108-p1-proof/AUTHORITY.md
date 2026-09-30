# Second and final attempt authorization
Received from net-lead at approximately 2026-09-30T15:10Z; extended box approximately 16:05Z.

Original dev-lead GO at 14:47Z, quoted by the assignment:
> the candidate herdr 15ea987e running in parallel, with its own 0700 proof dir, sockets, XDG and session under the test-org Node HOME; staged through the Node inbox and driven through the existing forward shell, with 4 nemotron calls on test-org's stand-in key. The installed Herdr release and its units stay untouched. Stop and remove everything after, with a receipt: processes 0, proof dir removed, inbox artifact removed.

Renewed net-lead decision, verbatim:
> correct to stop; don't bypass the harness. The 503 auth_unavailable ('Nvidia Service temporarily overloaded') is upstream provider availability, not the candidate: the tab's shell did retain member paul, which is the thing under test. DECISION (net-lead, 15:0xZ): finish the candidate-only stop and FULL cleanup of this attempt, and write its receipt (processes 0, proof dir removed, inbox artifact removed). Then ONE fresh complete attempt from `init` in a NEW proof dir, after waiting ~10 min, with the same harness, model and member; I approve 4 more tiny nemotron calls. If the same upstream 503 hits again, stop, clean up, and report BLOCKED on provider availability with both attempts' evidence; don't try a third time or a different model. Keep both attempts' recordings private for my review. Your box extends to ~16:05Z for this.

Capture acceptance, verbatim:
> util-linux `script` with timing plus the exact ANSI key-frame snapshots is acceptable as the recording (a real terminal session). Keep the typescript and timing files so it can be replayed with scriptreplay. Using member paul for the four nemotron calls is fine; I'll keep that member/model quiet. Continue.

Cooldown: deterministic Jev program 94da2f1d-179d-47a4-b8a8-7a6a16f28d1a; 530000ms from 15:12:28Z; completed 15:21:18Z with zero inference and one counted program.sleep call. No candidate init/model calls before cooldown receipt. Build preparation alone runs in parallel.
Prior attempt evidence remains at /tmp/herdr108-evidence.QGt8qF; its runtime, proof dir, stage and source/build scratch were removed.
