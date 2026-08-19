---
name: herdr
description: "Control Herdr, a terminal multiplexer for coding agents. Use when the user mentions Herdr, or when `HERDR_ENV=1` and the task requires inspecting, contacting, prompting, waiting for, starting, or controlling another top-level agent, session, pane, tab, or workspace. Do not use for ordinary same-session subagent delegation or parallelism. Requires HERDR_ENV=1."
---

# Herdr

Herdr organizes terminals into workspaces, tabs, and panes, recognizes coding agents running inside panes, and exposes the current session through the `herdr` CLI.

Before issuing any control command, verify that this agent is running inside a Herdr-managed pane:

```bash
test "${HERDR_ENV:-}" = 1
```

If the check fails, say that you are not running inside Herdr and stop. Do not inspect or control the focused Herdr session from outside Herdr.

When the check passes, the `herdr` binary in `PATH` talks to the current session. Use it to inspect neighboring work, read output, and wait for state changes. Create layout or issue terminal control only when the user or operator explicitly authorized that action.

## Learn the current CLI

The installed binary is the authority for command syntax. Start with:

```bash
herdr --help
```

Then inspect the relevant command group with `--help`:

```bash
herdr agent --help
herdr pane --help
herdr workspace --help
herdr tab --help
herdr worktree --help
herdr terminal --help
herdr notification --help
herdr integration --help
herdr plugin --help
herdr session --help
```

Do not run bare `herdr` for discovery; it launches or attaches the TUI. Do not run a command group without `--help`: current Herdr prints usage but exits with status 2, which automation surfaces as an error. Do not probe a mutating nested command by omitting arguments. Commands such as `herdr workspace create` are valid with defaults and will execute.

Most control commands return JSON. Read identifiers and state from those responses instead of predicting them.

Before starting or resuming a supported agent, run `herdr integration status`. If the target integration is missing or outdated, report that state and update it with `herdr integration install <kind>` only when authorized; then recheck status before relying on lifecycle or session metadata.

## Use installed plugin surfaces

HerdR plugins are the shared extension surface for executable workflow tools. Discover installed capabilities before calling a product binary directly or inventing an agent-specific wrapper:

```bash
herdr plugin list --json
herdr plugin list --plugin <plugin-id> --json
herdr plugin action list --plugin <plugin-id>
herdr plugin pane --help
```

The filtered plugin JSON is the manifest-backed source for action IDs and pane entrypoint IDs. Invoke the declared surface with:

```bash
herdr plugin action invoke <action-id> --plugin <plugin-id>
herdr plugin pane open --plugin <plugin-id> --entrypoint <entrypoint-id> \
  --workspace "$HERDR_WORKSPACE_ID" --cwd "$PWD" --no-focus
```

Use a qualified action ID when more than one plugin declares the same local ID. Keep background panes unfocused unless the user asks to switch context. A successful invoke or open proves launch only; inspect the returned log or pane and verify the tool's result. Do not install, uninstall, enable, disable, or relink a plugin unless the user authorized that host mutation.

## Understand layout, panes, and agents

Choose the primitive that matches the job:

- Workspace, tab, and pane topology organize terminal locations.
- Pane commands control raw terminals, shells, tests, servers, input, and output.
- Agent commands control the recognized coding agent currently occupying a pane.

A pane exists whether or not it contains an agent. `agent start` requires an existing available shell pane and never creates, splits, or moves layout. Use pane commands for ordinary processes. Use agent commands when Herdr must validate agent identity or interpret `idle`, `working`, `blocked`, `done`, and `unknown` lifecycle states.

Agent commands accept either a unique live agent name or the pane ID currently hosting that agent. They do not accept terminal IDs or bare agent-kind labels. Names must match `[a-z][a-z0-9_-]{0,31}` and be unique among live agents. A name follows the current pane occupant and is cleared when that agent exits, is released, or is replaced.

`idle` means the agent is ready for input and its tab has been seen in the focused Herdr UI. `done` is the same underlying idle state after unseen background work finishes. Focusing the tab or targeting the pane or agent with a focus command marks it seen. CLI reads do not mark it seen. `blocked` means Herdr recognized an approval or question UI. `unknown` means an agent is present but Herdr cannot classify it confidently; it does not prove completion.

## Use IDs and caller context

Public IDs are opaque stable handles:

- workspace: `w1`
- tab: `w1:t1`
- pane: `w1:p1`

Closed tab and pane IDs are not reused. A pane moved into another workspace receives a new workspace-qualified pane ID. After `pane move`, continue with `.result.move_result.pane.pane_id` or the live agent name. The old value is reported as `.result.move_result.previous_pane_id`; only the moved process's inherited caller context keeps resolving that old ID, so do not use it as a general agent target.

Herdr injects the caller's context into each managed pane:

```bash
printf '%s\n' "$HERDR_WORKSPACE_ID" "$HERDR_TAB_ID" "$HERDR_PANE_ID"
```

