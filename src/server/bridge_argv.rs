//! The arguments of a Herdr SSH bridge, exactly as the launcher writes them
//! (`src/remote/attach.rs`, `session_args` and `bridge_command_with_idle_timeout`).
//! The server's bridge check and the setgid `herdr-sshd-lookup` helper both use
//! this one file (the helper includes it by path), so the two cannot drift
//! (smarty-dev#13408 F2).
//!
//! Standard library only: the helper links nothing else.

/// The bridge subcommand.
pub(crate) const BRIDGE_SUBCOMMAND: &str = "remote-client-bridge";
/// The only option the launcher passes after the subcommand.
const IDLE_TIMEOUT_OPTION: &str = "--idle-timeout-v1";
/// `src/session.rs` `MAX_SESSION_NAME_LEN`.
const MAX_SESSION_NAME_LEN: usize = 64;

/// One session name as `herdr --session` accepts it (`session::validate_name`).
fn session_name(name: &[u8]) -> bool {
    (1..=MAX_SESSION_NAME_LEN).contains(&name.len())
        && name != b"."
        && name != b".."
        && name
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
}

/// `args` is argv without argv[0]. True only for
/// `[--session S | --session=S] remote-client-bridge [--idle-timeout-v1]`.
pub(crate) fn is_bridge_argv<A: AsRef<[u8]>>(args: &[A]) -> bool {
    let args: Vec<&[u8]> = args.iter().map(AsRef::as_ref).collect();
    let rest = match args.as_slice() {
        [b"--session", name, rest @ ..] if session_name(name) => rest,
        [first, rest @ ..] if first.strip_prefix(b"--session=").is_some_and(session_name) => rest,
        rest => rest,
    };
    let bridge = BRIDGE_SUBCOMMAND.as_bytes();
    match rest {
        [command] => *command == bridge,
        [command, option] => *command == bridge && *option == IDLE_TIMEOUT_OPTION.as_bytes(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_argv_accepts_the_launcher_forms_only() {
        for args in [
            &["remote-client-bridge"][..],
            &["remote-client-bridge", "--idle-timeout-v1"],
            &["--session", "smarty-dev", "remote-client-bridge"],
            &[
                "--session",
                "playful",
                "remote-client-bridge",
                "--idle-timeout-v1",
            ],
            &["--session=smarty-dev", "remote-client-bridge"],
            &[
                "--session=a.b_c-1",
                "remote-client-bridge",
                "--idle-timeout-v1",
            ],
        ] {
            assert!(is_bridge_argv(args), "{args:?}");
        }
        let long = "a".repeat(MAX_SESSION_NAME_LEN + 1);
        for args in [
            &[][..],
            &["server"],
            &["remote-client-bridge-x"],
            &["remote-api-bridge"],
            &["--session", "s", "server"],
            &["--session", "s"],
            &["--session"],
            &["--session="],
            &["--session=", "remote-client-bridge"],
            &["--session", "", "remote-client-bridge"],
            &["--session", "..", "remote-client-bridge"],
            &["--session", "a/b", "remote-client-bridge"],
            &["--session", &long, "remote-client-bridge"],
            &["--session", "a", "--session", "b", "remote-client-bridge"],
            &["--session=a", "--session=b", "remote-client-bridge"],
            &["--session", "a", "--session=b", "remote-client-bridge"],
            &["remote-client-bridge", "--session", "a"],
            &[
                "remote-client-bridge",
                "--idle-timeout-v1",
                "--idle-timeout-v1",
            ],
            &["remote-client-bridge", "--idle-timeout-v2"],
            &["remote-client-bridge", "extra"],
            &["remote-client-bridge", ""],
            &["--remote", "remote-client-bridge"],
            &["--", "remote-client-bridge"],
        ] {
            assert!(!is_bridge_argv(args), "{args:?}");
        }
    }
}
