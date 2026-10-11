use crate::api::schema::{
    AgentChannelInfoParams, AgentDraftStateParams, AgentPromptGuardedParams, Method, Request,
};

const GUARDED_USAGE: &str = "usage: herdr agent prompt-guarded <target> <text> --expected-terminal TERMINAL_ID --expected-registration-epoch EPOCH --request-id ID [--timeout-ms MS] [--allow-cross-pane] [--if-draft-empty]";

// These are command-owned values, not global launch options. Keep this list in
// sync with the guarded parser so the session/remote extractors cannot consume
// an opaque identity that happens to look like a global option.
fn value_option(arg: &str) -> Option<&str> {
    let name = arg.split_once('=').map_or(arg, |(name, _)| name);
    matches!(
        name,
        "--expected-terminal" | "--expected-registration-epoch" | "--request-id" | "--timeout-ms"
    )
    .then_some(name)
}

fn skip_launch_options(args: &[String], mut index: usize) -> usize {
    while let Some(arg) = args.get(index) {
        match arg.as_str() {
            "--session" | "--remote" | "--remote-keybindings" => index += 2,
            "--handoff" => index += 1,
            value
                if value.starts_with("--session=")
                    || value.starts_with("--remote=")
                    || value.starts_with("--remote-keybindings=") =>
            {
                index += 1;
            }
            _ => break,
        }
    }
    index
}

/// Locate literal command data before global session/remote extraction. This
/// deliberately does not change extraction for unrelated command paths.
pub(crate) fn literal_slots(args: &[String]) -> Vec<usize> {
    let mut index = skip_launch_options(args, 1);
    if args.get(index).map(String::as_str) != Some("agent") {
        return Vec::new();
    }
    index = skip_launch_options(args, index + 1);
    let Some(command) = args.get(index).map(String::as_str) else {
        return Vec::new();
    };
    index += 1;
    // Protect exactly the TARGET/TEXT the prompt parser will read, from the
    // same classification it uses.
    if command == "prompt" {
        let argv = classify_prompt_argv(&args[index..]);
        return [argv.target, argv.text]
            .into_iter()
            .flatten()
            .map(|slot| index + slot)
            .collect();
    }
    let positional_count = match command {
        "channel-info" | "draft-state" => 1,
        "prompt-guarded" => 2,
        _ => return Vec::new(),
    };
    let mut slots = Vec::new();
    let mut positionals = 0;
    while let Some(arg) = args.get(index) {
        if arg == "--" {
            // Both global extractors preserve everything after the separator.
            break;
        }
        // Once TARGET is present, the next argv is literal TEXT, even if it
        // matches a known command option. Use -- to make a flag-shaped TARGET
        // that matches one of the guarded options literal as well.
        if positionals == 1 && positional_count == 2 {
            slots.push(index);
            positionals += 1;
        } else if command == "prompt-guarded" && value_option(arg).is_some() {
            if !arg.contains('=') && index + 1 < args.len() {
                slots.push(index + 1);
                index += 1;
            }
        } else if (command == "prompt-guarded"
            && matches!(arg.as_str(), "--allow-cross-pane" | "--if-draft-empty"))
            || (command == "draft-state" && arg == "--allow-cross-pane")
        {
            // These flags have no value.
        } else if positionals < positional_count {
            slots.push(index);
            positionals += 1;
        }
        index += 1;
    }
    slots
}

const ALLOW_CROSS_PANE: &str = "--allow-cross-pane";

/// Roles in `agent prompt` argv (the argv after `prompt`), as indices into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PromptArgv {
    pub(super) target: Option<usize>,
    pub(super) text: Option<usize>,
    /// Set by an opt-in before TARGET or right after it. An opt-in after TEXT
    /// is an ordinary trailing option, parsed with the others from `options`.
    pub(super) cross_pane_opt_in: bool,
    /// First index of the trailing options (after TEXT).
    pub(super) options: usize,
}

/// The one classification of `agent prompt` argv (smarty-dev#9677). Both the
/// prompt parser and the global session/remote extractors (through
/// `literal_slots`) use it, so they cannot disagree on TARGET, TEXT or the
/// opt-in.
///
/// Global launch options (`--session`, `--remote`, `--remote-keybindings` with
/// their values in either form, and `--handoff`) before the first `--` are
/// identified first; opt-in positions are counted on the remaining argv. An
/// exact `--allow-cross-pane` is the cross-pane opt-in only:
/// (a) leading, before TARGET, possibly repeated; or
/// (b) right after TARGET, when a next argument exists and does not start
///     with `-`; that argument is TEXT.
/// Then globals before TEXT are extracted and are not TARGET or TEXT.
///
/// Otherwise the existing contract holds unchanged: the first two argv are
/// the literal TARGET and TEXT, whatever their shape, so
/// `<pane> --allow-cross-pane` and `<pane> --allow-cross-pane --wait` send the
/// literal text `--allow-cross-pane`. A trailing `--allow-cross-pane` (c) is
/// parsed as an option after TEXT, as before.
///
/// Removing any of the identified globals does not change the result, so
/// classifying before, between and after the two extractors agrees.
pub(super) fn classify_prompt_argv(args: &[String]) -> PromptArgv {
    let mut positions = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--" {
            // Neither extractor looks past the separator.
            positions.extend(index..args.len());
            break;
        }
        let next = skip_launch_options(args, index);
        if next == index {
            positions.push(index);
            index += 1;
        } else {
            index = next;
        }
    }
    let arg = |slot: usize| positions.get(slot).map(|&index| args[index].as_str());
    let leading = positions
        .iter()
        .take_while(|&&index| args[index] == ALLOW_CROSS_PANE)
        .count();
    // Nothing after a `--` TARGET is an opt-in.
    let after_target = arg(leading).is_some_and(|target| target != "--")
        && arg(leading + 1) == Some(ALLOW_CROSS_PANE)
        && arg(leading + 2).is_some_and(|next| !next.starts_with('-'));
    if leading == 0 && !after_target {
        return PromptArgv {
            target: (!args.is_empty()).then_some(0),
            text: (args.len() > 1).then_some(1),
            cross_pane_opt_in: false,
            options: args.len().min(2),
        };
    }
    let target = positions.get(leading).copied();
    let text = positions
        .get(leading + 1 + usize::from(after_target))
        .copied();
    PromptArgv {
        target,
        text,
        cross_pane_opt_in: true,
        options: text.map_or(args.len(), |text| text + 1),
    }
}

