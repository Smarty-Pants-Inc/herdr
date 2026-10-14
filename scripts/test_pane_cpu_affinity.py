#!/usr/bin/env python3
"""Real Linux pane-affinity E2E; never connects to the caller's Herdr session.

Run each build separately:
  scripts/test_pane_cpu_affinity.py --binary /absolute/base/herdr --expect inherited
  scripts/test_pane_cpu_affinity.py --binary /absolute/candidate/herdr --expect online

Requires Linux, util-linux taskset, CPU 0, and permission to use every online CPU.
A restricted cpuset is an unsuitable host for this full-online regression check.
Exit 0 means the requested expectation was observed; baseline prints RED even
when --expect inherited succeeds. Artifacts are retained under mktemp's TMPDIR.
No cargo, TUI, daemon launcher, global configuration, or installed server is used.
"""

import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import threading


ONLINE_PATH = Path("/sys/devices/system/cpu/online")
READY_MESSAGE = b"herdr server running;"
DONE_PATTERN = r"(?m)^AFFINITY_PROBE_DONE\r?$"


def cpu_set(value):
    """Normalize the kernel's comma-separated inclusive CPU ranges."""
    cpus = set()
    for part in value.strip().split(","):
        if not re.fullmatch(r"\d+(?:-\d+)?", part):
            raise ValueError(f"invalid CPU list: {value!r}")
        ends = part.split("-")
        first, last = int(ends[0]), int(ends[-1])
        if last < first:
            raise ValueError(f"reversed CPU range: {part}")
        cpus.update(range(first, last + 1))
    return cpus


def allowed_list(status):
    match = re.search(r"(?m)^Cpus_allowed_list:\s*([0-9,-]+)\s*$", status)
    if not match:
        raise ValueError("Cpus_allowed_list missing from process status")
    return match.group(1)


def isolated_environment(root):
    # Strip unknown/future Herdr knobs too, not just the current socket selectors.
    env = {
        key: value for key, value in os.environ.items()
        if not key.startswith("HERDR_")
        and key not in ("PI_CODING_AGENT_DIR", "PI_CONFIG_DIR", "ENV", "BASH_ENV")
    }
    for key, directory in (
        ("HOME", "home"), ("XDG_CONFIG_HOME", "config"),
        ("XDG_RUNTIME_DIR", "runtime"), ("XDG_STATE_HOME", "state"),
        ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "tmp"),
    ):
        path = root / directory
        path.mkdir(mode=0o700)
        env[key] = str(path)
    env.update({
        "HERDR_SOCKET_PATH": str(root / "runtime" / "api.sock"),
        "HERDR_CONFIG_PATH": str(root / "config" / "herdr" / "config.toml"),
        "SHELL": "/bin/sh", "TERM": "xterm-256color",
    })
    config = Path(env["HERDR_CONFIG_PATH"])
    config.parent.mkdir(mode=0o700)
    config.write_text(
        'onboarding = false\n[terminal]\ndefault_shell = "/bin/sh"\n'
        'shell_mode = "non_login"\n'
    )
    return env


class Cli:
    def __init__(self, binary, env, root):
        self.binary, self.env, self.root = binary, env, root
        self.sequence = 0

    def run(self, *args, timeout=10, as_json=True):
        self.sequence += 1
        stem = self.root / f"cli-{self.sequence:02d}-{args[0]}-{args[1]}"
        command = [str(self.binary), *args]
        stem.with_suffix(".command.json").write_text(json.dumps(command) + "\n")
        try:
            result = subprocess.run(
                command, env=self.env, cwd=self.root, stdin=subprocess.DEVNULL,
                capture_output=True, text=True, timeout=timeout,
            )
        except subprocess.TimeoutExpired as error:
            stem.with_suffix(".stdout").write_bytes(error.stdout or b"")
            stem.with_suffix(".stderr").write_bytes(error.stderr or b"")
            raise RuntimeError(f"CLI timed out after {timeout}s: {command}") from error
        stem.with_suffix(".stdout").write_text(result.stdout)
        stem.with_suffix(".stderr").write_text(result.stderr)
        if result.returncode:
            raise RuntimeError(f"CLI exit {result.returncode}: {command}\n{result.stderr}")
        if not as_json:
            return result.stdout
        response = json.loads(result.stdout)
        if "error" in response:
            raise RuntimeError(f"CLI error: {response['error']}")
        return response["result"]


