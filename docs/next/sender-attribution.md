# Sender attribution: API now, Pi transcript display deferred

## Decision and scope

Herdr exposes per-pane sender display metadata through `pane.last_input`. It does
not ship Pi author badges, sender-attribution session entries, or changes to the
Pi integration version. This is the explicitly selected API-only fallback, not
completion of transcript author display. The accepted Pi display and queue-safe
metadata follow-up is tracked in
[smarty-dev#4078](https://github.com/Smarty-Pants-Inc/smarty-dev/issues/4078).
Herdr #3937's API scope proceeds separately; Pi display is a named follow-up, not
an unresolved part of that API scope.

The Pi analysis below concerns the public extension APIs and source shipped in
`@earendil-works/pi-coding-agent` **0.87.1**. Reassess the missing contracts before
implementing against a later version. The blocker is submission identity and
queue-bound metadata, **not** a missing custom-entry renderer.

## Herdr's implemented API contract

Request:

```json
{"id":"last_input","method":"pane.last_input","params":{"pane":"w1:p1"}}
```

`pane` is required; `pane_id` is an alias. The standard success envelope contains:

```ts
{
  type: "pane_last_input",
  last_input: { user: string | null, client_id: number, at: number } | null
}
```

`client_id` is a server-assigned connection ID. `at` is the server-recorded input
time in Unix epoch milliseconds; both are unsigned 64-bit values on the server.
The optional JSON hello `user` is self-declared. Herdr clients load their own
`identity.name`, defaulting to non-empty `USER`, then `USERNAME`; an explicit
empty name disables declaring it. This is never authentication or authorization.

The existing per-pane media tracker also holds this display annotation. It
records the **last accepted client input**, which can include mouse input, not
only keyboard typing or prompt submission. An anonymous client produces an
object with `user: null`, its connection ID, and its input time. Display
attribution persists until subsequent input, client disconnect, or pane removal;
media routing still has its separate freshness requirement. Accepted API input
invalidates display attribution (`last_input: null`) without clearing the media
routing owner. It must not inherit a previous client's display name.

This snapshot is not a message history or evidence that a particular native Pi
user message originated from the recorded client interaction.

## What Pi 0.87.1 already supports

The public [extension declarations](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/extensions/types.ts)
and [custom-entry example](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/examples/extensions/entry-renderer.ts)
provide the relevant contracts:

- `input` exposes text, optional images, `source: "interactive" | "rpc" |
  "extension"`, and optional `streamingBehavior: "steer" | "followUp"`. It has no
  stable submission ID or durable message reference.
- `message_start` and `message_end` expose the message, but not its originating
  input ID or source. Markdown transformers likewise have no submission identity.
- `pi.appendEntry()` persists a `type: "custom"` entry **outside model context**.
  `pi.registerEntryRenderer()` renders that custom entry in the interactive
  transcript. `sendMessage()` instead creates model-facing custom content and
  is not a substitute for metadata-only attribution.

See [extension state guidance](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md#persist-state)
and [TUI guidance](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/tui.md).
Public links identify the corresponding source locations; the findings are from
the inspected 0.87.1 package, not a guarantee about a moving upstream branch.

For a simple idle submission, `AgentSession` invokes input handlers before
constructing the native user message. Appending a custom entry there can persist
and render an author line before the genuine `UserMessageComponent`, without
rewriting its text. A claim that Pi cannot render metadata-only author lines
would therefore be incorrect.

## Why that idle implementation is not safe end to end

Relevant source paths are `packages/coding-agent/src/core/agent-session.ts`,
`src/core/session-manager.ts`, and `src/modes/interactive/interactive-mode.ts`.

1. **Steering and follow-up queues:** input handlers run when input is enqueued.
   An appended author entry is committed and rendered immediately, not alongside
   the later native user message. The interactive custom-entry path can insert it
   before the streaming assistant, separating it from its eventual message.
2. **Duplicate messages and reordering:** Alice submits `same` as follow-up; Bob
   submits `same` as steering. Steering can drain first. Text matching cannot
   distinguish the messages, and a FIFO author queue assigns Alice to Bob's
   message. Prompt transformations make text matching less reliable still.
3. **Compaction-delayed input:** the editor queues a submission during compaction
   before the public input hook runs. Alice's queued message may be replayed
   after Bob's later input. Querying `pane.last_input` at replay reads Bob, not
   Alice, even without interference from another extension.
4. **Consumed, cancelled, or failed input:** a later handler may return `handled`,
   or validation/preflight may fail after an author entry was appended. No
   corresponding native message need follow. A later extension-originated
   `sendUserMessage("same")` must not inherit the abandoned author marker.
5. **Deferring to message events:** rendering at `message_start` fixes immediate
   placement, but loses the original source and submission association. Looking
   up the latest pane snapshot, comparing timestamps, or assigning the next
   user message cannot recover that missing identity safely.
6. **Replay and branches:** resume and fork need the association persisted in the
   session tree. A renderer cannot reconstruct it from adjacent text alone.

Skipping every queued message would be a reduced feature needing explicit
acceptance, not a silent implementation of author-on-genuine-submission behavior.

## Minimal upstream facility

Keep native user rendering and the existing `registerEntryRenderer()`. Add a
submission-scoped, queue-bound metadata facility:

1. Emit a genuine submission event at acceptance, **before** compaction,
   steering, or follow-up queuing. Supply a stable opaque `submissionId` and
   immutable source. Preserve them through expansion, transformation, queues,
   and persistence; RPC and extension inputs retain their actual origins.
2. Let the handler attach a metadata-only custom entry to that submission. Pi
   carries the entry with the queued input and commits/emits it **immediately
   before the accepted native user message**, only when that message is
   committed. Consumed or cancelled input produces no author entry.
3. Persist the binding through resume and fork. Render the custom entry with
   `registerEntryRenderer("author", ...)` above the untouched native user message.
   Keep submission binding internal to Pi; preserve the existing author entry
   contract rather than introducing a second metadata schema. Missing attribution
   stays missing; do not infer it from another submission.

An illustrative sidecar, **not an existing API**, is:

```ts
entriesBeforeMessage: [{
  type: "custom",
  customType: "author",
  data: { name, source: "herdr-client", verified: false },
}]
```

Names must never enter native `UserMessage.content`, model/system prompts,
provider conversion, or compaction input. Do not spoof a user message, monkey
patch private TUI components, or treat a self-declared name as verified identity.
A propagated submission ID/source plus a persisted native header decorator is
an alternative, but requires a broader API than the queue-bound custom entry.

## Acceptance before enabling Pi display

Require deterministic tests for untouched user and model content, metadata-only
storage with `verified: false`, and no stale attribution for anonymous, API,
RPC, or extension input. Cover duplicate text, transformations, consumed and
cancelled submissions, independent steering/follow-up queues, delayed compaction,
resume, and fork. Then validate the real entry order in an isolated scratch Pi
profile and Herdr server. No transcript display proof is claimed by this fallback.