fn parse_terminal_target(args: &[String], command: &str) -> Result<String, i32> {
    let target = match args {
        [target] if target != "--" => target,
        [separator, target] if separator == "--" => target,
        _ => {
            eprintln!("usage: herdr agent {command} <target>");
            return Err(2);
        }
    };
    if target.is_empty() {
        eprintln!("agent {command} requires a nonempty target");
        return Err(2);
    }
    Ok(target.clone())
}

fn parse_channel_info_args(args: &[String]) -> Result<AgentChannelInfoParams, i32> {
    parse_terminal_target(args, "channel-info").map(|target| AgentChannelInfoParams { target })
}

const DRAFT_STATE_USAGE: &str = "usage: herdr agent draft-state <target> [--allow-cross-pane]";

/// TARGET plus the same explicit cross-pane opt-in as `agent prompt`. Any other
/// flag-shaped argv stays a literal target; use `--` for a literal `--allow-cross-pane`.
fn parse_draft_state_args(args: &[String]) -> Result<AgentDraftStateParams, i32> {
    let mut target = None;
    let mut allow_cross_pane = false;
    let mut options_ended = false;
    for arg in args {
        if !options_ended && target.is_none() && arg == "--" {
            options_ended = true;
        } else if !options_ended && arg == "--allow-cross-pane" {
            if allow_cross_pane {
                eprintln!("--allow-cross-pane may only be specified once");
                return Err(2);
            }
            allow_cross_pane = true;
        } else if target.is_none() {
            target = Some(arg.clone());
        } else {
            eprintln!("{DRAFT_STATE_USAGE}");
            return Err(2);
        }
    }
    let Some(target) = target else {
        eprintln!("{DRAFT_STATE_USAGE}");
        return Err(2);
    };
    if target.is_empty() {
        eprintln!("agent draft-state requires a nonempty target");
        return Err(2);
    }
    Ok(AgentDraftStateParams {
        target,
        allow_cross_pane,
    })
}

pub(super) fn draft_state(args: &[String]) -> std::io::Result<i32> {
    let params = match parse_draft_state_args(args) {
        Ok(params) => params,
        Err(code) => return Ok(code),
    };
    super::super::print_response(&super::super::send_request(&Request {
        id: "cli:agent:draft-state".into(),
        method: Method::AgentDraftState(params),
    })?)
}

pub(super) fn channel_info(args: &[String]) -> std::io::Result<i32> {
    let params = match parse_channel_info_args(args) {
        Ok(params) => params,
        Err(code) => return Ok(code),
    };
    // Query the terminal directly; agent.get would incorrectly require agent
    // detection and would hide an undetected terminal's missing channel.
    super::super::print_response(&super::super::send_request(&Request {
        id: "cli:agent:channel-info".into(),
        method: Method::AgentChannelInfo(params),
    })?)
}

fn parse_prompt_guarded_args(args: &[String]) -> Result<AgentPromptGuardedParams, i32> {
    let mut target = None;
    let mut text = None;
    let mut expected_terminal = None;
    let mut expected_registration_epoch = None;
    let mut request_id = None;
    let mut timeout_ms = None;
    let mut allow_cross_pane = false;
    let mut if_draft_empty = false;
    let mut options_ended = false;
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if target.is_some() && text.is_none() {
            text = Some(arg.clone());
        } else if !options_ended && arg == "--" {
            options_ended = true;
        } else if !options_ended && value_option(arg).is_some() {
            let (option, value) = if let Some((option, value)) = arg.split_once('=') {
                (option, value)
            } else {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("missing value for {arg}");
                    return Err(2);
                };
                index += 1;
                (arg.as_str(), value.as_str())
            };
            if value.is_empty() {
                eprintln!("{option} requires a nonempty value");
                return Err(2);
            }
            let duplicate = match option {
                "--expected-terminal" => expected_terminal.replace(value.to_owned()).is_some(),
                "--expected-registration-epoch" => expected_registration_epoch
                    .replace(value.to_owned())
                    .is_some(),
                "--request-id" => request_id.replace(value.to_owned()).is_some(),
                "--timeout-ms" => {
                    let Ok(value) = value.parse::<u64>() else {
                        eprintln!("--timeout-ms must be an integer between 1 and 300000");
                        return Err(2);
                    };
                    if !(1..=300_000).contains(&value) {
                        eprintln!("--timeout-ms must be an integer between 1 and 300000");
                        return Err(2);
                    }
                    timeout_ms.replace(value).is_some()
                }
                _ => unreachable!("value_option only returns known guarded options"),
            };
            if duplicate {
                eprintln!("{option} may only be specified once");
                return Err(2);
            }
        } else if !options_ended && arg == "--allow-cross-pane" {
            allow_cross_pane = true;
        } else if !options_ended && arg == "--if-draft-empty" {
            if if_draft_empty {
                eprintln!("--if-draft-empty may only be specified once");
                return Err(2);
            }
            if_draft_empty = true;
        } else if target.is_none() {
            target = Some(arg.clone());
        } else {
            eprintln!("unknown option or extra argument: {arg}");
            return Err(2);
        }
        index += 1;
    }
    let (
        Some(target),
        Some(text),
        Some(expected_terminal),
        Some(expected_registration_epoch),
        Some(request_id),
    ) = (
        target,
        text,
        expected_terminal,
        expected_registration_epoch,
        request_id,
    )
    else {
        eprintln!("{GUARDED_USAGE}");
        return Err(2);
    };
    if target.is_empty() || text.is_empty() {
        eprintln!("agent prompt-guarded requires a nonempty target and text");
        return Err(2);
    }
    Ok(AgentPromptGuardedParams {
        target,
        text,
        expected_terminal,
        expected_registration_epoch,
        request_id,
        timeout_ms,
        allow_cross_pane,
        if_draft_empty,
    })
}

fn draft_guard_refusal(reason: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "cli:agent:prompt-guarded",
        "error": {"code": "agent_prompt_rejected", "reason": reason, "message": message}
    })
}

