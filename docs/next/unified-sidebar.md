# One sidebar across the fleet

Design for Smarty Dev #4283. Owner: herdr-lead. **Design only:** no implementation,
installation, access change, or m5 proof is claimed in this round.

**Why this fits:** Paul's #4283 ask follows #3832's per-host visibility work. One
org > project > lane tree lets him supervise the fleet by its work, not its
machines; it follows the fleet's principles of less code and proof on his real path.

## Decision in brief

Build an **opt-in client-side grouped view** over Herdr's existing endpoint
connections. Keep each server's resources and control path separate. Group their
workspace rows by explicit org/project metadata; show Ryzen 1–4 only as small
host tags. Reuse `workspace.report_metadata` and the workspace tokens already in
the snapshot rather than change the protocol. Put the grouping policy in our
launchers, not a Smarty-specific dependency in Herdr.

First demonstrate one real project spanning Ryzen 1 and a Ryzen 3 lane on m5,
with Playful alongside it. Then apply the same view to every fleet workspace on
Ryzen 1–4. A first-project demonstration is an intermediate slice, **not** full
#4283 acceptance.

Paul has already chosen the hierarchy. There is no new access, spending, or
architecture decision he must make for this design. Ask him only if we propose
to change that hierarchy or make this view the default for other Herdr users.

## Evidence and limits

Inspected Herdr at `77eaf011` (the local `master` baseline), and the supplied
`~/herdr-lanes/i4283.md`, `i3832.md`, and `org.json`. Stack references below are
from `~/lanes/herdr-4283`, at
`331e230e47b702503247c49861fadf852e41c1ae` before this work.

The supplied #3832 record explains m5's original two Ryzen 1 session profiles
and the addition of Ryzen 2/3/4 lane-server profiles. The staged stack
`setup/herdr/orgs` still has only `smarty-dev` and `PLAYFUL`; it is not evidence
that the deployed five-profile setup is absent. Inventory m5 before the build
and preserve its actual profile IDs. No live m5 inspection was performed here.

Upstream investigation is **checkout-only**: the connecting-machines and
socket-API docs, configuration docs, AGENTS, README/CHANGELOG, and `.github`
material. No issue-body archive or roadmap for an org/project tree was found in
this checkout. No web lookup, upstream contact, GitHub write, or push was made.
This cannot establish what upstream might plan outside this snapshot.

## 1. What the sidebar does today

### Connections and projections

- [Connecting machines](website/src/content/docs/connecting-machines.mdx)
  documents one saved profile per **SSH target + remote session**, not per
  physical host and not all sessions on a host. Thus Ryzen 1 `smarty-dev` and
  Ryzen 1 `playful` are separate endpoints, even though they share a host.
- [EndpointCatalog](../../src/client/endpoint/catalog.rs) persists an opaque
  profile ID, label, target, session, and enabled flag in
  `client/endpoints.json`; selection is separate. Duplicate target/session
  profiles can have distinct IDs. Its JSON structs reject unknown fields, so
  adding grouping data directly to this file is not backward-safe.
- [EndpointRegistry](../../src/client/endpoint/registry.rs) already holds
  several transports keyed by `ClientEndpointId`, with a connection generation,
  negotiated methods/capabilities, health state, and one active input endpoint.
- [ClientShellEndpoint](../../src/client/shell/endpoints.rs) holds each
  endpoint's label, status, cached snapshot, and snapshot generation. The shell
  keeps Local plus the saved profiles. Inactive connected endpoints update
  workspace/agent metadata without streaming their terminal screens.

### Visible hierarchy

[render.rs](../../src/client/shell/render.rs) chooses the endpoint sidebar when
there is more than one endpoint. The expanded
[endpoint_sidebar.rs](../../src/client/shell/endpoint_sidebar.rs) builds
`Endpoint` rows followed by each endpoint's `Workspace` entries, headed
“machines.” It does not merge matching projects across endpoints. Collapsed
mode also starts with endpoint rows. Connection errors and reconnect badges
live on those machine rows today.