Prefer `--current` when a pane command should target the calling pane. Omitting a target may use the UI-focused pane, which can belong to the user or another client.

Discover live state with:

```bash
herdr workspace list
herdr tab list --workspace "$HERDR_WORKSPACE_ID"
herdr pane current --current
herdr pane list --workspace "$HERDR_WORKSPACE_ID"
herdr agent list
```

Creation responses expose the IDs to use next. `workspace create` returns `.result.workspace`, `.result.tab`, and `.result.root_pane`. `tab create` returns `.result.tab` and `.result.root_pane`. `pane split` returns the new pane as `.result.pane`.

`worktree open` resolves the owning repository from caller context. When the caller is itself in a linked worktree, pass the main checkout explicitly with `--cwd /absolute/main-checkout` or an already-open parent workspace with `--workspace <id>`; changing the shell's directory or relaunching Herdr is not required:

```bash
herdr worktree open --cwd /absolute/main-checkout --path /absolute/existing-worktree --label <label> --no-focus
```

## Coordinate existing top-level agents

Use Herdr to discover and observe top-level agents, but use the coordination transport provided by the current agent runtime or session for normal peer requests, replies, handoffs, and steering. Herdr does not define a peer-message transport.

1. Run `herdr agent list`; select candidates by verified cwd, workspace, and terminal title.
2. Run `herdr agent get <target>` and `herdr agent read <target> --source recent-unwrapped --lines 120` to verify the exact repository, worktree, process, and task.
3. Address the verified recipient through the available coordination transport and follow that transport's delivery and reply semantics.
4. Use fresh `agent get`, `agent read`, or `agent wait` state when terminal evidence is needed. A successful transport send proves delivery only according to that transport's contract.

If no suitable coordination transport is available, report that limitation. Do not fall back to `agent prompt`, `agent send-keys`, `agent start`, `pane send-text`, `pane send-keys`, or `pane run` for routine peer messaging. Those commands write content into a terminal and are reserved for explicitly authorized recovery or control below.

If no matching target exists, start one through Herdr only when the user explicitly asked to spawn it or authorized that exact control action. Do not substitute another terminal manager or raw terminal injection.

## Explicitly authorized start, recovery, and control

The commands in this section write content into another pane. Use them only when the direct user or operator instruction authorizes the exact target and action. For an attributed agent caller, pass `--allow-cross-pane` on that request; the flag records the deliberate override but does not create authorization by itself.

Default to a sibling pane in the current tab and the current working directory. Do not create a workspace, tab, worktree, or different cwd unless the user explicitly requests that topology or location.

Honor a direction requested by the user. Otherwise inspect the caller pane:

```bash
herdr pane layout --pane "$HERDR_PANE_ID"
```

Split a wide pane to the right and a narrow or tall pane down. Avoid repeated same-direction splits that create unusably narrow columns or short rows. Keep the user's focus in the calling pane and explicitly preserve the caller's working directory:

```bash
herdr pane split --current --direction right --cwd "$PWD" --no-focus
```

Replace `right` with `down` when appropriate. Read the new pane ID from `.result.pane.pane_id`.

An available shell pane must be at its interactive prompt, with the shell itself in the foreground and no foreground command, editor, or agent running. Start a supported agent in that pane with a useful unique name:

```bash
herdr agent start reviewer --kind codex --pane <returned-pane-id> --allow-cross-pane
```

Use the kind requested by the user. Run `herdr agent --help` to inspect the installed kind list and options. Pass native agent arguments only after `--`:

```bash
herdr agent start reviewer --kind codex --pane <returned-pane-id> --allow-cross-pane -- <agent-args...>
```

`agent start` returns only after Herdr detects the expected agent in the same pane and considers it ready for interactive input. It defaults to a 30-second startup timeout.

### Resume an existing OMP session

For a terminal-manager migration, use the exact absolute OMP session JSONL path instead of `--continue`, an ID prefix, or the interactive picker. CWD aliases and historical sessions can otherwise select the wrong conversation.

1. Verify the session header's `cwd` and a recent transcript marker against the source terminal. If the checkout moved or was renamed, verify that equivalence explicitly and pass the destination with `--cwd`.
2. Gracefully exit the source OMP process and confirm it stopped. Never run two OMP processes against the same session file.
3. Start the resumed session in the destination pane:

```bash
herdr agent start <name> --kind omp --pane <pane-id> --allow-cross-pane -- --resume /absolute/path/to/session.jsonl --cwd /absolute/destination/worktree
```

If multiple source processes share one JSONL but hold distinct in-memory branches, do not resume the shared path twice. Quiesce them one at a time, append a unique handoff marker, then run `/fork` inside that source OMP process before `/exit`. Source-side `/fork` atomically writes the exact in-memory branch to a new session file, even if a sibling process replaced the original path and left this process writing an unlinked inode. Record the `Session forked to ...` path and resume that new file in Herdr with `--resume`.