fn draft_guard_pins(response: &serde_json::Value) -> Result<(String, String), serde_json::Value> {
    if response.get("error").is_some() {
        return Err(match response["error"]["code"].as_str() {
            Some("method_not_found" | "unknown_method") => draft_guard_refusal(
                "unsupported",
                "server does not support channel discovery; guarded prompt was not sent",
            ),
            _ => response.clone(),
        });
    }
    let info = &response["result"];
    match info["ready"].as_bool() {
        Some(false) => {
            return Err(draft_guard_refusal(
                "unregistered",
                "target has no registered channel; guarded prompt was not sent",
            ));
        }
        Some(true) => {}
        None => {
            return Err(draft_guard_refusal(
                "unknown",
                "channel readiness is unknown; guarded prompt was not sent",
            ));
        }
    }
    // Strict boolean capability checking also protects older servers that
    // implement prompt_guarded but would silently ignore if_draft_empty.
    if info["draft_guard"].as_bool() != Some(true) {
        return Err(draft_guard_refusal(
            "unsupported",
            "channel does not advertise draft_guard: true; guarded prompt was not sent",
        ));
    }
    let pins = info["terminal_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .zip(
            info["registration_epoch"]
                .as_str()
                .filter(|value| !value.is_empty()),
        );
    match pins {
        Some((terminal, epoch)) => Ok((terminal.to_owned(), epoch.to_owned())),
        None => Err(draft_guard_refusal(
            "unknown",
            "channel identity is unknown; guarded prompt was not sent",
        )),
    }
}

fn discover_draft_guard(
    target: &str,
) -> std::io::Result<Result<(String, String), serde_json::Value>> {
    let response = super::super::send_request(&Request {
        id: "cli:agent:prompt-guarded:channel-info".into(),
        method: Method::AgentChannelInfo(AgentChannelInfoParams {
            target: target.to_owned(),
        }),
    })?;
    Ok(draft_guard_pins(&response))
}

fn fresh_prompt_request_id() -> std::io::Result<String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_REQUEST: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    Ok(format!(
        "cli:agent:prompt:{}:{time}:{}",
        std::process::id(),
        NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
    ))
}

pub(super) fn prompt_if_draft_empty(
    target: &str,
    text: &str,
    allow_cross_pane: bool,
) -> std::io::Result<i32> {
    if target.is_empty() || text.is_empty() {
        eprintln!("agent prompt --if-draft-empty requires a nonempty target and text");
        return Ok(2);
    }
    let (expected_terminal, expected_registration_epoch) = match discover_draft_guard(target)? {
        Ok(pins) => pins,
        Err(response) => return super::super::print_response(&response),
    };
    send_guarded_prompt(AgentPromptGuardedParams {
        target: target.to_owned(),
        text: text.to_owned(),
        expected_terminal,
        expected_registration_epoch,
        request_id: fresh_prompt_request_id()?,
        timeout_ms: None,
        allow_cross_pane,
        if_draft_empty: true,
    })
}

pub(super) fn prompt_guarded(args: &[String]) -> std::io::Result<i32> {
    let params = match parse_prompt_guarded_args(args) {
        Ok(params) => params,
        Err(code) => return Ok(code),
    };
    if params.if_draft_empty {
        if let Err(response) = discover_draft_guard(&params.target)? {
            return super::super::print_response(&response);
        }
    }
    send_guarded_prompt(params)
}

