# herdr-sshd-lookup

`herdr-sshd-lookup` tells the Herdr server which SSH key authenticated one sshd
login. The server uses the answer to map a remote client to a principal in
`/etc/herdr/principals.json` (smarty-dev#2636). The server itself is not in the
`systemd-journal` group; only this small helper reads the journal.

Linux with systemd-journald and OpenSSH only. Without the helper, remote
connections stay unmapped.

## Install

Build it from the Herdr checkout and install it setgid `systemd-journal`:

```bash
cargo build --release --locked -p herdr-sshd-lookup
sudo install -o root -g systemd-journal -m 2755 \
  target/release/herdr-sshd-lookup /usr/local/libexec/herdr-sshd-lookup
```

The server runs only `/usr/local/libexec/herdr-sshd-lookup`. Every directory on
that path must be root-owned and not group or world writable, and the file must
be root-owned and not group or world writable. Otherwise the server does not run
it. Check the result:

```bash
ls -l /usr/local/libexec/herdr-sshd-lookup
# -rwxr-sr-x 1 root systemd-journal ... /usr/local/libexec/herdr-sshd-lookup
```

The server must run without `NoNewPrivileges` (or another no-new-privileges
sandbox). Otherwise the kernel ignores the setgid bit, the helper cannot read
the system journal, and connections stay unmapped.

The host must run Yama with `kernel.yama.ptrace_scope` 1 or higher (the
Ubuntu default). With scope 0, or without Yama, any process of a user could
ptrace that user's bridge from another session, so the helper refuses every
lookup. Check it:

```bash
cat /proc/sys/kernel/yama/ptrace_scope   # 1, 2 or 3
```

To remove it: `sudo rm /usr/local/libexec/herdr-sshd-lookup`.

## Contract

```text
herdr-sshd-lookup 3<&CONNECTION 4<&EXECUTABLE
```

The helper takes no arguments. Its only inputs are two descriptors. Fd 3 is
the caller's accepted connection from `herdr remote-client-bridge`. Fd 4 is the
caller's own executable: the server opens `/proc/self/exe` with `O_PATH`.
Any argument, an fd 3 that is not a connected `AF_UNIX` stream socket, or an
fd 4 that is not a regular file, refuses. The helper reads no environment and
no configuration. On success it prints one line and exits 0:

```json
{"pid":1234,"user":"paul","fingerprint":"SHA256:...","source_ip":"100.64.0.7"}
```

`pid` is the sshd priv process the helper found. In every other case it prints
nothing and exits nonzero.

The server passes the client connection as fd 3 and its executable as fd 4
(`dup2`, then every other descriptor above 2 is closed at exec). It keeps its
own walk from the bridge to the priv and uses the answer only when the answer
names the same priv pid and user.

The server passes its executable because the helper cannot read it. A server
that loaded `/etc/herdr/server.key` is non-dumpable, so the kernel refuses
another process of the same user both `/proc/<server>/exe` and
`/proc/<server>/fd` (smarty-dev#13408). Helper and server are one release: a
server that does not pass fd 4 gets no answer.

## Checks

The helper reads the kernel and `/proc` itself and answers only when all of
these are true:

- Fd 3 is a connected `AF_UNIX` stream socket (`SO_DOMAIN`, `SO_TYPE`,
  `getpeername`). `SO_PEERCRED` on it gives the peer pid: the process that
  connected, as the kernel recorded it at `connect`.
- The peer runs the same executable as the caller: the device and inode of
  the peer's `/proc/<pid>/exe` equal those of fd 4, normally the installed
  `herdr` binary that runs both the server and the bridge. An unreadable peer
  executable (another user's process) refuses. The helper reads the peer's
  again at the end; a change refuses.
- The peer is a bridge: its arguments after `argv[0]` are exactly what the
  Herdr launcher writes, `[--session S | --session=S] remote-client-bridge
  [--idle-timeout-v1]`, with one valid session name. Any other subcommand, an
  extra or unknown option, or a repeated `--session` refuses. The server and
  the helper use the same rule (`src/server/bridge_argv.rs`). The helper reads
  them again at the end, and none of the peer's descriptors is a listening
  socket in its `/proc/<pid>/net/unix`. A Herdr server listens; it refuses.
- The caller accepted this connection: fd 3's socket, which the helper
  inherited from the caller, is not listening and carries a bound path in the
  caller's `/proc/<caller>/net/unix` (an accepted socket inherits its
  listener's path; the connecting side is unnamed), and a socket listening on
  that path exists there. The helper does not read `/proc/<caller>/fd`: the
  kernel refuses it for a non-dumpable server.
- `kernel.yama.ptrace_scope` is 1 or higher.
- From the peer, the helper walks at most three parents, like the server, to a
  process whose command line is exactly `sshd: <user> [priv]`, with `comm`
  `sshd` and all uids 0. The walk refuses at pid 1, at the helper's caller, on
  a cycle, and at a parent that is younger than its child. The peer itself is
  never the priv.
- The priv's parent is the system sshd listener: all uids 0, `comm` `sshd`, a
  child of pid 1, in the root-only cgroup `/system.slice/ssh.service` or
  `/system.slice/sshd.service`, and not younger than the login.
- When an executable link is readable, it is the inode of `/usr/sbin/sshd`.
  For root processes that a setgid helper cannot read, journald's trusted
  `_EXE=/usr/sbin/sshd` field on the login's records is the proof.

It then runs `/usr/bin/journalctl` once with a cleared environment and fixed
arguments: the current boot, `_PID=<priv> _COMM=sshd _UID=0`, from the login's
start for 600 seconds, at most 64 records, JSON with only the fields it reads
(`MESSAGE`, `_PID`, `_UID`, `_COMM`, `_EXE`, `_TRANSPORT`, `_BOOT_ID`,
`__REALTIME_TIMESTAMP`, `__MONOTONIC_TIMESTAMP`). The call has a 1.2 second
deadline and a 256 KiB output limit. Every record must carry this login's
trusted `_PID`, `_UID=0`, `_COMM=sshd`, `_EXE`, `_TRANSPORT=syslog`, `_BOOT_ID`
and timestamps after the login started. Exactly one record must start with
`Accepted `, and it must be
`Accepted publickey for <user> from <ip> port <port> ssh2: <type> SHA256:<fp>`.
At last the helper reads the caller's `/proc` state, every process of the walk
and the listener again; a change refuses the answer. A reused peer pid has another start time,
so it refuses too.

The helper keeps the journal group only while it starts `journalctl`, then drops
it for good. It sets `no_new_privs`, closes inherited descriptors above 4 at
start, and closes fds 3 and 4 once it has read the peer and the executable,
before `journalctl` starts.

Logins whose listener restarted (the priv process is now a child of pid 1),
bridges more than three parents below the priv, and OpenSSH 9.8 and later
(`sshd-session`) stay unmapped.

## Threat notes

- The answer is bound to a connection, not to a uid. A process learns about a
  login only when it holds a socket that it accepted on its own listener, and
  whose peer is a herdr bridge (same executable, bridge `argv`, no listening
  socket) within three parents of that login's priv. Nothing else is an input.
- A bridge never listens and connects only to a Herdr server socket, so a
  process holds an accepted socket whose peer is a bridge only when that bridge
  connected to its listener. The server does not pass its connections on.
  Herdr's own listening sockets (server, API, local attach) are filesystem
  sockets with mode 0600, so another user cannot reach them.
- A connection to another session's Herdr server (round 3): the peer is a
  server, which listens and does not run the bridge subcommand, and the
  connecting process holds the unnamed side, not an accepted socket. Refused.
  This also covers a pane of a Herdr server started directly by
  `ssh host herdr ...` that connects to its own server.
- A connection to a listening socket of some other process (an abstract Unix
  socket has no file permissions) refuses: that peer does not run herdr as a
  bridge.
- The caller checks compare an executable inode and `/proc` state; they are
  not a code-integrity check. A user can run the helper from a herdr process
  they control (for example with `LD_PRELOAD`), or from any process that
  passes the herdr binary as fd 4. The peer checks bind the answer. Such a
  caller learns only the login behind a bridge that it already holds an
  accepted connection from, which a herdr process of that user learns the
  same way. It cannot turn the answer into an attestation: only the server
  that loaded `/etc/herdr/server.key` signs, and it signs only on its own walk
  and its own helper call (smarty-dev#13408).
- `ptrace_scope` 1 or higher stops a process of the same user in another
  session from attaching to a bridge (or writing its memory) to make it
  connect elsewhere. Scope 1 still lets a process trace its own descendants;
  a bridge descends from sshd, not from another session.
- Residual: a process of the same user can replace that user's server socket
  file (the user owns the directory). The user's next bridge then connects to
  the impostor's listener, and the impostor learns that login's user, public
  key fingerprint and source address. Such a process can already take over
  the user's Herdr session this way; the fingerprint is of a public key and
  grants no access. The server never trusts the answer alone: it maps a
  principal only for its own verified bridge, through its own walk, with the
  same priv pid, and with a matching Tailscale node.
- A herdr update that replaces the binary gives new bridges a new inode. Until
  the server restarts, their connections stay unmapped; the server's own
  bridge check already compares the same inode.
- `SO_PEERCRED` names the process that connected. If that process exits and
  its pid is reused before the helper reads `/proc`, the helper sees the new
  process; it answers only if that process is also a herdr bridge within three
  parents of a priv, and the server's own pid pin and walk refuse a mismatch.
