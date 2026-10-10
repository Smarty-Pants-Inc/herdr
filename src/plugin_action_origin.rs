//! Server-local plugin action input grants (herdr-8592).
//!
//! A user-invoked plugin action (click, keybinding, `plugin action invoke`) gets
//! a random 256-bit bearer token in `HERDR_PLUGIN_ACTION_TOKEN`. The herdr CLI
//! forwards it with content writes, so the action (and the `herdr pane run`
//! subprocesses it forks) is attributed as user-initiated plugin input.
//!
//! The grant lives only in the minting server's memory, is bound to the plugin,
//! action and invoking pane, and is valid only while the recorded action child
//! is alive and for at most [`MAX_GRANT_AGE`]. The registry stores a SHA-256 of
//! the token, never the token, and no Debug/log/error path prints it.
//!
//! This is not same-UID authentication: any same-user process that can read the
//! child's environment can copy the token (existing #8151 class).

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

pub(crate) const TOKEN_ENV_VAR: &str = "HERDR_PLUGIN_ACTION_TOKEN";
/// Optional top-level request-line field; never part of `Method`.
pub(crate) const TOKEN_FIELD: &str = "plugin_action_token";
pub(crate) const MAX_GRANT_AGE: Duration = Duration::from_secs(10 * 60);
/// Bound on how long a request presented between spawn and pin may wait.
const ACTIVATION_WAIT: Duration = Duration::from_secs(5);
const TOKEN_BYTES: usize = 32;

/// A presented or minted secret. Copy, fixed-size, and redacted in Debug.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PluginActionToken([u8; TOKEN_BYTES]);

impl std::fmt::Debug for PluginActionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginActionToken(<redacted>)")
    }
}

impl PluginActionToken {
    fn mint() -> std::io::Result<Self> {
        let mut bytes = [0; TOKEN_BYTES];
        crate::platform::secure_random(&mut bytes)?;
        Ok(Self(bytes))
    }

    /// Exactly 64 hex digits; anything else is malformed.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let value = value.as_bytes();
        if value.len() != TOKEN_BYTES * 2 {
            return None;
        }
        let digit = |byte: u8| char::from(byte).to_digit(16).map(|digit| digit as u8);
        let mut bytes = [0; TOKEN_BYTES];
        for (index, pair) in value.chunks_exact(2).enumerate() {
            bytes[index] = digit(pair[0])? << 4 | digit(pair[1])?;
        }
        Some(Self(bytes))
    }

    /// Environment/wire form. Only the spawn path and the CLI send it.
    pub(crate) fn expose_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn key(&self) -> GrantKey {
        GrantKey(Sha256::digest(self.0).into())
    }
}

/// Content writes the input guard attributes; only these carry a grant.
pub(crate) fn method_carries_grant(method: &crate::api::schema::Method) -> bool {
    use crate::api::schema::Method;
    match method {
        Method::AgentStart(_)
        | Method::AgentStartGuarded(_)
        | Method::AgentPrompt(_)
        | Method::AgentSendKeys(_)
        | Method::PaneSendText(_)
        | Method::PaneSendKeys(_)
        | Method::PaneSendInput(_)
        | Method::PaneSendInputGuarded(_) => true,
        Method::PaneReportAgent(params) => params.resume_argv.is_some(),
        Method::PaneReportAgentSession(params) => params.resume_argv.is_some(),
        _ => false,
    }
}

/// This CLI process's claim, from the environment its action child received.
/// Missing is `Absent`; a present but blank, non-Unicode or malformed value is
/// `Malformed` and is still forwarded, so the server refuses it instead of
/// treating the caller as ordinary.
pub(crate) fn claim_from_env() -> PluginActionClaim {
    claim_from_env_value(std::env::var_os(TOKEN_ENV_VAR))
}

fn claim_from_env_value(value: Option<std::ffi::OsString>) -> PluginActionClaim {
    match value {
        None => PluginActionClaim::Absent,
        Some(value) => value
            .to_str()
            .and_then(PluginActionToken::parse)
            .map_or(PluginActionClaim::Malformed, PluginActionClaim::Presented),
    }
}

/// What a request carried, captured at the transport edge.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PluginActionClaim {
    #[default]
    Absent,
    /// The field was present but not a well-formed token.
    Malformed,
    Presented(PluginActionToken),
}

impl std::fmt::Debug for PluginActionClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Absent => f.write_str("Absent"),
            Self::Malformed => f.write_str("Malformed"),
            Self::Presented(_) => f.write_str("Presented(<redacted>)"),
        }
    }
}