fn send_guarded_prompt(params: AgentPromptGuardedParams) -> std::io::Result<i32> {
    // A distinct method and pins fail closed on old/replaced servers. Never turn
    // this into agent.prompt, PTY input, or a retry after lost transport.
    super::super::print_response(&super::super::send_request(&Request {
        id: "cli:agent:prompt-guarded".into(),
        method: Method::AgentPromptGuarded(params),
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn guarded_args(target: &str, text: &str) -> Vec<String> {
        args(&[
            target,
            text,
            "--expected-terminal",
            "opaque:λ/42",
            "--expected-registration-epoch",
            "epoch=日本語",
            "--request-id",
            "caller:key",
        ])
    }

    #[test]
    fn guarded_draft_flag_is_opt_in_and_does_not_consume_literal_text() {
        let mut input = guarded_args("worker", "--if-draft-empty");
        assert!(!parse_prompt_guarded_args(&input).unwrap().if_draft_empty);
        input.push("--if-draft-empty".into());
        let parsed = parse_prompt_guarded_args(&input).unwrap();
        assert!(parsed.if_draft_empty);
        assert_eq!(parsed.text, "--if-draft-empty");
        input.insert(0, "--if-draft-empty".into());
        assert!(parse_prompt_guarded_args(&input).is_err());
        input.pop();
        assert!(parse_prompt_guarded_args(&input).unwrap().if_draft_empty);
        input.push("--if-draft-empty=true".into());
        assert!(parse_prompt_guarded_args(&input).is_err());
    }

    #[test]
    fn draft_guard_discovery_requires_strict_capability_and_complete_identity() {
        let valid = serde_json::json!({"result": {
            "terminal_id": "term", "registration_epoch": "epoch",
            "ready": true, "draft_guard": true,
        }});
        assert_eq!(
            draft_guard_pins(&valid).unwrap(),
            ("term".into(), "epoch".into())
        );
        let mut unregistered = valid.clone();
        unregistered["result"]["ready"] = false.into();
        assert_eq!(
            draft_guard_pins(&unregistered).unwrap_err()["error"]["reason"],
            "unregistered"
        );
        for cap in [
            serde_json::Value::Null,
            false.into(),
            "true".into(),
            1.into(),
        ] {
            let mut response = valid.clone();
            response["result"]["draft_guard"] = cap;
            let refusal = draft_guard_pins(&response).unwrap_err();
            assert_eq!(refusal["error"]["code"], "agent_prompt_rejected");
            assert_eq!(refusal["error"]["reason"], "unsupported");
        }
        let mut missing = valid.clone();
        missing["result"]
            .as_object_mut()
            .unwrap()
            .remove("draft_guard");
        assert_eq!(
            draft_guard_pins(&missing).unwrap_err()["error"]["reason"],
            "unsupported"
        );
        for field in ["terminal_id", "registration_epoch", "ready"] {
            let mut response = valid.clone();
            response["result"].as_object_mut().unwrap().remove(field);
            assert_eq!(
                draft_guard_pins(&response).unwrap_err()["error"]["reason"],
                "unknown"
            );
        }
        for field in ["terminal_id", "registration_epoch"] {
            let mut response = valid.clone();
            response["result"][field] = "".into();
            assert_eq!(
                draft_guard_pins(&response).unwrap_err()["error"]["reason"],
                "unknown"
            );
        }
        for code in ["unknown_method", "method_not_found"] {
            let response = serde_json::json!({"error": {"code": code}});
            assert_eq!(
                draft_guard_pins(&response).unwrap_err()["error"]["reason"],
                "unsupported"
            );
        }
        let error = serde_json::json!({"id": "query", "error": {"code": "target_not_found"}});
        assert_eq!(draft_guard_pins(&error).unwrap_err(), error);
    }

    #[test]
    fn guarded_preserves_literal_target_text_and_exact_pins() {
        for target in ["w1:p2", "--session=target", "--remote", "--handoff"] {
            for text in [
                "--help",
                "--",
                "-h",
                "--session=payload",
                "--remote",
                "--allow-cross-pane",
                "--expected-terminal=payload",
                "$(touch /tmp/not-run);\nλ 日本語",
            ] {
                let parsed = parse_prompt_guarded_args(&guarded_args(target, text)).unwrap();
                assert_eq!(parsed.target, target);
                assert_eq!(parsed.text, text);
                assert_eq!(parsed.expected_terminal, "opaque:λ/42");
                assert_eq!(parsed.expected_registration_epoch, "epoch=日本語");
                assert_eq!(parsed.request_id, "caller:key");
                assert_eq!(parsed.timeout_ms, None);
                assert!(!parsed.allow_cross_pane);
                assert!(!parsed.if_draft_empty);
            }
        }
    }

    #[test]
    fn guarded_accepts_equals_options_and_option_first_separator() {
        let parsed = parse_prompt_guarded_args(&args(&[
            "--expected-terminal=term=opaque",
            "--expected-registration-epoch=epoch=opaque",
            "--request-id=req=opaque",
            "--timeout-ms=300000",
            "--allow-cross-pane",
            "--",
            "--expected-terminal",
            "--session=literal",
        ]))
        .unwrap();
        assert_eq!(parsed.target, "--expected-terminal");
        assert_eq!(parsed.text, "--session=literal");
        assert_eq!(parsed.expected_terminal, "term=opaque");
        assert_eq!(parsed.expected_registration_epoch, "epoch=opaque");
        assert_eq!(parsed.request_id, "req=opaque");
        assert_eq!(parsed.timeout_ms, Some(300_000));
        assert!(parsed.allow_cross_pane);
    }

    #[test]
    fn guarded_requires_each_pin_and_rejects_duplicate_or_invalid_options() {
        for option in [
            "--expected-terminal",
            "--expected-registration-epoch",
            "--request-id",
        ] {
            let valid = guarded_args("worker", "text");
            let mut missing = valid.clone();
            let index = missing.iter().position(|value| value == option).unwrap();
            missing.drain(index..index + 2);
            assert!(parse_prompt_guarded_args(&missing).is_err());
            let mut empty = valid.clone();
            empty[index + 1].clear();
            assert!(parse_prompt_guarded_args(&empty).is_err());
            let mut duplicate = valid.clone();
            duplicate.extend(args(&[option, "other"]));
            assert!(parse_prompt_guarded_args(&duplicate).is_err());
            let mut no_value = valid;
            no_value.push(option.to_owned());
            assert!(parse_prompt_guarded_args(&no_value).is_err());
        }
        for timeout in ["", "0", "300001", "-1", "many", "18446744073709551616"] {
            let mut input = guarded_args("worker", "text");
            input.extend(args(&["--timeout-ms", timeout]));
            assert!(parse_prompt_guarded_args(&input).is_err(), "{timeout}");
        }
        for extra in ["--wait", "--timeout=1", "--bogus", "extra-text"] {
            let mut input = guarded_args("worker", "text");
            input.push(extra.to_owned());
            assert!(parse_prompt_guarded_args(&input).is_err(), "{extra}");
        }
        assert!(parse_prompt_guarded_args(&guarded_args("", "text")).is_err());
        assert!(parse_prompt_guarded_args(&guarded_args("worker", "")).is_err());
    }

    #[test]
    fn guarded_separator_does_not_reenable_options() {
        let mut input = args(&[
            "--expected-terminal",
            "term",
            "--expected-registration-epoch",
            "epoch",
            "--request-id",
            "request",
            "--",
            "worker",
            "--timeout-ms=1",
        ]);
        let parsed = parse_prompt_guarded_args(&input).unwrap();
        assert_eq!(parsed.text, "--timeout-ms=1");
        assert_eq!(parsed.timeout_ms, None);
        input.push("--allow-cross-pane".into());
        assert!(parse_prompt_guarded_args(&input).is_err());
        input.pop();
        *input.last_mut().unwrap() = "--if-draft-empty".into();
        let parsed = parse_prompt_guarded_args(&input).unwrap();
        assert_eq!(parsed.text, "--if-draft-empty");
        assert!(!parsed.if_draft_empty);
        input.push("--if-draft-empty".into());
        assert!(parse_prompt_guarded_args(&input).is_err());
    }

    #[test]
    fn channel_info_accepts_literal_terminal_target_without_agent_lookup() {
        for target in [
            "undetected-terminal",
            "--session=literal",
            "--remote",
            "--handoff",
        ] {
            assert_eq!(
                parse_channel_info_args(&args(&[target])).unwrap().target,
                target
            );
            assert_eq!(
                parse_channel_info_args(&args(&["--", target]))
                    .unwrap()
                    .target,
                target
            );
        }
        for invalid in [&[][..], &[""][..], &["--"][..], &["one", "two"][..]] {
            assert!(parse_channel_info_args(&args(invalid)).is_err());
        }
    }

    #[test]
    fn channel_literals_survive_session_and_remote_extractors() {
        let env = crate::environment::test_env();
        env.remove(crate::session::SESSION_ENV_VAR);
        for target in ["worker", "--session=target", "--remote=target", "--handoff"] {
            for text in ["--session", "--session=payload", "--remote", "--handoff"] {
                let mut input = args(&["herdr", "agent", "prompt-guarded", "--if-draft-empty"]);
                input.extend(args(&[
                    target,
                    text,
                    "--expected-terminal",
                    "--session=terminal",
                    "--expected-registration-epoch",
                    "--session=epoch",
                    "--request-id",
                    "--session=request",
                ]));
                let cleaned = crate::session::configure_from_args(&input).unwrap();
                assert_eq!(cleaned, input);
                let (cleaned, remote) = crate::remote::extract_remote_args(&cleaned).unwrap();
                assert_eq!(cleaned, input);
                assert!(remote.is_none());
                let parsed = parse_prompt_guarded_args(&cleaned[3..]).unwrap();
                assert_eq!(parsed.target, target);
                assert_eq!(parsed.text, text);
                assert_eq!(parsed.expected_terminal, "--session=terminal");
                assert_eq!(parsed.expected_registration_epoch, "--session=epoch");
                assert_eq!(parsed.request_id, "--session=request");
                assert!(parsed.if_draft_empty);
                assert_eq!(crate::session::active_name(), None);
            }
            for command in ["channel-info", "draft-state"] {
                let input = args(&["herdr", "agent", command, target]);
                assert_eq!(crate::session::configure_from_args(&input).unwrap(), input);
                let (cleaned, remote) = crate::remote::extract_remote_args(&input).unwrap();
                assert_eq!(cleaned, input);
                assert!(remote.is_none());
                let parsed = if command == "draft-state" {
                    parse_draft_state_args(&cleaned[3..]).unwrap().target
                } else {
                    parse_terminal_target(&cleaned[3..], command).unwrap()
                };
                assert_eq!(parsed, target);
            }
            // The opt-in flag on either side of TARGET is not a literal slot and
            // leaves a flag-shaped TARGET intact through both global extractors.
            for flag_first in [true, false] {
                let mut input = args(&["herdr", "agent", "draft-state", target]);
                input.insert(if flag_first { 3 } else { 4 }, "--allow-cross-pane".into());
                assert_eq!(literal_slots(&input), [if flag_first { 4 } else { 3 }]);
                assert_eq!(crate::session::configure_from_args(&input).unwrap(), input);
                let (cleaned, remote) = crate::remote::extract_remote_args(&input).unwrap();
                assert_eq!(cleaned, input);
                assert!(remote.is_none());
                let parsed = parse_draft_state_args(&cleaned[3..]).unwrap();
                assert_eq!(parsed.target, target);
                assert!(parsed.allow_cross_pane);
            }
        }
    }

    #[test]
    fn convenience_guard_literals_survive_global_extraction() {
        let env = crate::environment::test_env();
        env.remove(crate::session::SESSION_ENV_VAR);
        for target in [
            "--session=target",
            "--remote",
            "--handoff",
            "--if-draft-empty",
        ] {
            for text in [
                "--session",
                "--remote=text",
                "--handoff",
                "--if-draft-empty",
                "--help",
            ] {
                let input = args(&["herdr", "agent", "prompt", target, text, "--if-draft-empty"]);
                assert_eq!(literal_slots(&input), [3, 4]);
                let cleaned = crate::session::configure_from_args(&input).unwrap();
                assert_eq!(cleaned, input);
                let (cleaned, remote) = crate::remote::extract_remote_args(&cleaned).unwrap();
                assert_eq!(cleaned, input);
                assert!(remote.is_none());
                assert_eq!(crate::session::active_name(), None);
            }
        }
    }

    #[test]
    fn draft_state_parser_preserves_literal_target_and_rejects_options() {
        for target in [
            "terminal",
            "--session=literal",
            "--if-draft-empty",
            "--help",
        ] {
            for input in [vec![target], vec!["--", target]] {
                let parsed = parse_draft_state_args(&args(&input)).unwrap();
                assert_eq!(parsed.target, target);
                assert!(!parsed.allow_cross_pane, "opt-in is never implicit");
            }
            for input in [
                vec![target, "--allow-cross-pane"],
                vec!["--allow-cross-pane", target],
                vec!["--allow-cross-pane", "--", target],
            ] {
                let parsed = parse_draft_state_args(&args(&input)).unwrap();
                assert_eq!(parsed.target, target);
                assert!(parsed.allow_cross_pane);
            }
        }
        let literal = parse_draft_state_args(&args(&["--", "--allow-cross-pane"])).unwrap();
        assert_eq!(literal.target, "--allow-cross-pane");
        assert!(!literal.allow_cross_pane);
        for input in [
            &[][..],
            &[""][..],
            &["--"][..],
            &["--allow-cross-pane"][..],
            &["--allow-cross-pane", "--allow-cross-pane", "worker"][..],
            &["worker", "--"][..],
            &["worker", "--wait"][..],
            &["worker", "--timeout", "1"][..],
        ] {
            assert!(parse_draft_state_args(&args(input)).is_err(), "{input:?}");
        }
    }

    #[test]
    fn channel_literal_slots_cover_prefix_and_option_first_identity_values() {
        let input = args(&[
            "herdr",
            "--session",
            "server",
            "agent",
            "prompt-guarded",
            "--expected-terminal",
            "--session=terminal",
            "--expected-registration-epoch",
            "--remote=epoch",
            "--request-id",
            "--handoff",
            "--allow-cross-pane",
            "--session=target",
            "--remote=text",
        ]);
        assert_eq!(literal_slots(&input), [6, 8, 10, 12, 13]);
        let mut guarded = input.clone();
        guarded.insert(5, "--if-draft-empty".into());
        assert_eq!(literal_slots(&guarded), [7, 9, 11, 13, 14]);
        for command in ["prompt", "prompt-guarded"] {
            let input = args(&[
                "herdr",
                "agent",
                command,
                "--session=target",
                "--remote=text",
            ]);
            assert_eq!(literal_slots(&input), [3, 4]);
        }
        assert!(literal_slots(&args(&["herdr", "pane", "run", "w1:p1", "text"])).is_empty());
    }

    #[test]
    fn prompt_argv_takes_cross_pane_opt_in_only_in_unambiguous_positions() {
        const FLAG: &str = "--allow-cross-pane";
        let classify = |input: &[&str]| {
            let argv = classify_prompt_argv(&args(input));
            (argv.target, argv.text, argv.cross_pane_opt_in, argv.options)
        };
        for (input, expected) in [
            // (a) leading, possibly repeated.
            (&[FLAG, "w1", "hi"][..], (Some(1), Some(2), true, 3)),
            (&[FLAG, FLAG, "w1", "hi"][..], (Some(2), Some(3), true, 4)),
            (
                &[FLAG, "w1", "hi", "--wait"][..],
                (Some(1), Some(2), true, 3),
            ),
            (&[FLAG, "--", "hi"][..], (Some(1), Some(2), true, 3)),
            (&[FLAG, "w1"][..], (Some(1), None, true, 2)),
            (&[FLAG][..], (None, None, true, 1)),
            // (b) right after TARGET, before a next argument not starting with '-'.
            (&["w1", FLAG, "hi"][..], (Some(0), Some(2), true, 3)),
            (
                &["w1", FLAG, "hi", "--wait"][..],
                (Some(0), Some(2), true, 3),
            ),
            (&[FLAG, "w1", FLAG, "hi"][..], (Some(1), Some(3), true, 4)),
            // (c) after TEXT it is a trailing option, as before.
            (&["w1", "hi", FLAG][..], (Some(0), Some(1), false, 2)),
            // Everything else keeps the literal first-two-argv contract.
            (&["w1", FLAG][..], (Some(0), Some(1), false, 2)),
            (&["w1", FLAG, "--wait"][..], (Some(0), Some(1), false, 2)),
            (&["w1", FLAG, "-x"][..], (Some(0), Some(1), false, 2)),
            (&["w1", FLAG, "--"][..], (Some(0), Some(1), false, 2)),
            (&["w1", FLAG, FLAG][..], (Some(0), Some(1), false, 2)),
            (&["--", FLAG, "hi"][..], (Some(0), Some(1), false, 2)),
            (&[FLAG, "--", FLAG, "hi"][..], (Some(1), Some(2), true, 3)),
            (&["w1", "hi"][..], (Some(0), Some(1), false, 2)),
            (&["w1"][..], (Some(0), None, false, 1)),
            (&[][..], (None, None, false, 0)),
            (
                &["--session=target", "--remote=text"][..],
                (Some(0), Some(1), false, 2),
            ),
            // Positions are counted after global options and their values.
            (
                &["w1", "--session=x", FLAG, "hi"][..],
                (Some(0), Some(3), true, 4),
            ),
            (
                &[FLAG, "--session=work", "w1:p2", "--remote=host"][..],
                (Some(2), None, true, 4),
            ),
            (
                &[FLAG, "--session=work", "w1:p2", "--remote=host", "hi"][..],
                (Some(2), Some(4), true, 5),
            ),
            (
                &[
                    FLAG,
                    "--session",
                    "work",
                    "w1",
                    "--remote",
                    "host",
                    "hi",
                    "--wait",
                ][..],
                (Some(3), Some(6), true, 7),
            ),
            (
                &["--session=work", FLAG, "--handoff", "w1", "hi"][..],
                (Some(3), Some(4), true, 5),
            ),
            (
                &["w1", FLAG, "--remote-keybindings", "server", "hi"][..],
                (Some(0), Some(4), true, 5),
            ),
            (
                &["w1", FLAG, "--session=x"][..],
                (Some(0), Some(1), false, 2),
            ),
            (
                &["w1", FLAG, "--session", "y"][..],
                (Some(0), Some(1), false, 2),
            ),
        ] {
            assert_eq!(classify(input), expected, "{input:?}");
        }
    }

    #[test]
    fn prompt_literal_slots_are_the_classified_target_and_text() {
        let prompt = |rest: &[&str]| {
            let mut input = args(&["herdr", "--session", "s", "agent", "prompt"]);
            input.extend(args(rest));
            literal_slots(&input)
        };
        let flag = "--allow-cross-pane";
        assert!(prompt(&[flag, "--remote=t", "--session=x"]).is_empty());
        assert_eq!(prompt(&[flag, "w1", "--remote=x", "hi"]), [6, 8]);
        assert_eq!(prompt(&["w1", flag, "--session=x"]), [5, 6]);
        assert_eq!(prompt(&["w1", flag, "hi", "--session", "y"]), [5, 7]);
        assert_eq!(prompt(&["w1", flag, "--session", "y", "hi"]), [5, 9]);
        assert_eq!(prompt(&["w1", "hi", flag]), [5, 6]);
        assert_eq!(prompt(&["w1", flag, "--wait"]), [5, 6]);
        assert_eq!(
            prompt(&[flag, "--session=work", "w1:p2", "--remote=host"]),
            [7]
        );
    }

    /// Session extraction, then remote extraction, then the prompt parser, as
    /// `main` runs them; each classifies the argv it sees.
    fn extract_prompt(rest: &[&str]) -> (Option<String>, Option<String>, Vec<String>, PromptArgv) {
        crate::session::clear_explicit_session_for_test();
        let mut input = args(&["herdr", "agent", "prompt"]);
        input.extend(args(rest));
        let cleaned = crate::session::configure_from_args(&input).unwrap();
        let (cleaned, remote) = crate::remote::extract_remote_args(&cleaned).unwrap();
        let prompt_argv = cleaned[3..].to_vec();
        let argv = classify_prompt_argv(&prompt_argv);
        (
            crate::session::explicit_session_requested()
                .then(|| crate::session::active_name().unwrap_or_default()),
            remote.map(|remote| remote.target),
            prompt_argv,
            argv,
        )
    }

    #[test]
    fn prompt_classification_agrees_across_global_extraction() {
        let env = crate::environment::test_env();
        const FLAG: &str = "--allow-cross-pane";

        // Review trace 1: master behavior, literal flag TEXT and --wait option.
        env.remove(crate::session::SESSION_ENV_VAR);
        let (session, remote, cleaned, argv) = extract_prompt(&["w1", FLAG, "--wait"]);
        assert_eq!((session, remote), (None, None));
        assert_eq!(cleaned, args(&["w1", FLAG, "--wait"]));
        assert_eq!(argv.target.map(|i| cleaned[i].as_str()), Some("w1"));
        assert_eq!(argv.text.map(|i| cleaned[i].as_str()), Some(FLAG));
        assert!(!argv.cross_pane_opt_in);
        assert_eq!(&cleaned[argv.options..], ["--wait"]);

        // Review trace 2: both globals applied, TARGET kept, no TEXT consumed.
        for (rest, text) in [
            (
                &[FLAG, "--session=work", "w1:p2", "--remote=host"][..],
                None,
            ),
            (
                &[FLAG, "--session=work", "w1:p2", "--remote=host", "hi"][..],
                Some("hi"),
            ),
            (
                &[FLAG, "--session", "work", "w1:p2", "--remote", "host", "hi"][..],
                Some("hi"),
            ),
            (
                &["w1:p2", FLAG, "--session=work", "--remote=host", "hi"][..],
                Some("hi"),
            ),
        ] {
            env.remove(crate::session::SESSION_ENV_VAR);
            let (session, remote, cleaned, argv) = extract_prompt(rest);
            assert_eq!(session.as_deref(), Some("work"), "{rest:?}");
            assert_eq!(remote.as_deref(), Some("host"), "{rest:?}");
            let mut expected = args(&[FLAG, "w1:p2"]);
            if rest[0] != FLAG {
                expected.swap(0, 1);
            }
            expected.extend(text.map(str::to_owned));
            assert_eq!(cleaned, expected, "{rest:?}");
            assert_eq!(argv.target.map(|i| cleaned[i].as_str()), Some("w1:p2"));
            assert_eq!(argv.text.map(|i| cleaned[i].as_str()), text, "{rest:?}");
            assert!(argv.cross_pane_opt_in, "{rest:?}");
            assert_eq!(argv.options, cleaned.len(), "{rest:?}");
        }

        // Without an opt-in before TEXT, both slots stay literal as before.
        env.remove(crate::session::SESSION_ENV_VAR);
        let (session, remote, cleaned, argv) =
            extract_prompt(&["--session=t", "--remote=x", "--session=work"]);
        assert_eq!((session.as_deref(), remote), (Some("work"), None));
        assert_eq!(cleaned, args(&["--session=t", "--remote=x"]));
        assert_eq!((argv.target, argv.text), (Some(0), Some(1)));
        assert!(!argv.cross_pane_opt_in);
        env.remove(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
    }

    #[test]
    fn remote_extraction_stops_at_a_literal_prompt_separator() {
        let input = args(&["herdr", "agent", "prompt", "--", "hi", "--remote=host"]);
        assert_eq!(literal_slots(&input), [3, 4]);
        assert_eq!(
            crate::remote::extract_remote_args(&input).unwrap(),
            (input, None)
        );
    }

    fn command_with_mock_server(
        command: &str,
        leaf_args: &[String],
        reply: serde_json::Value,
    ) -> (i32, Vec<serde_json::Value>) {
        command_with_mock_replies(command, leaf_args, vec![reply])
    }

    fn command_with_mock_replies(
        command: &str,
        leaf_args: &[String],
        replies: Vec<serde_json::Value>,
    ) -> (i32, Vec<serde_json::Value>) {
        use crate::api::client::{ApiClient, ConnectionTarget};
        use interprocess::local_socket::traits::Listener as _;
        use std::io::{BufRead, BufReader, Write};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "herdr-cli-channel-{}-{}.sock",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
        ));
        let listener = crate::ipc::bind_private_local_listener(&path).unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for step in 0..replies.len() * 2 {
                let stream = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                let mut response = if step % 2 == 0 {
                    assert_eq!(request["method"], "ping");
                    serde_json::json!({"result": {
                        "type": "pong", "version": "test", "protocol": crate::protocol::PROTOCOL_VERSION,
                        "capabilities": null,
                    }})
                } else {
                    replies[step / 2].clone()
                };
                response["id"] = request["id"].clone();
                writeln!(reader.get_mut(), "{response}").unwrap();
                requests.push(request);
            }
            requests
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let mut command_args = args(&[command]);
        command_args.extend_from_slice(leaf_args);
        let exit_code = crate::cli::target::with_test_client(client, || {
            super::super::run_agent_command(&command_args)
        })
        .unwrap();
        let requests = server.join().unwrap();
        // Named-pipe platforms do not create a filesystem socket.
        let _ = std::fs::remove_file(path);
        (exit_code, requests)
    }

    fn ready_draft_channel() -> serde_json::Value {
        serde_json::json!({"result": {
            "terminal_id": "opaque:λ/42", "registration_epoch": "epoch=日本語",
            "ready": true, "draft_guard": true,
        }})
    }

    #[test]
    fn draft_state_transport_is_read_only_and_target_only() {
        for (leaf, expected) in [
            (
                vec!["--session=undetected-terminal"],
                serde_json::json!({"target": "--session=undetected-terminal"}),
            ),
            (
                vec!["--allow-cross-pane", "--session=undetected-terminal"],
                serde_json::json!({"target": "--session=undetected-terminal", "allow_cross_pane": true}),
            ),
        ] {
            let (code, requests) = command_with_mock_server(
                "draft-state",
                &args(&leaf),
                serde_json::json!({"result": {"status": "known", "empty": true, "hold": null}}),
            );
            assert_eq!(code, 0);
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[1]["method"], "agent.draft_state");
            assert_eq!(requests[1]["params"], expected);
        }
    }

    #[test]
    fn prompt_draft_guard_discovers_pins_and_uses_fresh_single_attempt_requests() {
        let mut previous_id = None;
        for reply in [
            serde_json::json!({"result": {"status": "accepted"}}),
            serde_json::json!({"error": {"code": "delivery_unknown", "message": "receipt lost"}}),
            serde_json::json!({"error": {"code": "method_not_found", "message": "old server"}}),
            serde_json::json!({"error": {"code": "agent_prompt_rejected", "reason": "draft_present", "message": "draft exists"}}),
        ] {
            let expected_code = i32::from(reply.get("error").is_some());
            let (code, requests) = command_with_mock_replies(
                "prompt",
                &args(&[
                    "worker",
                    "--if-draft-empty",
                    "--if-draft-empty",
                    "--allow-cross-pane",
                ]),
                vec![ready_draft_channel(), reply],
            );
            assert_eq!(code, expected_code);
            assert_eq!(requests.len(), 4);
            assert_eq!(requests[1]["method"], "agent.channel_info");
            assert_eq!(
                requests[1]["params"],
                serde_json::json!({"target": "worker"})
            );
            let prompt = &requests[3];
            assert_eq!(prompt["method"], "agent.prompt_guarded");
            let params = &prompt["params"];
            assert_eq!(params["target"], "worker");
            assert_eq!(params["text"], "--if-draft-empty");
            assert_eq!(params["expected_terminal"], "opaque:λ/42");
            assert_eq!(params["expected_registration_epoch"], "epoch=日本語");
            assert_eq!(params["if_draft_empty"], true);
            assert_eq!(params["allow_cross_pane"], true);
            assert!(params.get("wait").is_none());
            assert!(params.get("timeout_ms").is_none());
            let id = params["request_id"].as_str().unwrap().to_owned();
            assert!(!id.is_empty());
            assert_ne!(Some(&id), previous_id.as_ref());
            previous_id = Some(id);
        }
    }

    #[test]
    fn both_draft_guard_commands_refuse_missing_capability_without_sending_prompt() {
        let mut refusals = vec![
            serde_json::json!({"result": {"terminal_id": "term", "ready": false}}),
            serde_json::json!({"result": {"terminal_id": "term", "ready": true, "registration_epoch": "epoch"}}),
            serde_json::json!({"error": {"code": "method_not_found", "message": "old server"}}),
        ];
        for cap in [
            serde_json::Value::Null,
            false.into(),
            "true".into(),
            1.into(),
        ] {
            let mut reply = ready_draft_channel();
            reply["result"]["draft_guard"] = cap;
            refusals.push(reply);
        }
        for field in ["terminal_id", "registration_epoch", "ready"] {
            let mut reply = ready_draft_channel();
            reply["result"].as_object_mut().unwrap().remove(field);
            refusals.push(reply);
        }
        for reply in refusals {
            for command in ["prompt", "prompt-guarded"] {
                let mut input = if command == "prompt" {
                    args(&["worker", "literal"])
                } else {
                    guarded_args("worker", "literal")
                };
                input.push("--if-draft-empty".into());
                let (code, requests) = command_with_mock_server(command, &input, reply.clone());
                assert_eq!(code, 1);
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[1]["method"], "agent.channel_info");
                assert_eq!(
                    requests[1]["params"],
                    serde_json::json!({"target": "worker"})
                );
            }
        }
    }

    #[test]
    fn convenience_draft_guard_defaults_to_no_cross_pane_override() {
        let (code, requests) = command_with_mock_replies(
            "prompt",
            &args(&["worker", "literal", "--if-draft-empty"]),
            vec![
                ready_draft_channel(),
                serde_json::json!({"result": {"status": "accepted"}}),
            ],
        );
        assert_eq!(code, 0);
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[3]["method"], "agent.prompt_guarded");
        assert_eq!(requests[3]["params"]["if_draft_empty"], true);
        assert!(requests[3]["params"].get("allow_cross_pane").is_none());
    }

    #[test]
    fn explicit_draft_guard_preflights_capability_and_preserves_caller_pins() {
        let mut input = guarded_args("worker", "literal");
        input.push("--if-draft-empty".into());
        let (code, requests) = command_with_mock_replies(
            "prompt-guarded",
            &input,
            vec![
                ready_draft_channel(),
                serde_json::json!({"result": {"status": "queued"}}),
            ],
        );
        assert_eq!(code, 0);
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[1]["method"], "agent.channel_info");
        assert_eq!(requests[3]["method"], "agent.prompt_guarded");
        assert_eq!(
            requests[3]["params"],
            serde_json::json!({
                "target": "worker", "text": "literal", "expected_terminal": "opaque:λ/42",
                "expected_registration_epoch": "epoch=日本語", "request_id": "caller:key", "if_draft_empty": true,
            })
        );
    }

    #[test]
    fn prompt_draft_guard_refuses_legacy_options_before_any_rpc() {
        for options in [
            &["--wait"][..],
            &["--until", "idle"][..],
            &["--timeout", "1000"][..],
            &["--wait", "--until", "done", "--timeout", "1000"][..],
            &["--if-draft-empty"][..],
            &["extra"][..],
        ] {
            let mut input = args(&["worker", "literal", "--if-draft-empty"]);
            input.extend(args(options));
            // No client/server is installed: any accidental RPC would fail this test.
            assert_eq!(super::super::agent_prompt(&input).unwrap(), 2);
        }
        for input in [
            &["", "literal", "--if-draft-empty"][..],
            &["worker", "", "--if-draft-empty"][..],
        ] {
            assert_eq!(super::super::agent_prompt(&args(input)).unwrap(), 2);
        }
    }

    #[test]
    fn flag_shaped_prompt_text_alone_keeps_legacy_route() {
        let (code, requests) = command_with_mock_server(
            "prompt",
            &args(&["worker", "--if-draft-empty"]),
            serde_json::json!({"result": {"status": "sent"}}),
        );
        assert_eq!(code, 0);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1]["method"], "agent.prompt");
        assert_eq!(requests[1]["params"]["text"], "--if-draft-empty");
    }

    #[test]
    fn channel_info_transport_queries_undetected_terminal_directly() {
        let (code, requests) = command_with_mock_server(
            "channel-info",
            &args(&["undetected-terminal"]),
            serde_json::json!({
                "result": {"terminal_id": "undetected-terminal", "ready": false},
            }),
        );
        assert_eq!(code, 0);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1]["method"], "agent.channel_info");
        assert_eq!(
            requests[1]["params"],
            serde_json::json!({"target": "undetected-terminal"})
        );
    }

    #[test]
    fn guarded_transport_carries_exact_pins_and_never_falls_back_or_retries() {
        let mut input = guarded_args("worker", "--session=literal\n$(echo no-shell)");
        input.extend(args(&["--timeout-ms", "1234", "--allow-cross-pane"]));
        for error in [
            "delivery_unknown",
            "method_not_found",
            "agent_prompt_rejected",
        ] {
            let (code, requests) = command_with_mock_server(
                "prompt-guarded",
                &input,
                serde_json::json!({
                    "error": {"code": error, "message": "not acknowledged"},
                }),
            );
            assert_eq!(code, 1);
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[1]["method"], "agent.prompt_guarded");
            assert_eq!(
                requests[1]["params"],
                serde_json::json!({
                    "target": "worker", "text": "--session=literal\n$(echo no-shell)",
                    "expected_terminal": "opaque:λ/42", "expected_registration_epoch": "epoch=日本語",
                    "request_id": "caller:key", "timeout_ms": 1234, "allow_cross_pane": true,
                })
            );
        }
    }

    #[test]
    fn channel_requests_serialize_as_distinct_flat_protocol_methods() {
        let params =
            parse_prompt_guarded_args(&guarded_args("worker", "--session=literal")).unwrap();
        let request = Request {
            id: "cli:agent:prompt-guarded".into(),
            method: Method::AgentPromptGuarded(params),
        };
        let wire = serde_json::to_value(request).unwrap();
        assert_eq!(wire["method"], "agent.prompt_guarded");
        assert_eq!(wire["params"]["target"], "worker");
        assert_eq!(wire["params"]["text"], "--session=literal");
        assert_eq!(wire["params"]["expected_terminal"], "opaque:λ/42");
        assert_eq!(
            wire["params"]["expected_registration_epoch"],
            "epoch=日本語"
        );
        assert_eq!(wire["params"]["request_id"], "caller:key");
        assert!(wire["params"].get("prompt").is_none());
        let request = Request {
            id: "cli:agent:channel-info".into(),
            method: Method::AgentChannelInfo(
                parse_channel_info_args(&args(&["undetected-terminal"])).unwrap(),
            ),
        };
        let wire = serde_json::to_value(request).unwrap();
        assert_eq!(wire["method"], "agent.channel_info");
        assert_eq!(
            wire["params"],
            serde_json::json!({"target": "undetected-terminal"})
        );
    }
}