Within **one snapshot**, [sidebar.rs](../../src/client/shell/sidebar.rs)
`workspace_entries` groups by `worktree.key`. A group needs at least two
members and a non-linked-worktree root. It emits that root followed by the
children; collapse retains the focused child. Groups without a root remain
flat. Collapse state is endpoint-specific for remote groups.

The key comes from the canonical Git common-directory path, through
[Git discovery](../../src/workspace/git/discovery.rs), API `repo_key`, and the
[server snapshot projection](../../src/server/client_shell.rs). It is a
**host-local repository identity**, not a portable project ID. Equal paths on
two hosts can mean different repositories; copies of one project can have
different paths. It must not become the cross-host grouping key.

The lower agent panel is already federated:
[aggregate_navigation.rs](../../src/client/shell/aggregate_navigation.rs) and
[endpoint_agents.rs](../../src/client/shell/endpoint_agents.rs) build
endpoint-qualified agent rows and preserve views, sorting, and stale state.
The existing `machine` agent-row token uses the **endpoint label**. A profile
called `Smarty Pants` is not a physical-host tag.

### Names and labels

- [Workspace display names](../../src/workspace.rs) prefer `custom_name`,
  otherwise use cached Git/cwd-derived identity. Create/rename and worktree-open
  labels can override them. Indented child rows without a custom label prefer
  their branch name, stripping `worktree/`.
- [workspace_info](../../src/app/creation.rs) publishes that label, metadata
  tokens, and worktree membership. `src/server/client_shell.rs` passes them to
  `ClientShellWorkspace` and sets `custom_label` from server state.
- [Agent naming](../../src/app/agents.rs) stores a registered agent name on the
  terminal. The snapshot separately carries `name`, detected `agent`,
  `display_agent`, reported/terminal titles, status, and tokens. In
  [agent_sidebar.rs](../../src/client/shell/agent_sidebar.rs), the displayed
  agent label prefers `display_agent`, then `name`, then `agent`, then `title`.
  These are different facts: an agent kind or title is not a lead or project
  identifier.
- Stack `bin/smarty-role::lane_pane` finds a non-linked root whose `repo_key`
  matches the lane's Git common directory, then calls
  `herdr worktree open --workspace ROOT --path LANE --no-focus --label NAME`.
  `--herdr-name` supplies the agent name. Merely creating a workspace at a
  worktree cwd does not give it this nesting membership.
- Stack `bin/smarty-herdr orgs sync` matches target/session, keeps profile IDs,
  and sets table labels. `setup/herdr/orgs` configures connections; it does not
  encode an org/project for every workspace.

**Focus/read/prompt already cross endpoints.** A row carries endpoint ID plus
workspace/pane ID. [focus_or_activate](../../src/client/shell/endpoint_navigation.rs)
focuses the active endpoint or emits `ActivateEndpoint` for another one.
[shell_runtime.rs](../../src/client/shell_runtime.rs) and the
[activation protocol](../../src/client/endpoint/activation/protocol.rs) fence
input during switching, verify generation/boot/revision and the matching
surface, then enable input on the target. The screen comes from that server;
typing prompts that server's pane. `herdr --machine <profile-id> agent read` and
`agent prompt` retain explicit profile routing. UI selection does not retarget
CLI commands already running inside another pane.

## 2. Where a portable grouping key comes from

Separate **display grouping** `(org_id, project_id)` from **resource routing**
`(endpoint_id, workspace_id/pane_id)`. Keep the existing generation and boot-ID
checks. Org, lead names, purpose, issue text, and host hints confer no authority.

