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
herdr-sshd-lookup <pid>
```

The input is exactly one canonical decimal pid. The helper reads no environment
and no configuration. On success it prints one line and exits 0:

```json
{"pid":1234,"user":"paul","fingerprint":"SHA256:...","source_ip":"100.64.0.7"}
```

In every other case it prints nothing and exits nonzero.

## Checks

The helper reads `/proc` itself and answers only when all of these are true:

- The pid is alive, its command line is exactly `sshd: <user> [priv]`, its
  `comm` is `sshd`, and all its uids are 0.
- Its parent is the system sshd listener: all uids 0, `comm` `sshd`, a child of
  pid 1, in the root-only cgroup `/system.slice/ssh.service` or
  `/system.slice/sshd.service`, and not younger than the login.
- When an executable link is readable, it is the inode of `/usr/sbin/sshd`.
  For root processes that a setgid helper cannot read, journald's trusted
  `_EXE=/usr/sbin/sshd` field on the login's records is the proof.
- The login's audit login uid (`/proc/<pid>/loginuid`, set once by
  `pam_loginuid`) is the helper's real uid, and the caller (the helper's parent)
  runs wholly as that uid. A process of another user cannot ask about the
  login.

It then runs `/usr/bin/journalctl` once with a cleared environment and fixed
arguments: the current boot, `_PID=<pid> _COMM=sshd _UID=0`, from the login's
start for 600 seconds, at most 64 records, JSON with only the needed fields. The
call has a 1.2 second deadline and a 256 KiB output limit. Every record must
carry this login's trusted `_PID`, `_UID=0`, `_COMM=sshd`, `_EXE`,
`_TRANSPORT=syslog`, `_BOOT_ID` and timestamps after the login started. Exactly
one record must start with `Accepted `, and it must be
`Accepted publickey for <user> from <ip> port <port> ssh2: <type> SHA256:<fp>`.
At last the helper reads the login, its listener and its caller again; a change
refuses the answer.

The helper keeps the journal group only while it starts `journalctl`, then drops
it for good. It sets `no_new_privs` and closes inherited descriptors above 2.

Logins whose listener restarted (the priv process is now a child of pid 1),
logins without `pam_loginuid`, and OpenSSH 9.8 and later (`sshd-session`) stay
unmapped.