impl PluginActionClaim {
    /// Reads only the optional top-level field of one request line. Only a
    /// missing field is `Absent`; null, empty, non-string or malformed values
    /// are `Malformed`, as is an unparsable line.
    pub(crate) fn from_request_line(line: &str) -> Self {
        let Ok(serde_json::Value::Object(object)) = serde_json::from_str(line) else {
            return Self::Malformed;
        };
        match object.get(TOKEN_FIELD) {
            None => Self::Absent,
            Some(serde_json::Value::String(value)) => {
                PluginActionToken::parse(value).map_or(Self::Malformed, Self::Presented)
            }
            Some(_) => Self::Malformed,
        }
    }

    /// Wire value the CLI forwards: the hex token, or an empty (malformed)
    /// value so a supplied-but-invalid env claim is refused server-side.
    pub(crate) fn wire_value(&self) -> Option<String> {
        match self {
            Self::Absent => None,
            Self::Malformed => Some(String::new()),
            Self::Presented(token) => Some(token.expose_hex()),
        }
    }
}

/// Non-secret attribution of a valid grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginActionGrant {
    pub(crate) plugin_id: String,
    pub(crate) action_id: String,
    /// Public pane ID where the action was invoked, if any.
    pub(crate) invoking_pane: Option<String>,
    /// Non-secret grant id: the plugin command log id.
    pub(crate) grant_id: String,
}

/// SHA-256 of a token: the registry key, and the worker's handle for pinning
/// and revocation. Not a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct GrantKey([u8; 32]);

enum GrantState {
    /// Minted before spawn; the child is not pinned yet.
    Pending,
    Live(crate::platform::OwnedChildPin),
    /// Child exited, pin failed, or expired. Never valid again; the entry
    /// (without its pin) remains only so an already admitted write keeps its
    /// attribution in the durable input log.
    Revoked,
}

/// Revoked entries are dropped once no admitted write can still need them.
const ATTRIBUTION_RETENTION: Duration = Duration::from_secs(2 * 10 * 60);

struct Entry {
    grant: PluginActionGrant,
    minted: Instant,
    state: GrantState,
}

/// Per-App (therefore per-server) registry.
#[derive(Clone, Default)]
pub(crate) struct PluginActionGrants {
    inner: Arc<(Mutex<HashMap<GrantKey, Entry>>, Condvar)>,
}

impl std::fmt::Debug for PluginActionGrants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginActionGrants")
    }
}

impl PluginActionGrants {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<GrantKey, Entry>> {
        self.inner
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Mint a pending grant before the child exists, so a request the child
    /// sends immediately after exec can never precede registration.
    pub(crate) fn mint(
        &self,
        grant: PluginActionGrant,
    ) -> std::io::Result<(PluginActionToken, GrantKey)> {
        let token = PluginActionToken::mint()?;
        let key = token.key();
        let mut entries = self.lock();
        let now = Instant::now();
        entries.retain(|_, entry| now.duration_since(entry.minted) <= ATTRIBUTION_RETENTION);
        entries.insert(
            key,
            Entry {
                grant,
                minted: now,
                state: GrantState::Pending,
            },
        );
        Ok((token, key))
    }

    /// Bind the grant to the exact spawned child. False if already revoked.
    pub(crate) fn activate(&self, key: GrantKey, pin: crate::platform::OwnedChildPin) -> bool {
        let activated = match self.lock().get_mut(&key) {
            Some(entry) if matches!(entry.state, GrantState::Pending) => {
                entry.state = GrantState::Live(pin);
                true
            }
            _ => false,
        };
        self.inner.1.notify_all();
        activated
    }

    /// Immediate, idempotent revocation (child exit, spawn or pin failure).
    pub(crate) fn revoke(&self, key: GrantKey) {
        if let Some(entry) = self.lock().get_mut(&key) {
            entry.state = GrantState::Revoked;
        }
        self.inner.1.notify_all();
    }

    /// Non-secret attribution for a presented token that this server minted,
    /// whether or not it is still valid. Never authorization: the guard
    /// decided admission with [`Self::resolve`]; the input log uses this so a
    /// child exiting between admission and logging cannot erase attribution.
    pub(crate) fn attribution(&self, claim: PluginActionClaim) -> Option<PluginActionGrant> {
        let PluginActionClaim::Presented(token) = claim else {
            return None;
        };
        self.lock()
            .get(&token.key())
            .map(|entry| entry.grant.clone())
    }

