use std::collections::HashMap;
use std::io::{self, Write as _};
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
pub(crate) struct LayoutApplyReceipt {
    pub session_epoch: String,
    pub request_digest: String,
    pub effect_nonce: String,
    pub outcome: LayoutApplyOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
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
struct IdempotencyFile {
    #[serde(rename = "version")]
    _version: u32,
    session_epoch: String,
    #[serde(default)]
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

pub(crate) fn new_layout_session_epoch() -> Result<String, String> {
    random_nonce().map_err(|err| err.to_string())
}

pub(crate) fn load_layout_apply_ledger() -> io::Result<LayoutApplyLedger> {
    load_from_path(&idempotency_path())
}

pub(crate) fn save_layout_apply_ledger(ledger: &LayoutApplyLedger) -> io::Result<()> {
    save_to_path(&idempotency_path(), ledger)
}

pub(crate) fn clear_layout_apply_ledger() -> io::Result<()> {
    let path = idempotency_path();
    clear_path(&path)?;
    clear_path(&path.with_extension("json.tmp"))
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

fn load_from_path(path: &Path) -> io::Result<LayoutApplyLedger> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return LayoutApplyLedger::empty(),
        Err(err) => return Err(err),
    };
    if metadata.len() > MAX_LAYOUT_IDEMPOTENCY_FILE_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "API idempotency ledger exceeds the size limit",
        ));
    }
    let content = std::fs::read(path)?;
    let value: serde_json::Value = serde_json::from_slice(&content)?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ledger version"))?;

    if version == 1 {
        // Version 1 persisted the complete request, including cwd, argv, and environment values.
        // Replace it atomically with an empty nonsecret ledger instead of deserializing secrets.
        let ledger = LayoutApplyLedger::empty()?;
        save_to_path(path, &ledger)?;
        return Ok(ledger);
    }
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
    restrict_existing_file(path)?;
    Ok(ledger)
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
    getrandom::fill(&mut bytes).map_err(|err| io::Error::other(err.to_string()))?;
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
    create_private_dir(parent)?;
    let temp_path = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    restrict_file_options(&mut options);
    let mut temp = options.open(&temp_path)?;
    temp.write_all(&json)?;
    temp.sync_all()?;
    drop(temp);
    if let Err(err) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err);
    }
    restrict_existing_file(path)?;
    sync_directory(parent)?;
    Ok(())
}

fn clear_path(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn restrict_file_options(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.mode(0o600);
}

#[cfg(not(unix))]
fn restrict_file_options(_options: &mut std::fs::OpenOptions) {}

#[cfg(unix)]
fn create_private_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

#[cfg(unix)]
fn restrict_existing_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_existing_file(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
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
        assert_eq!(load_from_path(&path).unwrap(), ledger);
        cleanup(&path);
    }

    #[test]
    fn legacy_raw_receipts_are_replaced_without_deserializing_params() {
        let path = temp_path("legacy");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"layout_apply":{"key":{"params":{"root":{"type":"pane","env":{"TOKEN":"secret"}}},"outcome":{"state":"no_effect"}}}}"#,
        )
        .unwrap();

        let ledger = load_from_path(&path).unwrap();
        assert!(ledger.receipts.is_empty());
        assert!(!std::fs::read_to_string(&path).unwrap().contains("secret"));
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
