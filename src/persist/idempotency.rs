use std::collections::HashMap;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
#[cfg(debug_assertions)]
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::snapshot::{SessionHistorySnapshot, SessionSnapshot};
use crate::api::schema::LayoutApplyParams;

const IDEMPOTENCY_FILE_VERSION: u32 = 2;
pub(crate) const MAX_LAYOUT_IDEMPOTENCY_KEY_BYTES: usize = 128;
pub(crate) const MAX_LAYOUT_IDEMPOTENCY_RECEIPTS: usize = 1024;
const MAX_LAYOUT_IDEMPOTENCY_FILE_BYTES: usize = 512 * 1024;
const MAX_LAYOUT_IDEMPOTENCY_REQUEST_BYTES: usize = 64 * 1024;
const NONCE_BYTES: usize = 16;
const NONCE_HEX_LEN: usize = NONCE_BYTES * 2;
const DIGEST_HEX_LEN: usize = 32 * 2;

pub(crate) type LayoutApplyReceipts = HashMap<String, LayoutApplyReceipt>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LayoutApplyLedger {
    pub session_epoch: String,
    pub receipts: LayoutApplyReceipts,
}

impl LayoutApplyLedger {
    pub(crate) fn empty() -> io::Result<Self> {
        Ok(Self {
            session_epoch: random_nonce()?,
            receipts: LayoutApplyReceipts::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LayoutApplyReceipt {
    pub session_epoch: String,
    pub request_digest: String,
    pub effect_nonce: String,
    pub outcome: LayoutApplyOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum LayoutApplyOutcome {
    Pending {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_tab_id: Option<String>,
    },
    Committed {
        tab_id: String,
    },
    Cancelled,
    NoEffect,
}

impl LayoutApplyOutcome {
    pub(crate) fn pending(expected_tab_id: String) -> Self {
        Self::Pending {
            expected_tab_id: Some(expected_tab_id),
        }
    }

    pub(crate) fn expected_tab_id(&self) -> Option<&str> {
        match self {
            Self::Pending { expected_tab_id } => expected_tab_id.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdempotencyFile {
    #[serde(rename = "version")]
    _version: u32,
    session_epoch: String,
    layout_apply: LayoutApplyReceipts,
}

#[derive(Serialize)]
struct IdempotencyFileRef<'a> {
    version: u32,
    session_epoch: &'a str,
    layout_apply: &'a LayoutApplyReceipts,
}

fn idempotency_path() -> PathBuf {
    crate::session::data_dir().join("api-idempotency.json")
}

pub(crate) fn validate_layout_idempotency_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > MAX_LAYOUT_IDEMPOTENCY_KEY_BYTES {
        return Err(format!(
            "idempotency_key must contain 1 to {MAX_LAYOUT_IDEMPOTENCY_KEY_BYTES} bytes"
        ));
    }
    if !key.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        return Err("idempotency_key contains unsupported characters".into());
    }
    Ok(())
}

pub(crate) fn layout_apply_request_digest(params: &LayoutApplyParams) -> Result<String, String> {
    let value = serde_json::to_value(params).map_err(|err| err.to_string())?;
    let canonical = canonicalize_json(value);
    let bytes = serde_json::to_vec(&canonical).map_err(|err| err.to_string())?;
    if bytes.len() > MAX_LAYOUT_IDEMPOTENCY_REQUEST_BYTES {
        return Err(format!(
            "idempotent layout request exceeds {MAX_LAYOUT_IDEMPOTENCY_REQUEST_BYTES} serialized bytes"
        ));
    }
    Ok(hex(&Sha256::digest(bytes)))
}

pub(crate) fn new_layout_effect_nonce() -> Result<String, String> {
    random_nonce().map_err(|err| err.to_string())
}

#[cfg(unix)]
pub(crate) fn new_layout_session_epoch() -> Result<String, String> {
    random_nonce().map_err(|err| err.to_string())
}

pub(crate) fn validate_layout_session_epoch(epoch: &str) -> Result<(), String> {
    validate_hex(epoch, NONCE_HEX_LEN, "session epoch").map_err(|err| err.to_string())
}

pub(crate) fn load_layout_apply_ledger() -> io::Result<Option<LayoutApplyLedger>> {
    load_from_path(&idempotency_path())
}

pub(crate) fn save_layout_apply_ledger(ledger: &LayoutApplyLedger) -> io::Result<()> {
    save_to_path(&idempotency_path(), ledger)
}

pub(crate) fn save_layout_apply_session_snapshot(
    snapshot: &SessionSnapshot,
    history: Option<&SessionHistorySnapshot>,
) -> io::Result<()> {
    let data_dir = crate::session::data_dir();
    super::io::save_to_paths(
        &data_dir.join("session.json"),
        &data_dir.join("session-history.json"),
        snapshot,
        history,
    )
}

fn load_from_path(path: &Path) -> io::Result<Option<LayoutApplyLedger>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    // A dangling link (or an entry lost after inspection) is unavailable history,
    // not permission to initialize an empty ledger.
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "API idempotency ledger is not a regular file",
        ));
    }
    if metadata.len() > MAX_LAYOUT_IDEMPOTENCY_FILE_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "API idempotency ledger exceeds the size limit",
        ));
    }
    let mut content = Vec::new();
    std::io::Read::take(
        std::fs::File::open(path)?,
        MAX_LAYOUT_IDEMPOTENCY_FILE_BYTES as u64 + 1,
    )
    .read_to_end(&mut content)?;
    if content.len() > MAX_LAYOUT_IDEMPOTENCY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "API idempotency ledger exceeds the size limit",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&content)?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ledger version"))?;

    // ponytail: legacy raw receipts stay intact and unavailable. Erasing them
    // could restore spent keys; migration needs a separately reviewed contract.
    if version != IDEMPOTENCY_FILE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported API idempotency file version {version}"),
        ));
    }

    let file: IdempotencyFile = serde_json::from_value(value)?;
    let ledger = LayoutApplyLedger {
        session_epoch: file.session_epoch,
        receipts: file.layout_apply,
    };
    validate_ledger(&ledger)?;
    Ok(Some(ledger))
}