If a source process is already gone and the remaining on-disk shared file is verified to represent that branch, ensure no process has it open, then use Herdr `--fork` once for that branch:

```bash
herdr agent start <name> --kind omp --pane <pane-id> --allow-cross-pane -- --fork /absolute/path/to/shared-session.jsonl --cwd /absolute/destination/worktree
```

Verify every destination session path differs from the shared source and from the other branches before migrating the next one.

4. Verify `herdr agent get <name>` reports the expected session path, then read the agent and confirm its transcript marker before treating the migration as complete.

For an explicitly authorized recovery or control step, submit terminal input through the agent surface:

```bash
herdr agent prompt reviewer "Review the current diff and report only actionable findings." --wait --timeout 120000 --allow-cross-pane
```

`agent prompt` sends text, then encoded Enter after a short delay, while honoring the pane's live bracketed-paste mode. If the agent is already `blocked`, it returns `agent_blocked` without sending input; inspect the dialog and use `agent send-keys` only for a deliberate authorized response. For this control path, `--wait` waits for the first settled `idle`, `done`, or `blocked` state reached after an accepted submission. Do not repeat those defaults with `--until`.

An accepted prompt sent from another non-working state must produce an observed lifecycle change within five seconds. Otherwise Herdr returns `agent_prompt_stalled` instead of waiting indefinitely; if the caller sets `--timeout` to five seconds or less, Herdr returns the normal `timeout` error instead. This wait tracks lifecycle state, not an individual turn; if the agent is already working, completion of the active turn may satisfy it.

Use `--until` only for a state-specific workflow, such as waiting for an already-running agent to request input:

```bash
herdr agent wait reviewer --until blocked --timeout 120000
```

Without `--until`, standalone `agent wait` uses the same settled-state defaults as `agent prompt --wait`.

For an explicitly authorized interactive recovery, use logical keys:

```bash
herdr agent send-keys reviewer esc --allow-cross-pane
herdr agent send-keys reviewer ctrl+c --allow-cross-pane
```

Herdr validates all keys before writing any bytes. Read the result through the resolved agent:

```bash
herdr agent get reviewer
herdr agent read reviewer --source recent-unwrapped --lines 120
```

If a wait fails or returns `blocked`, inspect `agent get` and `agent read` before deciding whether the authorized recovery requires more input. Use the pane surface only when raw terminal control was explicitly authorized.

## Explicitly authorized command execution in another pane

This is a control path, not a peer-coordination transport. Confirm the exact target pane and command before sending input.

Create a sibling pane with the same geometry rule, preserve the caller's working directory, and keep user focus unchanged:

```bash
herdr pane split --current --direction right --cwd "$PWD" --no-focus
```

Read the new pane ID from `.result.pane.pane_id`, then run and inspect the command:

```bash
herdr pane run <returned-pane-id> "just test" --allow-cross-pane
herdr pane wait-output <returned-pane-id> --match "test result" --timeout 120000
herdr pane read <returned-pane-id> --source recent-unwrapped --lines 120
```

`pane run` atomically sends command text and Enter. `pane wait-output` searches the selected snapshot immediately, so output that already exists can match. Use `--match <text>` for a literal substring or `--regex <pattern>` for a Rust regular expression. Omitting `--timeout` allows an indefinite wait.

Use the read source that matches the task:

- `visible`: the currently rendered viewport.
- `recent`: recent rendered output, including soft wraps.
- `recent-unwrapped`: recent output with soft wraps joined; prefer it for logs and transcripts.
- `detection`: the plain-text bottom-buffer snapshot used for agent detection.

Use `--format ansi` when colors and terminal styling are evidence. Otherwise use text.

`--lines` asks Herdr for more rows from the pane's available screen and host scrollback. If increasing it does not reveal more of a completed response, the pane is probably running the agent on the terminal's alternate screen. Rows that leave the alternate screen do not enter Herdr's host scrollback, so a larger line count cannot recover them.

After that failed read, use the normal coordination transport to ask the agent to write its complete response as Markdown in a temporary directory and reply only with the file path, then read the file directly. Do not fall back to terminal injection solely to recover output.

## Safety and coordination rules

- Cross-pane `agent start`, `agent prompt`, `agent send-keys`, `pane send-text`, `pane send-keys`, and `pane run` are denied for attributed agent callers by default. Use `--allow-cross-pane` only for the exact explicitly authorized recovery or control action; the flag is not authorization.
- Use `--no-focus` for background work unless the user asked to switch context.
- Use `--current`, an explicit pane ID, or a unique agent name. Do not rely on another client's focused pane.
- Parse IDs from JSON responses. Do not derive them from sidebar order or examples.
- Do not close workspaces, tabs, panes, or sessions you did not create unless the user explicitly asked.
- Never run `herdr server stop` from an active session unless the user explicitly intends to stop the server and its pane processes.
- Never kill the main Herdr process. Use named test sessions for experiments that need an isolated server.
- CLI server errors are JSON on stderr with exit status 1. CLI syntax errors exit with status 2.
