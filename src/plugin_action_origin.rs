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

use std::collections::{HashMap, HashSet};
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
/// Hard cap on TOTAL registry entries: pending, live and revoked tombstones.
pub(crate) const MAX_ENTRIES: usize = 256;
/// Hard cap on stable terminal IDs one grant may target.
pub(crate) const MAX_TARGETS_PER_GRANT: usize = 256;

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
        // Session-checked aliases are the same content writes with an identity
        // expectation, so they carry the claim exactly like their base methods.
        Method::AgentPromptSessionChecked(_)
        | Method::AgentPromptStatusChecked(_)
        | Method::PaneSendTextSessionChecked(_)
        | Method::PaneSendKeysSessionChecked(_) => true,
        // Pane creation the CLI performs, so the server can attribute the
        // created terminals to the grant (herdr#223). Carrying a claim never
        // awards rights by itself; the server registers only actually
        // created stable terminal IDs for a valid grant.
        Method::PaneSplit(_)
        | Method::TabCreate(_)
        | Method::WorkspaceCreate(_)
        | Method::WorkspaceCreateLinked(_) => true,
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
    /// Stable terminal IDs (never public pane IDs) this grant may target.
    /// Starts empty: the runtime binds the trusted invoking terminal and the
    /// server adds terminals the grant actually created. Nothing is derived
    /// from `grant.invoking_pane`.
    allowed_terminal_ids: HashSet<String>,
}

impl Entry {
    fn is_expired(&self) -> bool {
        self.minted.elapsed() > MAX_GRANT_AGE
    }

    /// Lazily revoke an expired or dead grant; true if it is now Revoked.
    fn revoke_if_invalid(&mut self) -> bool {
        let invalid = match &self.state {
            GrantState::Revoked => true,
            GrantState::Live(pin) => !pin.is_alive() || self.is_expired(),
            GrantState::Pending => self.is_expired(),
        };
        if invalid {
            self.state = GrantState::Revoked;
        }
        invalid
    }

    fn allow_target(&mut self, target: &str) -> bool {
        if target.is_empty() {
            return false;
        }
        if self.allowed_terminal_ids.contains(target) {
            return true;
        }
        if self.allowed_terminal_ids.len() >= MAX_TARGETS_PER_GRANT {
            return false;
        }
        self.allowed_terminal_ids.insert(target.to_owned())
    }
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
    ///
    /// The registry holds at most [`MAX_ENTRIES`] entries in total. Before
    /// inserting, entries past attribution retention are dropped, expired or
    /// dead grants become tombstones, and, only when still full, the oldest
    /// tombstones are evicted (their attribution is then lost). If pending and
    /// live grants alone fill the cap this fails closed with an error that
    /// carries no secret; the runtime refuses to launch the user action.
    pub(crate) fn mint(
        &self,
        grant: PluginActionGrant,
    ) -> std::io::Result<(PluginActionToken, GrantKey)> {
        let token = PluginActionToken::mint()?;
        let key = token.key();
        let mut entries = self.lock();
        let now = Instant::now();
        entries.retain(|_, entry| now.duration_since(entry.minted) <= ATTRIBUTION_RETENTION);
        for entry in entries.values_mut() {
            entry.revoke_if_invalid();
        }
        while entries.len() >= MAX_ENTRIES {
            let oldest_tombstone = entries
                .iter()
                .filter(|(_, entry)| matches!(entry.state, GrantState::Revoked))
                .min_by_key(|(_, entry)| entry.minted)
                .map(|(key, _)| *key);
            match oldest_tombstone {
                Some(evict) => {
                    entries.remove(&evict);
                }
                None => {
                    drop(entries);
                    // Lazy revocations above may wake resolvers.
                    self.inner.1.notify_all();
                    return Err(std::io::Error::other(
                        "plugin action grant registry is full",
                    ));
                }
            }
        }
        entries.insert(
            key,
            Entry {
                grant,
                minted: now,
                state: GrantState::Pending,
                allowed_terminal_ids: HashSet::new(),
            },
        );
        drop(entries);
        self.inner.1.notify_all();
        Ok((token, key))
    }