fn validate_ledger(ledger: &LayoutApplyLedger) -> io::Result<()> {
    validate_hex(&ledger.session_epoch, NONCE_HEX_LEN, "session epoch")?;
    if ledger.receipts.len() > MAX_LAYOUT_IDEMPOTENCY_RECEIPTS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "API idempotency ledger exceeds the receipt count limit",
        ));
    }
    for (key, receipt) in &ledger.receipts {
        validate_layout_idempotency_key(key)
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
        if receipt.session_epoch != ledger.session_epoch {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "API idempotency receipt belongs to a different session epoch",
            ));
        }
        validate_hex(&receipt.request_digest, DIGEST_HEX_LEN, "request digest")?;
        validate_hex(&receipt.effect_nonce, NONCE_HEX_LEN, "effect nonce")?;
        if receipt
            .outcome
            .expected_tab_id()
            .is_some_and(|tab_id| tab_id.len() > MAX_LAYOUT_IDEMPOTENCY_KEY_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "API idempotency receipt tab identity is too long",
            ));
        }
        if let LayoutApplyOutcome::Committed { tab_id } = &receipt.outcome {
            if tab_id.is_empty() || tab_id.len() > MAX_LAYOUT_IDEMPOTENCY_KEY_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "API idempotency committed tab identity is invalid",
                ));
            }
        }
    }
    Ok(())
}

fn validate_hex(value: &str, expected_len: usize, label: &str) -> io::Result<()> {
    if value.len() != expected_len || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid API idempotency {label}"),
        ));
    }
    Ok(())
}

fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonicalize_json).collect())
        }
        serde_json::Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, canonicalize_json(value)))
                    .collect(),
            )
        }
        value => value,
    }
}

