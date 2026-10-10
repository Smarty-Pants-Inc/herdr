pub(crate) const REMOTE_PREFERENCES_ENV_VAR: &str = "HERDR_REMOTE_PREFERENCES_IDENTITY";
pub(crate) const REATTACH_COMMAND_ENV_VAR: &str = "HERDR_REATTACH_COMMAND";
pub(crate) const REMOTE_KEYBINDINGS_ENV_VAR: &str = "HERDR_REMOTE_KEYBINDINGS";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteKeybindings {
    Local,
    Server,
}

impl RemoteKeybindings {
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "local" => Ok(Self::Local),
            "server" => Ok(Self::Server),
            _ => Err("--remote-keybindings must be 'local' or 'server'".to_string()),
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Server => "server",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteLaunch {
    pub(crate) target: String,
    pub(crate) keybindings: RemoteKeybindings,
    pub(crate) live_handoff: bool,
}

pub(crate) fn extract_remote_args(
    args: &[String],
) -> Result<(Vec<String>, Option<RemoteLaunch>), String> {
    let mut cleaned = Vec::with_capacity(args.len());
    if let Some(program) = args.first() {
        cleaned.push(program.clone());
    }

    let literal_slots = crate::cli::agent_channel_literal_slots(args);
    let mut remote_target = None;
    let mut keybindings = RemoteKeybindings::Local;
    let mut keybindings_seen = false;
    let mut live_handoff = false;
    let mut index = 1;
    while index < args.len() {
        let arg = &args[index];
        if literal_slots.contains(&index) {
            cleaned.push(arg.clone());
            index += 1;
            continue;
        }
        if arg == "--" {
            cleaned.extend_from_slice(&args[index..]);
            break;
        }
        if arg == "--handoff" {
            live_handoff = true;
            index += 1;
            continue;
        }
        if arg == "--remote" {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote".to_string());
            };
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote=") {
            if remote_target.is_some() {
                return Err("--remote can only be specified once".to_string());
            }
            remote_target = Some(validate_remote_target(value)?.to_owned());
            index += 1;
            continue;
        }
        if arg == "--remote-keybindings" {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            let Some(value) = args.get(index + 1) else {
                return Err("missing value for --remote-keybindings".to_string());
            };
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 2;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--remote-keybindings=") {
            if keybindings_seen {
                return Err("--remote-keybindings can only be specified once".to_string());
            }
            keybindings = RemoteKeybindings::parse(value)?;
            keybindings_seen = true;
            index += 1;
            continue;
        }

        cleaned.push(arg.clone());
        index += 1;
    }

    let remote = remote_target.map(|target| RemoteLaunch {
        target,
        keybindings,
        live_handoff,
    });
    if remote.is_none() && keybindings_seen {
        return Err("--remote-keybindings requires --remote".to_string());
    }
    if remote.is_none() && live_handoff {
        cleaned.push("--handoff".to_string());
    }

    Ok((cleaned, remote))
}

pub(crate) fn validate_remote_target(target: &str) -> Result<&str, String> {
    if target.is_empty() {
        return Err("missing value for --remote".to_string());
    }
    if target.starts_with('-') {
        return Err("--remote target must not start with '-'".to_string());
    }
    Ok(target)
}

#[cfg(test)]
mod channel_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn channel_global_extraction_preserves_literal_payload_and_guard_values() {
        for target in ["--remote", "--handoff", "--remote-keybindings=server"] {
            for text in [
                "--remote=other",
                "--remote-keybindings",
                "--handoff",
                "--session=literal",
            ] {
                let input = args(&[
                    "herdr",
                    "agent",
                    "prompt-guarded",
                    target,
                    text,
                    "--expected-terminal",
                    "--remote=terminal",
                    "--expected-registration-epoch",
                    "--handoff",
                    "--request-id",
                    "--remote-keybindings=literal",
                ]);
                let (cleaned, remote) = extract_remote_args(&input).unwrap();
                assert_eq!(cleaned, input);
                assert_eq!(remote, None);
            }
            let input = args(&["herdr", "agent", "channel-info", target]);
            assert_eq!(extract_remote_args(&input).unwrap(), (input, None));
        }
    }

    #[test]
    fn channel_global_extraction_keeps_separator_and_prefix_behavior() {
        let input = args(&[
            "herdr",
            "--remote",
            "server",
            "agent",
            "prompt-guarded",
            "--expected-terminal",
            "--remote=terminal",
            "--expected-registration-epoch",
            "--handoff",
            "--request-id",
            "--remote-keybindings=literal",
            "--",
            "--remote=target",
            "--handoff",
        ]);
        let (cleaned, remote) = extract_remote_args(&input).unwrap();
        assert_eq!(cleaned, [args(&["herdr"]), input[3..].to_vec()].concat());
        assert_eq!(remote.unwrap().target, "server");
        let input = args(&["herdr", "agent", "channel-info", "--", "--remote"]);
        assert_eq!(extract_remote_args(&input).unwrap(), (input, None));
    }

    #[test]
    fn existing_prompt_globals_remain_literal_in_both_positional_slots() {
        for target in ["worker", "--remote=target", "--handoff"] {
            let input = args(&["herdr", "agent", "prompt", target, "--remote=payload"]);
            assert_eq!(extract_remote_args(&input).unwrap(), (input, None));
        }
        let input = args(&["herdr", "status", "--remote=server"]);
        let (cleaned, remote) = extract_remote_args(&input).unwrap();
        assert_eq!(cleaned, args(&["herdr", "status"]));
        assert_eq!(remote.unwrap().target, "server");
    }
}