    /// Record the trusted initial terminal (stable terminal ID, never a public
    /// pane ID) before the child spawns. Pending or live grants only; false
    /// for unknown, revoked, expired or dead grants and for an empty target.
    pub(crate) fn bind_target(&self, key: GrantKey, target: &str) -> bool {
        let bound = match self.lock().get_mut(&key) {
            Some(entry) => !entry.revoke_if_invalid() && entry.allow_target(target),
            None => false,
        };
        self.inner.1.notify_all();
        bound
    }

    /// Add a stable terminal ID the server actually created for a request that
    /// carried this claim. Only a currently valid grant gains the target;
    /// absent, malformed, unknown, expired or revoked claims change nothing.
    pub(crate) fn register_created_target(&self, claim: PluginActionClaim, target: &str) -> bool {
        matches!(
            self.with_valid_entry(claim, |entry| entry.allow_target(target)),
            Ok(Some(true))
        )
    }

    /// Validate the grant (pin and lifetime) like [`Self::resolve`], then
    /// check `target` against its allowed set. `Ok(false)`: valid grant, target
    /// not allowed (also for `Absent`, which carries no grant authority).
    /// `Err(())`: the claim is invalid. Never an Ordinary fallback.
    pub(crate) fn permits_target(
        &self,
        claim: PluginActionClaim,
        target: &str,
    ) -> Result<bool, ()> {
        self.with_valid_entry(claim, |entry| entry.allowed_terminal_ids.contains(target))
            .map(|allowed| allowed.unwrap_or(false))
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
        self.with_valid_entry(claim, |entry| entry.grant.clone())
    }