fn random_nonce() -> io::Result<String> {
    let mut bytes = [0u8; NONCE_BYTES];
    crate::platform::fill_random_bytes(&mut bytes)?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

#[cfg(debug_assertions)]
fn maybe_fail_test_sidecar_write(path: &Path) -> io::Result<()> {
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);

    if path != idempotency_path()
        || std::env::var("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_WRITE_AT").is_err()
    {
        return Ok(());
    }
    let fail_at = std::env::var("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_WRITE_AT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let attempt = ATTEMPTS.fetch_add(1, Ordering::Relaxed) + 1;
    if attempt != fail_at {
        return Ok(());
    }
    if let Some(delay_ms) = std::env::var("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
    Err(io::Error::other(format!(
        "injected layout idempotency sidecar write failure at attempt {attempt}"
    )))
}

#[cfg(not(debug_assertions))]
fn maybe_fail_test_sidecar_write(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn save_to_path(path: &Path, ledger: &LayoutApplyLedger) -> io::Result<()> {
    validate_ledger(ledger)?;
    maybe_fail_test_sidecar_write(path)?;
    let file = IdempotencyFileRef {
        version: IDEMPOTENCY_FILE_VERSION,
        session_epoch: &ledger.session_epoch,
        layout_apply: &ledger.receipts,
    };
    let json = serde_json::to_vec(&file)?;
    if json.len() > MAX_LAYOUT_IDEMPOTENCY_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "API idempotency ledger exceeds the serialized size limit",
        ));
    }

    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ledger path has no parent"))?;
    crate::platform::create_private_state_directory(parent)?;
    let temp_path = path.with_extension("json.tmp");
    // The single server owns this temporary path. create_new prevents following
    // a stale symlink; a directory or other obstruction remains a write failure.
    match std::fs::remove_file(&temp_path) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let mut temp = crate::platform::create_private_state_file(&temp_path)?;
    temp.write_all(&json)?;
    temp.sync_all()?;
    drop(temp);
    if let Err(err) = crate::platform::replace_file(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    // Exercise the uncertain state after rename, not a pre-write failure.
    #[cfg(test)]
    if path == idempotency_path()
        && std::env::var_os("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_DIRECTORY_SYNC").is_some()
    {
        return Err(io::Error::other(
            "injected layout idempotency directory sync failure after rename",
        ));
    }
    crate::platform::sync_parent_directory(parent)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{LayoutNode, LayoutPane};

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "herdr-api-idempotency-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .join("api-idempotency.json")
    }

    fn cleanup(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    fn params(env: HashMap<String, String>) -> LayoutApplyParams {
        LayoutApplyParams {
            workspace_id: Some("w1".into()),
            tab_id: None,
            tab_label: Some("test".into()),
            focus: false,
            root: LayoutNode::Pane {
                pane: LayoutPane {
                    cwd: Some("/private/worktree".into()),
                    command: Some(vec!["secret-command".into()]),
                    env,
                    ..Default::default()
                },
            },
        }
    }

    fn receipt(epoch: &str, digest: String) -> LayoutApplyReceipt {
        LayoutApplyReceipt {
            session_epoch: epoch.into(),
            request_digest: digest,
            effect_nonce: "ab".repeat(NONCE_BYTES),
            outcome: LayoutApplyOutcome::Committed {
                tab_id: "w1:t2".into(),
            },
        }
    }

    #[test]
    fn digest_binds_all_layout_identity_fields() {
        let original = params(HashMap::new());
        let digest = layout_apply_request_digest(&original).unwrap();
        let mut variants = Vec::new();
        let mut changed = original.clone();
        changed.workspace_id = Some("other".into());
        variants.push(changed);
        let mut changed = original.clone();
        changed.tab_id = Some("w1:t2".into());
        variants.push(changed);
        let mut changed = original.clone();
        changed.tab_label = Some("other".into());
        variants.push(changed);
        let mut changed = original.clone();
        changed.focus = true;
        variants.push(changed);
        for field in ["cwd", "command", "env", "pane_id", "label"] {
            let mut changed = original.clone();
            let LayoutNode::Pane { pane } = &mut changed.root else {
                panic!("pane fixture");
            };
            match field {
                "cwd" => pane.cwd = Some("/other".into()),
                "command" => pane.command = Some(vec!["other".into()]),
                "env" => {
                    pane.env.insert("TOKEN".into(), "other".into());
                }
                "pane_id" => pane.pane_id = Some("w1:p2".into()),
                _ => pane.label = Some("other".into()),
            }
            variants.push(changed);
        }
        for changed in variants {
            assert_ne!(layout_apply_request_digest(&changed).unwrap(), digest);
        }
    }

    #[test]
    fn unknown_or_incomplete_ledger_state_stays_intact_and_unavailable() {
        let path = temp_path("unknown-state");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let epoch = "cd".repeat(NONCE_BYTES);
        let ledger = LayoutApplyLedger {
            session_epoch: epoch.clone(),
            receipts: LayoutApplyReceipts::from([(
                "spent".into(),
                receipt(&epoch, "ab".repeat(32)),
            )]),
        };
        save_to_path(&path, &ledger).unwrap();
        let original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for defect in ["version", "outcome", "missing_receipts", "extra", "epoch"] {
            let mut value = original.clone();
            match defect {
                "version" => value["version"] = serde_json::json!(99),
                "outcome" => {
                    value["layout_apply"]["spent"]["outcome"]["state"] = serde_json::json!("future")
                }
                "missing_receipts" => {
                    value.as_object_mut().unwrap().remove("layout_apply");
                }
                "extra" => value["unknown_state"] = serde_json::json!({}),
                _ => {
                    value["layout_apply"]["spent"]["session_epoch"] =
                        serde_json::json!("ef".repeat(16))
                }
            }
            let bytes = serde_json::to_vec(&value).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            assert!(load_from_path(&path).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        cleanup(&path);
    }

    #[test]
    fn request_and_ledger_byte_limits_fail_closed() {
        let large = params(HashMap::from([(
            "LARGE".into(),
            "x".repeat(MAX_LAYOUT_IDEMPOTENCY_REQUEST_BYTES),
        )]));
        assert!(layout_apply_request_digest(&large).is_err());
        let path = temp_path("oversized");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = vec![b' '; MAX_LAYOUT_IDEMPOTENCY_FILE_BYTES + 1];
        std::fs::write(&path, &bytes).unwrap();
        assert!(load_from_path(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        cleanup(&path);
    }

    #[test]
    fn digest_is_canonical_for_environment_order() {
        let left = params(HashMap::from([
            ("B".into(), "two".into()),
            ("A".into(), "one".into()),
        ]));
        let right = params(HashMap::from([
            ("A".into(), "one".into()),
            ("B".into(), "two".into()),
        ]));
        assert_eq!(
            layout_apply_request_digest(&left).unwrap(),
            layout_apply_request_digest(&right).unwrap()
        );
    }

    #[test]
    fn sidecar_contains_only_digest_and_recovery_metadata() {
        let path = temp_path("redacted");
        let epoch = "cd".repeat(NONCE_BYTES);
        let digest = layout_apply_request_digest(&params(HashMap::from([(
            "TOKEN".into(),
            "super-secret".into(),
        )])))
        .unwrap();
        let ledger = LayoutApplyLedger {
            session_epoch: epoch.clone(),
            receipts: LayoutApplyReceipts::from([("operation".into(), receipt(&epoch, digest))]),
        };

        save_to_path(&path, &ledger).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        for raw in [
            "super-secret",
            "secret-command",
            "/private/worktree",
            "TOKEN",
        ] {
            assert!(!saved.contains(raw));
        }
        assert_eq!(load_from_path(&path).unwrap(), Some(ledger));
        cleanup(&path);
    }

    #[test]
    fn ledger_absence_is_distinct_from_valid_empty_history() {
        let path = temp_path("missing-history");
        assert_eq!(load_from_path(&path).unwrap(), None);
        assert!(!path.exists());
        let ledger = LayoutApplyLedger::empty().unwrap();
        save_to_path(&path, &ledger).unwrap();
        assert_eq!(load_from_path(&path).unwrap(), Some(ledger));
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn dangling_ledger_symlink_is_unavailable_and_unchanged() {
        let path = temp_path("dangling-history");
        let target = path.with_file_name("missing-ledger-target");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(
            load_from_path(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
        assert!(!target.exists());
        cleanup(&path);
    }

    #[cfg(windows)]
    #[test]
    fn sidecar_is_owner_private_after_snapshot_creates_existing_windows_directory() {
        let path = temp_path("existing-windows-permissions");
        let parent = path.parent().unwrap();
        let ledger = LayoutApplyLedger::empty().unwrap();
        let mut snapshot = super::super::snapshot::parse_snapshot(
            r#"{"version":3,"workspaces":[],"active":null,"selected":0}"#,
        )
        .unwrap();
        snapshot.idempotency_epoch = Some(ledger.session_epoch.clone());
        super::super::io::save_to_paths(
            &parent.join("session.json"),
            &parent.join("session-history.json"),
            &snapshot,
            None,
        )
        .unwrap();

        // Child-scoped PowerShell inspects native ACLs on the admitted Windows
        // runner. No global environment or host ACL outside this fixture changes.
        let powershell = |script: &str| {
            let output = std::process::Command::new("powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    script,
                ])
                // Let Windows PowerShell use its own modules, not inherited
                // PowerShell 7 paths (same boundary as the Windows updater).
                .env_remove("PSModulePath")
                .env("HERDR_TEST_ACL_DIRECTORY", parent)
                .env("HERDR_TEST_ACL_FILE", &path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        powershell(
            r#"
$ErrorActionPreference = 'Stop'
$path = $env:HERDR_TEST_ACL_DIRECTORY
$acl = Get-Acl -LiteralPath $path
$acl.SetAccessRuleProtection($true, $false)
foreach ($rule in @($acl.Access)) { $acl.RemoveAccessRuleSpecific($rule) }
$everyone = [System.Security.Principal.SecurityIdentifier]::new('S-1-1-0')
$rule = [System.Security.AccessControl.FileSystemAccessRule]::new(
    $everyone, 'FullControl', 'ContainerInherit, ObjectInherit', 'None', 'Allow')
$acl.AddAccessRule($rule)
Set-Acl -LiteralPath $path -AclObject $acl
$world = @((Get-Acl -LiteralPath $path).Access | Where-Object {
    $_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value -eq 'S-1-1-0'
})
if ($world.Count -eq 0) { throw 'fixture must start with Everyone access' }
"#,
        );
        save_to_path(&path, &ledger).unwrap();
        powershell(
            r#"
$ErrorActionPreference = 'Stop'
$directory = Get-Acl -LiteralPath $env:HERDR_TEST_ACL_DIRECTORY
if (-not $directory.AreAccessRulesProtected) { throw 'directory still inherits its DACL' }
foreach ($path in @($env:HERDR_TEST_ACL_DIRECTORY, $env:HERDR_TEST_ACL_FILE)) {
    $acl = Get-Acl -LiteralPath $path
    $owner = $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value
    $allowed = @('S-1-5-18', 'S-1-3-4', $owner)
    if (@($acl.Access).Count -eq 0) { throw 'missing private access rules' }
    foreach ($rule in $acl.Access) {
        $sid = $rule.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value
        if ($rule.AccessControlType -ne 'Allow' -or $allowed -notcontains $sid) {
            throw "unexpected private-state access: $sid"
        }
    }
}
"#,
        );
        assert_eq!(load_from_path(&path).unwrap(), Some(ledger));
        cleanup(&path);
    }

    #[test]
    fn legacy_raw_receipts_are_rejected_without_erasing_spent_history() {
        let path = temp_path("legacy");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"layout_apply":{"key":{"params":{"root":{"type":"pane","env":{"TOKEN":"secret"}}},"outcome":{"state":"no_effect"}}}}"#,
        )
        .unwrap();

        let before = std::fs::read(&path).unwrap();
        assert!(load_from_path(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        cleanup(&path);
    }

    #[test]
    fn receipt_count_and_key_length_are_bounded() {
        assert!(
            validate_layout_idempotency_key(&"x".repeat(MAX_LAYOUT_IDEMPOTENCY_KEY_BYTES)).is_ok()
        );
        assert!(
            validate_layout_idempotency_key(&"x".repeat(MAX_LAYOUT_IDEMPOTENCY_KEY_BYTES + 1))
                .is_err()
        );
        assert!(validate_layout_idempotency_key("contains space").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_is_owner_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = temp_path("permissions");
        let ledger = LayoutApplyLedger::empty().unwrap();
        save_to_path(&path, &ledger).unwrap();
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        cleanup(&path);
    }
}
