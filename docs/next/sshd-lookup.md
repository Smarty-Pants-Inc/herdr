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

To remove it: `sudo rm /usr/local/libexec/herdr-sshd-lookup`.

## Contract

```text
herdr-sshd-lookup 3<&CONNECTION
```

The helper takes no arguments. Its only input is fd 3: the caller's accepted
connection from `herdr remote-client-bridge`. Any argument, or an fd 3 that is
not a connected `AF_UNIX` stream socket, refuses. The helper reads no
environment and no configuration. On success it prints one line and exits 0:

```json
{"pid":1234,"user":"paul","fingerprint":"SHA256:...","source_ip":"100.64.0.7"}
```

`pid` is the sshd priv process the helper found. In every other case it prints
nothing and exits nonzero.

The server passes the client connection as fd 3 (`dup2`, then every other
descriptor above 2 is closed at exec). It keeps its own walk from the bridge to
the priv and uses the answer only when the answer names the same priv pid and
user.

## Checks

The helper reads the kernel and `/proc` itself and answers only when all of
these are true:

- Fd 3 is a connected `AF_UNIX` stream socket (`SO_DOMAIN`, `SO_TYPE`,
  `getpeername`). `SO_PEERCRED` on it gives the peer pid: the process that
  connected, as the kernel recorded it at `connect`.
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
At last the helper reads the caller, every process of the walk and the listener
again; a change refuses the answer. A reused peer pid has another start time,
so it refuses too.

The helper keeps the journal group only while it starts `journalctl`, then drops
it for good. It sets `no_new_privs`, closes inherited descriptors above 3 at
start, and closes fd 3 once it has read the peer, before `journalctl` starts.

Logins whose listener restarted (the priv process is now a child of pid 1),
bridges more than three parents below the priv, and OpenSSH 9.8 and later
(`sshd-session`) stay unmapped.

## Threat notes

- The answer is bound to a connection, not to a uid. A process learns about a
  login only when it holds a connected stream socket whose peer descends from
  that login's priv within three parents. Nothing else is an input.
- A process cannot obtain a socket connected to another user's bridge: the
  bridge does not listen, and the server does not pass its connections on.
- A socket a process connects to itself (`socketpair`, or a connection to its
  own listener) has that process as peer. If the process does not descend from
  an sshd priv within three parents, the walk refuses. If the caller is the
  peer or sits in the walk, it refuses too.
- Residual: a process that holds a connection to any peer within three parents
  of a priv learns that login's user, key fingerprint and source address. This
  includes a process that itself runs within three parents of a priv (for
  example a pane of a Herdr server started directly by `ssh host herdr ...`
  learns that login), and a connection to a listening socket of a process
  started from someone's SSH login, where the socket permits it (abstract Unix
  sockets have no file permissions). The fingerprint is of a public key; it
  grants no access. The server never trusts the answer alone: it maps a
  principal only for its own verified bridge, through its own walk, with the
  same priv pid, and with a matching Tailscale node.
- `SO_PEERCRED` names the process that connected. If that process exits and
  its pid is reused before the helper reads `/proc`, the helper sees the new
  process; it answers only if that process also descends from a priv within
  three parents, and the server's own pid pin and walk refuse a mismatch.