| Source | Benefit | Cost / failure mode |
| --- | --- | --- |
| Workspace label convention, such as `Smarty Pants / Herdr / unified sidebar` | Already transported; can seed a quick demonstration | Rename-sensitive, parsing/escaping rules, truncation, and collisions; clutters readable names. Never infer identity from arbitrary labels. |
| Explicit workspace metadata from launchers | Portable across hosts; independently editable names; no client knowledge of org files | Must stamp existing workspaces and restore them after restart. **Existing tokens provide this field today.** |
| `setup/org.json` plus a repository/path mapping | Reuses org name, keeper project pins, repo roots, repositories, and role `sets` | This file is one org's configuration, not a universal project catalog. Paths differ remotely; a project may own several repos. Issue repo is not necessarily the project. m5 should receive display keys, not the whole principal/session configuration. |

Use the third source **inside the stack launcher/reconciliation path** to
produce the second. Keep a small explicit assignment mapping where existing
pins do not identify the project. For example, `project=code` may own several
repositories, while `issues_repo=.../smarty-dev` can track work for another
product. Prefer assignment project ownership over guessing from issue repo.

The supplied `org.json` has Smarty Pants, Paul/Kate org instances, and keeper
project pins, but no `herdr-lead` pin and no complete Playful/TriStar catalog.
Both principals belong under Smarty Pants; do not create separate Paul/Kate
orgs. Put their root workspaces in an “Org agents” section within that org,
without pretending they are product lanes. Obtain Playful/TriStar keys from
those orgs' own configuration or explicit owner mapping; never fabricate them.

### Remote lanes inherit the lead's project, not the host's repository name

Stack `bin/smarty-lane-ryzen2` saves the lead's Fabric session in `.local/lead`
and embeds it in the assignment. It creates `~/lanes/LANE` as a worktree of
`~/smarty/smarty-pants`, selects the host's Herdr session, and invokes
`smarty-role --set agent=... --herdr-name LANE --worktree ...`. This staged
launcher does **not** transmit a portable org/project identity. The `agent`
role setting and Fabric return address are not workspace grouping metadata.

Resolve org/project **at the assigning lead**, then pass the resolved keys and
readable labels with the launch packet. The receiver stamps the lane even if
its infrastructure checkout is `smarty-pants`. This lane illustrates the
problem: stack workspace `~/lanes/herdr-4283` hosts design work for the separate
Herdr checkout. Repository-path-only grouping would put it under the wrong
product. `.local/lead` is useful for recovery but a rotating session UUID must
not be the permanent project key. Missing/ambiguous ownership stays visibly
ungrouped until reconciled; never silently guess from a lane prefix.

### Launcher contract for #4282

This is the short implementation contract for l4282n's readable-name change;
**no launcher changes are made by this note**. Emit these exact workspace token
keys in one `workspace.report_metadata` call, with `source=smarty.launcher`,
no TTL, and no sequence unless the writer maintains a monotonic sequence.
Namespaced keys use underscores because the current API does not allow dots.

| Token | Meaning / example for this lane |
| --- | --- |
| `smarty_org_id` | Stable org slug: `smarty-pants` |
| `smarty_org_label` | Readable org name: `Smarty Pants` |
| `smarty_project_id` | Stable project slug within that org: `herdr` |
| `smarty_project_label` | Readable project name: `Herdr` |
| `smarty_lane_id` | Stable assignment/worktree identifier: `herdr-4283` |
| `smarty_lane_purpose` | Short task purpose: `Unified fleet sidebar design` |
| `smarty_issue` | Fully qualified tracking issue: `Smarty-Pants-Inc/smarty-dev#4283` |
| `smarty_host` | Normalized execution host: `ryzen1`, `ryzen2`, `ryzen3`, `ryzen4`, or `m5` |
| `smarty_role` | `org-agent`, `project-agent`, or `worktree-agent` |
| `smarty_lead` | Readable owning lead name: `herdr-lead`; display hint only |