    /// `Ok(None)`: no claim. `Ok(Some)`: valid grant. `Err(())`: a claim was
    /// made but is malformed, unknown here, expired, or revoked.
    pub(crate) fn resolve(
        &self,
        claim: PluginActionClaim,
    ) -> Result<Option<PluginActionGrant>, ()> {
        let token = match claim {
            PluginActionClaim::Absent => return Ok(None),
            PluginActionClaim::Malformed => return Err(()),
            PluginActionClaim::Presented(token) => token,
        };
        let key = token.key();
        let deadline = Instant::now() + ACTIVATION_WAIT;
        let mut entries = self.lock();
        loop {
            let entry = entries.get_mut(&key).ok_or(())?;
            if entry.minted.elapsed() > MAX_GRANT_AGE {
                entry.state = GrantState::Revoked;
                return Err(());
            }
            match &entry.state {
                GrantState::Live(pin) if pin.is_alive() => return Ok(Some(entry.grant.clone())),
                GrantState::Live(_) | GrantState::Revoked => {
                    entry.state = GrantState::Revoked;
                    return Err(());
                }
                GrantState::Pending => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(());
                    }
                    entries = self
                        .inner
                        .1
                        .wait_timeout(entries, deadline - now)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
            }
        }
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn backdate_for_test(&self, key: GrantKey, age: Duration) {
        if let Some(entry) = self.lock().get_mut(&key) {
            entry.minted = Instant::now()
                .checked_sub(age)
                .expect("test clock backdate");
        }
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn is_revoked_for_test(&self, key: GrantKey) -> bool {
        self.lock()
            .get(&key)
            .is_some_and(|entry| matches!(entry.state, GrantState::Revoked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant() -> PluginActionGrant {
        PluginActionGrant {
            plugin_id: "explorr".into(),
            action_id: "open".into(),
            invoking_pane: Some("w1:p1".into()),
            grant_id: "plugin-log-1".into(),
        }
    }

    #[test]
    fn token_never_appears_in_debug_and_round_trips_through_hex() {
        let grants = PluginActionGrants::default();
        let (token, key) = grants.mint(grant()).expect("mint");
        let hex = token.expose_hex();
        assert_eq!(hex.len(), 64);
        assert_eq!(PluginActionToken::parse(&hex), Some(token));
        for debug in [
            format!("{token:?}"),
            format!("{:?}", PluginActionClaim::Presented(token)),
            format!("{grants:?}"),
            format!("{key:?}"),
        ] {
            assert!(!debug.contains(&hex), "{debug}");
            assert!(!debug.contains(&hex[..16]), "{debug}");
        }
        let (other, _) = grants.mint(grant()).expect("second mint");
        assert_ne!(other.expose_hex(), hex, "independent random tokens");
    }

    #[test]
    fn request_line_claims_are_absent_malformed_or_presented() {
        let token = PluginActionToken([7; 32]);
        let line = |field: &str| format!(r#"{{"id":"x","method":"ping","params":{{}}{field}}}"#);
        assert_eq!(
            PluginActionClaim::from_request_line(&line("")),
            PluginActionClaim::Absent
        );
        assert_eq!(
            PluginActionClaim::from_request_line(&line(&format!(
                r#","plugin_action_token":"{}""#,
                token.expose_hex()
            ))),
            PluginActionClaim::Presented(token)
        );
        for bad in [
            r#","plugin_action_token":"""#,
            r#","plugin_action_token":"zz""#,
            r#","plugin_action_token":7"#,
            r#","plugin_action_token":null"#,
        ] {
            assert_eq!(
                PluginActionClaim::from_request_line(&line(bad)),
                PluginActionClaim::Malformed,
                "{bad}"
            );
        }
        let long = format!(r#","plugin_action_token":"{}0""#, token.expose_hex());
        assert_eq!(
            PluginActionClaim::from_request_line(&line(&long)),
            PluginActionClaim::Malformed
        );
    }

    #[test]
    fn env_claim_distinguishes_missing_from_supplied_invalid() {
        use std::ffi::OsString;
        let hex = "Cd".repeat(32);
        assert_eq!(claim_from_env_value(None), PluginActionClaim::Absent);
        assert_eq!(
            claim_from_env_value(Some(OsString::from(&hex))),
            PluginActionClaim::Presented(PluginActionToken::parse(&hex).unwrap())
        );
        for bad in ["", " ", "forged", &format!(" {hex}"), &hex[..63]] {
            assert_eq!(
                claim_from_env_value(Some(OsString::from(bad))),
                PluginActionClaim::Malformed,
                "{bad:?}"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert_eq!(
                claim_from_env_value(Some(OsString::from_vec(vec![0xff; 64]))),
                PluginActionClaim::Malformed
            );
        }
        assert_eq!(PluginActionClaim::Absent.wire_value(), None);
        assert_eq!(
            PluginActionClaim::Malformed.wire_value(),
            Some(String::new())
        );
        assert_eq!(
            PluginActionClaim::from_request_line(
                r#"{"id":"x","method":"ping","params":{},"plugin_action_token":""}"#
            ),
            PluginActionClaim::Malformed
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn grant_is_valid_only_while_pinned_child_lives_and_within_max_age() {
        let grants = PluginActionGrants::default();
        let (token, key) = grants.mint(grant()).expect("mint");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("child");
        let pin = crate::platform::pin_owned_child(&child).expect("pin");
        assert_eq!(pin.identity().pid, child.id());
        assert!(grants.activate(key, pin));
        let claim = PluginActionClaim::Presented(token);
        assert_eq!(grants.resolve(claim), Ok(Some(grant())));

        // A different server's registry never knows this token.
        assert_eq!(PluginActionGrants::default().resolve(claim), Err(()));
        // Forged token of the right shape.
        assert_eq!(
            grants.resolve(PluginActionClaim::Presented(PluginActionToken([1; 32]))),
            Err(())
        );
        assert_eq!(grants.resolve(PluginActionClaim::Malformed), Err(()));
        assert_eq!(grants.resolve(PluginActionClaim::Absent), Ok(None));

        // Expiry is enforced even while the child lives.
        grants.backdate_for_test(key, MAX_GRANT_AGE + Duration::from_secs(1));
        assert_eq!(grants.resolve(claim), Err(()));
        assert!(grants.is_revoked_for_test(key), "expired grant revoked");

        // Child exit invalidates even before the owner reaps or revokes.
        let (token, key) = grants.mint(grant()).expect("mint again");
        assert!(grants.activate(key, crate::platform::pin_owned_child(&child).expect("pin")));
        let claim = PluginActionClaim::Presented(token);
        assert_eq!(grants.resolve(claim), Ok(Some(grant())));
        child.kill().expect("kill own child");
        let deadline = Instant::now() + Duration::from_secs(5);
        while grants.resolve(claim).is_ok() {
            assert!(Instant::now() < deadline, "exit not observed before reap");
            std::thread::sleep(Duration::from_millis(5));
        }
        child.wait().expect("reap");
        assert_eq!(grants.resolve(claim), Err(()), "replay after exit denied");
    }

    #[test]
    fn pending_grant_waits_for_activation_and_revocation_denies() {
        let grants = PluginActionGrants::default();
        let (token, key) = grants.mint(grant()).expect("mint");
        let claim = PluginActionClaim::Presented(token);
        let worker = {
            let grants = grants.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                grants.revoke(key);
            })
        };
        // Blocks on the pending grant, then observes the revocation.
        assert_eq!(grants.resolve(claim), Err(()));
        worker.join().expect("worker");
        assert_eq!(grants.resolve(claim), Err(()));
        assert!(
            !grants.activate(key, own_pin()),
            "revoked grant stays revoked"
        );
    }

    /// Spawn/bootstrap race: a request presented before the worker pins the
    /// child waits for the pin instead of failing or being trusted unpinned.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn request_before_pin_waits_for_activation() {
        let grants = PluginActionGrants::default();
        let (token, key) = grants.mint(grant()).expect("mint");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("child");
        let pin = crate::platform::pin_owned_child(&child).expect("pin");
        let worker = {
            let grants = grants.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                assert!(grants.activate(key, pin));
            })
        };
        let started = Instant::now();
        assert_eq!(
            grants.resolve(PluginActionClaim::Presented(token)),
            Ok(Some(grant()))
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
        worker.join().expect("worker");
        let _ = child.kill();
        let _ = child.wait();
    }

    fn own_pin() -> crate::platform::OwnedChildPin {
        #[cfg(unix)]
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("child");
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 5 127.0.0.1 >NUL"])
            .spawn()
            .expect("child");
        let pin = crate::platform::pin_owned_child(&child).expect("pin");
        let _ = child.kill();
        let _ = child.wait();
        pin
    }
}