def capture_server(pipe, log_path, ready, state):
    # The real foreground server writes this readiness notification only after
    # both sockets and HeadlessServer exist. Wait on that event, not socket polls.
    try:
        tail = b""
        with log_path.open("wb") as log:
            while True:
                chunk = os.read(pipe.fileno(), 4096)
                if not chunk:
                    break
                log.write(chunk)
                log.flush()
                tail = (tail + chunk)[-4096:]
                if READY_MESSAGE in tail:
                    state["ready"] = True
                    ready.set()
    finally:
        pipe.close()
        ready.set()  # EOF also wakes the caller so early startup failure is finite.


def run(args):
    binary = Path(args.binary)
    if not binary.is_absolute() or not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("--binary must name an absolute executable Herdr binary")
    if sys.platform != "linux":
        raise RuntimeError("this E2E requires Linux /proc and /sys")
    taskset = shutil.which("taskset")
    mktemp = shutil.which("mktemp")
    if not taskset or not mktemp:
        raise RuntimeError("taskset and mktemp must already be installed")
    # Let mktemp inherit the parent's TMPDIR without a path override. Retain
    # the directory; TemporaryDirectory would discard the evidence after the run.
    root = Path(subprocess.check_output(
        [mktemp, "-d"],
        text=True, timeout=5,
    ).strip()).resolve()
    print(f"Artifacts: {root}", flush=True)
    env = isolated_environment(root)
    cli = Cli(binary, env, root)
    online_text = ONLINE_PATH.read_text().strip()
    online = cpu_set(online_text)
    print(f"Online: {online_text}", flush=True)
    if 0 not in online or len(online) < 2:
        raise RuntimeError("need CPU 0 and at least two online CPUs to distinguish RED/GREEN")
    # Probe availability in a disposable child, never widen this runner's mask.
    preflight = subprocess.run(
        [taskset, "-c", online_text, "/bin/cat", "/proc/self/status"],
        env=env, cwd=root, capture_output=True, text=True, timeout=5,
    )
    (root / "preflight.stdout").write_text(preflight.stdout)
    (root / "preflight.stderr").write_text(preflight.stderr)
    if preflight.returncode or cpu_set(allowed_list(preflight.stdout)) != online:
        raise RuntimeError("kernel cpuset restrictions prevent full-online proof on this host")

    server = None
    reader = None
    pane_id = None
    outcome = {"expect": args.expect, "binary": str(binary), "online": online_text}
    try:
        command = [taskset, "-c", "0", str(binary), "server"]
        (root / "server.command.json").write_text(json.dumps(command) + "\n")
        ready, state = threading.Event(), {"ready": False}
        server = subprocess.Popen(
            command, env=env, cwd=root, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        # Retain exact ownership evidence. Cleanup signals only this unreaped
        # direct child via Popen, never a name, guessed PID, or shared group.
        (root / "server.owner.json").write_text(json.dumps({
            "pid": server.pid, "parent_pid": os.getpid(), "command": command,
            "proc_stat": Path(f"/proc/{server.pid}/stat").read_text(),
        }, indent=2) + "\n")
        outcome["server_pid"] = server.pid
        reader = threading.Thread(
            target=capture_server,
            args=(server.stderr, root / "server.stderr", ready, state), daemon=True,
        )
        reader.start()
        if not ready.wait(15) or not state["ready"]:
            raise RuntimeError("server did not announce readiness within 15s; see server.stderr")
        before = Path(f"/proc/{server.pid}/status").read_text()
        (root / "server-before.status").write_text(before)
        server_before = allowed_list(before)
        print(f"Server: pid={server.pid} CPUs={server_before}", flush=True)
        if cpu_set(server_before) != {0}:
            raise RuntimeError("counterexample invalid: server is not pinned to CPU 0")

        created = cli.run("workspace", "create", "--cwd", str(root), "--label", "affinity-e2e")
        workspace_id = created["workspace"]["workspace_id"]
        tab_id = created["tab"]["tab_id"]
        pane_id = created["root_pane"]["pane_id"]
        terminal_id = created["root_pane"]["terminal_id"]
        outcome.update({"workspace_id": workspace_id, "tab_id": tab_id,
                        "pane_id": pane_id, "terminal_id": terminal_id})
        print(f"Spawned: workspace={workspace_id} tab={tab_id} pane={pane_id} "
              f"terminal={terminal_id}", flush=True)
        # This shell command stays alive while output is read. The anchored
        # sentinel cannot match the terminal's echo of this input command.
        command = (
            "stty -echo; printf 'PANE_SHELL_PID=%s\\n' \"$$\"; "
            "cat /proc/self/status; printf '\\nAFFINITY_PROBE_DONE\\n'; exec sleep 60"
        )
        cli.run("pane", "run", pane_id, "--", command, as_json=False)
        matched = cli.run(
            "pane", "wait-output", pane_id, "--regex", DONE_PATTERN,
            "--source", "recent-unwrapped", "--lines", "200", "--timeout", "10000",
            timeout=15,
        )
        if matched["type"] != "output_matched":
            raise RuntimeError(f"unexpected output wait result: {matched}")
        text = cli.run(
            "pane", "read", pane_id, "--source", "recent-unwrapped",
            "--lines", "200", "--format", "text", as_json=False,
        )
        pane_text = allowed_list(text)
        shell_pid = re.search(r"(?m)^PANE_SHELL_PID=(\d+)\s*$", text)
        cat_pid = re.search(r"(?m)^Pid:\s*(\d+)\s*$", text)
        if not shell_pid or not cat_pid:
            raise RuntimeError("spawned shell/cat PID missing from actual pane output")
        outcome.update({"pane_cpus": pane_text, "shell_pid": int(shell_pid[1]),
                        "cat_pid": int(cat_pid[1])})
        print(f"Pane: id={pane_id} shell_pid={shell_pid[1]} cat_pid={cat_pid[1]} "
              f"CPUs={pane_text}", flush=True)
        after = Path(f"/proc/{server.pid}/status").read_text()
        (root / "server-after.status").write_text(after)
        server_after = allowed_list(after)
        outcome["server_cpus"] = server_after
        print(f"Server after spawn: pid={server.pid} CPUs={server_after}", flush=True)
        if cpu_set(server_after) != {0}:
            raise RuntimeError("server affinity changed; reset must be child-only")
        if cpu_set(ONLINE_PATH.read_text()) != online:
            raise RuntimeError("online CPUs changed during the run; repeat with stable topology")
        pane = cpu_set(pane_text)
        color = "GREEN" if pane == online else "RED" if pane == {0} else "OTHER"
        expected = online if args.expect == "online" else {0}
        outcome.update({"outcome": color, "expectation_met": pane == expected})
        print(f"Outcome: {color}; expect={args.expect}; "
              f"{'PASS' if pane == expected else 'FAIL'}; server_pid={server.pid} "
              f"pane_id={pane_id}", flush=True)
        if pane != expected:
            raise RuntimeError(f"pane CPU set {pane_text} did not meet --expect {args.expect}")
    finally:
        # Closing the owned pane through its isolated socket lets Herdr reap the
        # PTY child (which has its own process group). Even failed probes get here.
        if pane_id is not None and server is not None and server.poll() is None:
            try:
                cli.run("pane", "close", pane_id)
                outcome["pane_closed"] = True
            except Exception as error:
                outcome["pane_close_error"] = str(error)
                print(f"Cleanup pane close: {error}", file=sys.stderr)
        if server is not None:
            if server.poll() is None:
                server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()  # Exact unreaped direct child, not a process group.
                server.wait(timeout=5)
            outcome["server_exit"] = server.returncode
            if reader is not None:
                reader.join(timeout=2)
            print(f"Cleanup: owned server pid={server.pid} exited={server.returncode}", flush=True)
        (root / "result.json").write_text(json.dumps(outcome, indent=2) + "\n")
    if "pane_close_error" in outcome:
        raise RuntimeError("pane cleanup failed; inspect result.json and server.stderr")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", required=True, help="absolute path to the actual Herdr build")
    parser.add_argument("--expect", required=True, choices=("inherited", "online"))
    args = parser.parse_args()
    # SIGTERM must take the same finally cleanup path as Ctrl-C.
    signal.signal(signal.SIGTERM, lambda _signum, _frame: sys.exit(143))
    try:
        return run(args)
    except (OSError, ValueError, RuntimeError, KeyError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