Set the actual host, not the example host from an assignment template. For org
roots, omit/clear project and lane keys; for project roots, omit/clear lane
keys. Omit/clear issue or lead if genuinely absent. Clear obsolete contract
keys on reassignment, without touching unrelated metadata. Stamp all ten keys
at once when present (within the API's 16-key request limit). Keys are at most
32 ASCII characters; values are at most 80 characters after normalization.
Reject oversized/invalid identity slugs before sending—the API truncates long
values, which must not silently merge identities. Shorten display purpose and
labels deliberately. Keep workspace/agent labels readable and separate.

**When:** (1) after creating/opening a workspace and before its first prompt or
launch READY; (2) on reuse/adoption/attach, even if the workspace already exists;
(3) whenever project ownership, purpose, issue, or host changes; and (4) in the
existing keeper/factory workspace reconciliation after a server restart/restore,
once the new server is ready and current workspace IDs have been enumerated.
Match restored workspaces to the durable assignment plus local checkout/worktree
identity, not saved `wN` IDs or display names. Re-report idempotently on every
reconciliation, so a missed restart event cannot leave metadata absent. Reuse
the existing reconciliation path; do not start another background service.

**Restore is a required path, not future polish.**
[restore.rs](../../src/persist/restore.rs) initializes workspace metadata empty;
[MetadataTokens](../../src/metadata_tokens.rs) without TTL survive only that
runtime, not restart. Preserve resolved assignment values in the launcher's
existing durable lane/root record (extend it if necessary), and rehydrate them
without re-prompting/restarting the agent. A client reconnect alone cannot
repair server-side tokens. Until reconciliation completes, show an ungrouped
row, never stale grouping from a previous boot. Verify actual token values in
`workspace list`/snapshot before reporting the launch metadata complete.

## 3. Options and trade-offs

Effort below is a **provisional active-agent engineering range**, not a delivery
ETA or measured cycle time. CI/review, m5 availability, and approved installation
are separate gates; use measured cycle time when admitting the build round.

| Option | Effort / protocol impact | Mergeability and risk |
| --- | --- | --- |
| **A. Client-side federated tree in our fork** | First project slice roughly 4–8 agent-hours; complete rollout and edge coverage roughly 8–16 total, with launcher work parallel. No transport change when using existing tokens. | Smallest change; reuses connections, cached snapshots, activation and failure isolation. A generic metadata-driven, opt-in view can be separated from stack policy for a possible upstream contribution, but upstream acceptance is unknown. Main risks: identity collisions, inconsistent navigation order, collapse state, stale rows, and losing connection diagnostics. |
| **B. Server-side aggregation / proxy** | Roughly 24–48 agent-hours before fleet proof, plus any access/security gates. Needs a gateway to combine resources and proxy focus, screens, input, API calls, graphics, capabilities and lifecycle. | Adds a single failure point and a new routing namespace. A proxy could expose generation 1 without changing codecs, but it still must remap IDs and preserve boot/revision/input fences correctly. Larger upstream surface and operational cost. Moving SSH credentials/connections to a gateway changes the trust boundary and needs Paul's access approval and a named security pass. Not justified for a sidebar layout. |
| **C. Use / wait for upstream** | Current multi-machine foundation is already usable; renaming profiles is cheap but does not meet acceptance. Cost or date for a future upstream grouped tree is unknown. | Checkout docs explicitly describe machine > workspace and a shared agent list, not org > project > lane. Reuse this foundation; do not invent an upstream roadmap or wait for an unverified promise. No upstream contact in this round. |

### Protocol 22 is not endpoint generation 1

[wire.rs](../../src/protocol/wire.rs) has `PROTOCOL_VERSION = 22` for private
same-install/direct-terminal/internal paths.
[handshake.rs](../../src/client/handshake.rs) instead sends
`endpoint.hello.v1` for the client-owned shell, negotiating generation 1 and
`shell.snapshot.v1`, `shell.surface.v1`, `shell.input.semantic.v1`, and
`shell.blob.v1`. [endpoint.rs](../../src/protocol/endpoint.rs) carries methods
and capabilities in `endpoint.welcome.v1`. Saved SSH connections need the
surface-interest/presentation fencing and health support already checked by
the registry; other missing methods disable only the feature.

Do **not** add org/project fields to frozen binary-reachable structs or append
variants to a frozen enum and call it compatible. A protocol-22 bump is not a
way around generation-1 compatibility. Keep codec/tag fixtures, digests, and
`tests/fixtures/endpoint-method-shapes-v1.json` unchanged.

The recommended slice adds values to the existing `tokens` collection in
`ClientShellWorkspace`; it adds no field, method, variant, or codec. If we later
need durable typed workspace identity, design a neutral optional JSON API or
negotiated companion control, keep old-server fallbacks, and account separately
for any private binary representation. Do not make that migration a prerequisite
for Paul's first visible result.

## 4. Recommended view and safeguards

Illustrative arrangement, not a claim about today's inventory:

```text
Smarty Pants
  Org agents
    org (Paul)                       [Ryzen 1]
    org (Kate)                       [Ryzen 1]
  Herdr
    herdr-lead                       [Ryzen 1]
    Unified fleet sidebar design #4283 [Ryzen 4]
    <another real lane>               [Ryzen 3]
  Smarty Dev
    dev-lead                         [Ryzen 1]
    <real lane>                      [Ryzen 2]
Playful
  <real project>
    <lead and lanes>                 [host tags]
Ungrouped
  <unmapped workspace>               [host tag]
```

- Merge **group headings only** by `(org_id, project_id)`. Do not concatenate
  snapshots into one fake server, deduplicate real workspaces by label, or
  overwrite local Git membership. Two roots from different endpoints remain
  separate selectable leaves under one project; a synthetic project heading
  only expands/collapses. Lead first, then lanes in a stable order with purpose
  and issue. Every mapped workspace appears exactly once; unknown ones stay
  available under Ungrouped (including ordinary Local work).
- Resolve grouping from explicit workspace tokens first; otherwise inherit a
  stamped root's org/project only for unambiguous **same-endpoint** worktree
  membership. Validate complete, bounded ID pairs; partial or conflicting
  metadata stays visible/unmapped. Map generic org/project display roles to
  the contract's token keys in the opt-in client configuration; keep Smarty
  naming and org-file interpretation out of Herdr's runtime. Never use a remote
  path or readable profile label as a global key. Use stable org/project keys
  for collapse state, separately from existing endpoint/worktree collapse state.
- Share one ordered row projection between rendering, hit-testing,
  `prefix+w`, previous/next-workspace navigation, scrolling and reveal. Keep
  compact/mobile navigation usable too; do not make the mouse the only safe
  path. Group headings are not focus targets. Disable drag/reorder across
  synthetic groups in the first slice rather than accidentally invoke a
  server-local workspace move on another endpoint.
- Keep leaf `(endpoint_id, workspace_id)` and agent `(endpoint_id, pane_id)`
  routing. Reuse `focus_or_activate`, surface activation and input fencing
  unchanged. Do not select a transport by group ID, title, issue or lead name.
- Derive the actual host tag from the saved connection target through the
  stack's display mapping; both Ryzen 1 org sessions get `[Ryzen 1]`. Compare
  `smarty_host` as a hint, not authority; a mismatch must not retarget input.
  Unknown targets use their own target label rather than guess a fleet host.
- Preserve per-endpoint reconnect/Attention state in tagged leaves and an
  accessible connection-status/menu entry. An endpoint with no snapshot must
  still have a visible connection problem. Cached leaves are dim and
  non-actionable. Reconnect cannot steal focus, and an old boot's rows cannot
  route to recycled IDs. One host failing must not hold up other hosts.
- Make grouped view opt-in and keep today's Machines view as the default and
  immediate rollback. Existing saved profiles, custom row layouts and CLI
  routing stay intact. Advertised-method checks allow old compatible servers
  without metadata reporting to connect and remain Ungrouped. Cache the group
  projection on snapshot/config changes; do not scan remote files or org JSON
  in the render loop. Profile the pane-scaled paths under Herdr's existing
  multiplicative-performance guardrail if the implementation widens them.

## 5. Smallest build slice and m5 proof

**Before implementation:** inventory Paul's actual saved profiles and visible
workspace tree from his normal m5 start. Confirm both Ryzen 1 sessions and the
Ryzen 2/3/4 lane sessions independently connect. Treat any outstanding #3832
access/setup work as a prerequisite, not permission to add keys or replace a
server. Read-only snapshots do not prove UI focus or input.

Split the next round into bounded, exclusive scopes: (a) client row projection,
render/navigation integration; (b) l4282n's launcher contract and re-stamping;
(c) independent final audit after code/artifact freeze. Herdr lead integrates.

**First visible slice:** opt-in grouped view on the real m5 client, using existing
server tokens. Stamp the Herdr root and one actual Ryzen 3 lane under the same
Smarty Pants/Herdr project, plus one actual Playful project. Retain every other
workspace under its correct group or Ungrouped; do not hide it to make the
example look complete. Ship the same client projection for Ryzen 2/4, then
reconcile metadata for **all** fleet workspaces. No aggregator or new access is
needed if the saved connections from #3832 work.

### Video acceptance walk (build round)

Record a short continuous video on **Paul's m5**, using real UI input, starting
from his ordinary way of opening Herdr (#4280-style proof). Record the client
artifact/commit and server versions alongside it; use the reviewed installation
and tested rollback path. Do not substitute screenshots of a fixture or Linux
client for this video.

1. Open Herdr normally, enable the grouped view, and expand Smarty Pants and
   Playful. Show org > project > lane, both Ryzen 1 sessions, and real lane tags
   for Ryzen 2/3/4. Compare the leaf inventory with the actual server snapshots;
   no workspace may disappear or belong to a guessed project.
2. Open the actual Ryzen 3 lane from the unified project; read its existing
   screen, submit a harmless unique prompt agreed for that lane, and show the
   response there. Use `herdr --machine <actual-profile-id> agent read <pane>`
   as corroboration. Repeat focus/read/prompt on Ryzen 1/2/4, not just Ryzen 3.
3. Switch back to the Ryzen 1 lead, then Playful; demonstrate keyboard
   workspace navigation in the displayed order and collapse/reveal behavior.
4. Using a safe test connection interruption (not terminating fleet agents),
   show stale rows disabled, other hosts usable, and recovery without focus
   theft. Test same-looking IDs/names on two hosts: each routes only to its
   originating endpoint.
5. In an approved isolated restore probe, restore a server's real-shaped
   workspace inventory, reconcile stamps, and show the same grouping with
   fresh boot/current IDs. Do not restart a live fleet server just for the video.
6. Switch to Machines view or the prior client artifact for rollback and show
   existing connection/CLI behavior still works.

Targeted implementation checks must also cover missing/oversized/conflicting
metadata; a lane with no local root; multiple roots and duplicate labels; two
org sessions on one host; an unsupported metadata method; disabled/removed
profiles; stale generation/boot; and typing during a pending endpoint switch.
Keep existing endpoint codec/activation tests. A new grouping unit test is
useful only with these real routing/restore paths covered. Full delivery needs
all fleet metadata mapped, m5 video evidence, required review/CI/install gates,
and an independent **`ACCEPTANCE_AUDIT: PASS`**. This design note is not that PASS.

### This round's acceptance ledger

| Requested design check | Evidence in this note |
| --- | --- |
| Explain current sidebar, endpoints and names | Section 1 traces catalog → registry → per-endpoint snapshot → sidebar and activation. |
| Evaluate grouping sources and remote lead inheritance | Section 2 separates portable assignment identity from host-local Git keys. |
| Give launchers an exact metadata/restore contract | Section 2 specifies token keys, limits, stamping and reconciliation triggers. |
| Compare federation, aggregation and local upstream plans | Section 3 includes effort, compatibility, mergeability, risk, and checkout-only limits. |
| Recommend the smallest slice and real m5 proof | Sections 4–5 preserve routing and define video steps without claiming implementation. |
| Stay design-only | Only this Markdown note is changed and locally committed; no push or GitHub activity. |