    /// Shared validation: `Ok(None)` for no claim, `Ok(Some(f(entry)))` for a
    /// valid grant (waiting boundedly while it is pending), `Err(())` otherwise.
    fn with_valid_entry<R>(
        &self,
        claim: PluginActionClaim,
        f: impl FnOnce(&mut Entry) -> R,
    ) -> Result<Option<R>, ()> {
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
                GrantState::Live(pin) if pin.is_alive() => return Ok(Some(f(entry))),
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

    fn live_child() -> std::process::Child {
        #[cfg(unix)]
        let child = std::process::Command::new("sleep").arg("30").spawn();
        #[cfg(windows)]
        let child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
            .spawn();
        child.expect("child")
    }

    fn reap(mut child: std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn creation_methods_carry_the_claim_and_reads_do_not() {
        use crate::api::schema::Method;
        let split: Method = serde_json::from_value(serde_json::json!({
            "method": "pane.split",
            "params": {"target_pane_id": "w1:p1", "direction": "right"}
        }))
        .expect("pane.split params");
        assert!(matches!(split, Method::PaneSplit(_)));
        assert!(method_carries_grant(&split));
        for name in ["tab.create", "workspace.create", "workspace.create_linked"] {
            let method: Method = serde_json::from_value(serde_json::json!({
                "method": name,
                "params": {}
            }))
            .expect(name);
            assert!(method_carries_grant(&method), "{name}");
        }
        for (name, params) in [
            (
                "agent.prompt_session_checked",
                serde_json::json!({"target": "a", "text": "t", "expected_agent_session_id": "s"}),
            ),
            (
                "agent.prompt_status_checked",
                serde_json::json!({"target": "a", "text": "t", "expected_agent_session_id": "s", "expected_agent_status": "idle"}),
            ),
            (
                "pane.send_text_session_checked",
                serde_json::json!({"pane_id": "w1:p1", "text": "t", "expected_agent_session_id": "s"}),
            ),
            (
                "pane.send_keys_session_checked",
                serde_json::json!({"pane_id": "w1:p1", "keys": ["enter"], "expected_agent_session_id": "s"}),
            ),
        ] {
            let method: Method =
                serde_json::from_value(serde_json::json!({"method": name, "params": params}))
                    .expect(name);
            assert!(method_carries_grant(&method), "{name}");
        }
        let list: Method = serde_json::from_value(serde_json::json!({
            "method": "workspace.list",
            "params": {}
        }))
        .expect("workspace.list");
        assert!(!method_carries_grant(&list));
    }

    #[test]
    fn plugin_action_origin_targets_start_empty_and_follow_bindings() {
        let grants = PluginActionGrants::default();
        let (token, key) = grants.mint(grant()).expect("mint");
        let claim = PluginActionClaim::Presented(token);
        let child = live_child();
        // Pending: binding the trusted initial terminal is allowed pre-spawn.
        assert!(grants.bind_target(key, "term-A"));
        assert!(!grants.bind_target(key, ""), "empty target never binds");
        assert!(grants.activate(key, crate::platform::pin_owned_child(&child).expect("pin")));
        assert_eq!(grants.permits_target(claim, "term-A"), Ok(true));
        assert_eq!(grants.permits_target(claim, "term-B"), Ok(false));
        // The public invoking pane string is never authority.
        assert_eq!(grants.permits_target(claim, "w1:p1"), Ok(false));
        assert!(grants.register_created_target(claim, "term-C"));
        assert_eq!(grants.permits_target(claim, "term-C"), Ok(true));
        assert_eq!(grants.permits_target(claim, "term-B"), Ok(false));
        // Absent carries no grant authority; malformed/forged/foreign is Err.
        assert_eq!(
            grants.permits_target(PluginActionClaim::Absent, "term-A"),
            Ok(false)
        );
        assert_eq!(
            grants.permits_target(PluginActionClaim::Malformed, "term-A"),
            Err(())
        );
        let forged = PluginActionClaim::Presented(PluginActionToken([3; 32]));
        assert_eq!(grants.permits_target(forged, "term-A"), Err(()));
        let other_server = PluginActionGrants::default();
        assert_eq!(other_server.permits_target(claim, "term-A"), Err(()));
        assert!(!other_server.register_created_target(claim, "term-D"));
        assert!(!grants.register_created_target(PluginActionClaim::Absent, "term-D"));
        assert!(!grants.register_created_target(PluginActionClaim::Malformed, "term-D"));
        assert!(!grants.register_created_target(forged, "term-D"));
        assert_eq!(grants.permits_target(claim, "term-D"), Ok(false));
        // Resolve is still attribution only.
        assert_eq!(grants.resolve(claim), Ok(Some(grant())));

        // Revoked: no registration, no binding, every target check is Err.
        grants.revoke(key);
        assert!(!grants.register_created_target(claim, "term-E"));
        assert!(!grants.bind_target(key, "term-E"));
        assert_eq!(grants.permits_target(claim, "term-A"), Err(()));
        assert_eq!(grants.attribution(claim), Some(grant()));
        reap(child);
    }

    #[test]
    fn plugin_action_origin_global_grant_targets_only_created_terminals() {
        let grants = PluginActionGrants::default();
        let (token, key) = grants.mint(grant()).expect("mint");
        let claim = PluginActionClaim::Presented(token);
        let child = live_child();
        assert!(grants.activate(key, crate::platform::pin_owned_child(&child).expect("pin")));
        assert_eq!(grants.permits_target(claim, "term-A"), Ok(false));
        assert!(grants.register_created_target(claim, "term-C"));
        assert_eq!(grants.permits_target(claim, "term-C"), Ok(true));
        assert_eq!(grants.permits_target(claim, "term-A"), Ok(false));
        reap(child);
    }

    #[test]
    fn plugin_action_origin_dead_or_pending_grant_cannot_register() {
        let grants = PluginActionGrants::default();
        let (token, key) = grants.mint(grant()).expect("mint");
        let claim = PluginActionClaim::Presented(token);
        // Dead pinned child: invalid even before the owner revokes.
        assert!(grants.activate(key, own_pin()));
        assert!(!grants.register_created_target(claim, "term-C"));
        assert!(!grants.bind_target(key, "term-C"));
        assert_eq!(grants.permits_target(claim, "term-C"), Err(()));
        // Pending that is revoked while a registration waits.
        let (token, key) = grants.mint(grant()).expect("mint");
        let worker = {
            let grants = grants.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                grants.revoke(key);
            })
        };
        assert!(!grants.register_created_target(PluginActionClaim::Presented(token), "term-C"));
        worker.join().expect("worker");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn plugin_action_origin_expired_grant_cannot_bind_or_register() {
        let grants = PluginActionGrants::default();
        let (token, key) = grants.mint(grant()).expect("mint");
        let claim = PluginActionClaim::Presented(token);
        let child = live_child();
        assert!(grants.activate(key, crate::platform::pin_owned_child(&child).expect("pin")));
        assert!(grants.bind_target(key, "term-A"));
        grants.backdate_for_test(key, MAX_GRANT_AGE + Duration::from_secs(1));
        assert!(!grants.bind_target(key, "term-B"));
        assert!(!grants.register_created_target(claim, "term-C"));
        assert_eq!(grants.permits_target(claim, "term-A"), Err(()));
        assert!(grants.is_revoked_for_test(key));
        reap(child);
    }

    #[test]
    fn plugin_action_origin_per_grant_targets_are_capped() {
        let grants = PluginActionGrants::default();
        let (_, key) = grants.mint(grant()).expect("mint");
        for index in 0..MAX_TARGETS_PER_GRANT {
            assert!(grants.bind_target(key, &format!("term-{index}")));
        }
        assert!(
            grants.bind_target(key, "term-0"),
            "existing target idempotent"
        );
        assert!(!grants.bind_target(key, "term-over"));
    }

    #[test]
    fn plugin_action_origin_cap_refuses_when_pending_and_live_fill_it() {
        let grants = PluginActionGrants::default();
        for _ in 0..MAX_ENTRIES {
            grants.mint(grant()).expect("mint under cap");
        }
        let error = grants
            .mint(grant())
            .expect_err("full registry fails closed");
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(error.to_string(), "plugin action grant registry is full");
        assert_eq!(grants.lock().len(), MAX_ENTRIES);
    }

    #[test]
    fn plugin_action_origin_tombstones_count_and_evict_oldest_first() {
        let grants = PluginActionGrants::default();
        let mut tombstones = Vec::new();
        for _ in 0..MAX_ENTRIES {
            let (token, key) = grants.mint(grant()).expect("mint");
            grants.revoke(key);
            tombstones.push(PluginActionClaim::Presented(token));
        }
        // Under normal load tombstones keep attribution.
        assert_eq!(grants.lock().len(), MAX_ENTRIES);
        assert!(tombstones
            .iter()
            .all(|claim| grants.attribution(*claim).is_some()));
        // At the cap a mint evicts exactly the oldest tombstone.
        let (fresh, _) = grants.mint(grant()).expect("evicts a tombstone");
        assert_eq!(grants.lock().len(), MAX_ENTRIES);
        assert_eq!(grants.attribution(tombstones[0]), None);
        assert!(tombstones[1..]
            .iter()
            .all(|claim| grants.attribution(*claim).is_some()));
        assert_eq!(
            grants.attribution(PluginActionClaim::Presented(fresh)),
            Some(grant())
        );
    }

    #[test]
    fn plugin_action_origin_dead_live_is_pruned_before_refusing() {
        let grants = PluginActionGrants::default();
        let (_, dead) = grants.mint(grant()).expect("mint");
        assert!(grants.activate(dead, own_pin()));
        for _ in 1..MAX_ENTRIES {
            grants.mint(grant()).expect("mint");
        }
        // The dead live grant becomes a tombstone and is evicted.
        grants.mint(grant()).expect("dead grant pruned first");
        assert!(!grants.lock().contains_key(&dead));
        assert!(grants.mint(grant()).is_err(), "then pending fills it");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn plugin_action_origin_expired_and_retained_out_entries_are_pruned_first() {
        let grants = PluginActionGrants::default();
        let (_, expired) = grants.mint(grant()).expect("mint");
        let (_, stale) = grants.mint(grant()).expect("mint");
        for _ in 2..MAX_ENTRIES {
            grants.mint(grant()).expect("mint");
        }
        grants.backdate_for_test(expired, MAX_GRANT_AGE + Duration::from_secs(1));
        grants.backdate_for_test(stale, ATTRIBUTION_RETENTION + Duration::from_secs(1));
        // Retention drop frees one slot; the expired pending grant becomes a
        // tombstone and is evicted for the second.
        grants.mint(grant()).expect("retention prune");
        assert!(!grants.lock().contains_key(&stale));
        assert!(grants.lock().contains_key(&expired));
        grants.mint(grant()).expect("expired prune");
        assert!(!grants.lock().contains_key(&expired));
        assert!(grants.mint(grant()).is_err());
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
