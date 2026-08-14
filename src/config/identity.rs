use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthStr;

use super::config_dir;

const IDENTITY_FILE: &str = "identity.toml";
const PROFILE_ID_BYTES: usize = 32;

/// Client-local, non-authoritative identity announced on an App connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientIdentity {
    pub display_name: Option<String>,
    pub frontend_profile_id: String,
    /// Local high-entropy capability used only to bind this client's native renderer process.
    pub renderer_binding_token: String,
}

#[derive(Debug)]
pub enum IdentityError {
    InvalidDisplayName(&'static str),
    InvalidProfileId,
    Io(std::io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
    Random(getrandom::Error),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDisplayName(reason) => write!(f, "invalid display name: {reason}"),
            Self::InvalidProfileId => write!(f, "invalid frontend profile id"),
            Self::Io(error) => error.fmt(f),
            Self::Parse(error) => error.fmt(f),
            Self::Serialize(error) => error.fmt(f),
            Self::Random(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<std::io::Error> for IdentityError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Deserialize, Serialize)]
struct IdentityFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    frontend_profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    renderer_binding_token: Option<String>,
}

pub fn identity_path() -> PathBuf {
    config_dir().join(IDENTITY_FILE)
}

/// Validates a display-only name without transforming it.
pub fn validate_display_name(value: &str) -> Result<(), IdentityError> {
    if value.trim() != value {
        return Err(IdentityError::InvalidDisplayName("must already be trimmed"));
    }
    if value.is_empty() || value.chars().count() > 64 {
        return Err(IdentityError::InvalidDisplayName(
            "must contain 1–64 Unicode scalar values",
        ));
    }
    if value.len() > 64 {
        return Err(IdentityError::InvalidDisplayName(
            "must be at most 64 UTF-8 bytes",
        ));
    }
    if UnicodeWidthStr::width(value) > 32 {
        return Err(IdentityError::InvalidDisplayName(
            "must be at most 32 display cells",
        ));
    }
    if value.chars().any(is_disallowed_name_char) {
        return Err(IdentityError::InvalidDisplayName(
            "contains a disallowed control character",
        ));
    }
    Ok(())
}

fn is_disallowed_name_char(ch: char) -> bool {
    matches!(ch, '[' | ']')
        || ch.is_control()
        || matches!(
            ch as u32,
            0x061c | 0x2028 | 0x2029 | 0xfeff | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2069
        )
}

pub fn validate_frontend_profile_id(value: &str) -> Result<(), IdentityError> {
    (value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    .then_some(())
    .ok_or(IdentityError::InvalidProfileId)
}
fn random_token() -> Result<String, IdentityError> {
    let mut random = [0; PROFILE_ID_BYTES];
    getrandom::fill(&mut random).map_err(IdentityError::Random)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
}

pub fn load_or_create_identity() -> Result<ClientIdentity, IdentityError> {
    let path = identity_path();
    match fs::read_to_string(&path) {
        Ok(content) => {
            let file: IdentityFile = toml::from_str(&content).map_err(IdentityError::Parse)?;
            if let Some(name) = &file.display_name {
                validate_display_name(name)?;
            }
            validate_frontend_profile_id(&file.frontend_profile_id)?;
            let renderer_binding_token = file.renderer_binding_token;
            let identity = ClientIdentity {
                display_name: file.display_name,
                frontend_profile_id: file.frontend_profile_id,
                renderer_binding_token: renderer_binding_token.clone().unwrap_or(random_token()?),
            };
            validate_frontend_profile_id(&identity.renderer_binding_token)?;
            if renderer_binding_token.is_none() {
                save_identity(&identity)?;
            }
            Ok(identity)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let identity = ClientIdentity {
                display_name: None,
                frontend_profile_id: random_token()?,
                renderer_binding_token: random_token()?,
            };
            save_identity(&identity)?;
            Ok(identity)
        }
        Err(error) => Err(IdentityError::Io(error)),
    }
}

/// Atomically replaces the local identity file after re-validating every field.
pub fn save_identity(identity: &ClientIdentity) -> Result<(), IdentityError> {
    save_identity_in(&config_dir(), identity)
}

fn save_identity_in(directory: &Path, identity: &ClientIdentity) -> Result<(), IdentityError> {
    if let Some(name) = &identity.display_name {
        validate_display_name(name)?;
    }
    validate_frontend_profile_id(&identity.frontend_profile_id)?;
    validate_frontend_profile_id(&identity.renderer_binding_token)?;
    fs::create_dir_all(directory)?;
    let path = directory.join(IDENTITY_FILE);
    let content = toml::to_string(&IdentityFile {
        display_name: identity.display_name.clone(),
        frontend_profile_id: identity.frontend_profile_id.clone(),
        renderer_binding_token: Some(identity.renderer_binding_token.clone()),
    })
    .map_err(IdentityError::Serialize)?;
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(IdentityError::Random)?;
    let mut temporary = TemporaryIdentityFile(directory.join(format!(
        ".{IDENTITY_FILE}.{}.tmp",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random),
    )));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary.0)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    replace_identity_file(&temporary.0, &path)?;
    temporary.persist();
    sync_parent_directory(directory)?;
    Ok(())
}

struct TemporaryIdentityFile(PathBuf);

impl TemporaryIdentityFile {
    fn persist(&mut self) {
        self.0.clear();
    }
}

impl Drop for TemporaryIdentityFile {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(directory: &Path) -> std::io::Result<()> {
    match fs::File::open(directory).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error) if directory_sync_unsupported(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(not(windows))]
fn directory_sync_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::Unsupported | std::io::ErrorKind::InvalidInput
    )
}

#[cfg(windows)]
fn sync_parent_directory(_: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn replace_identity_file(
    temporary: &std::path::Path,
    path: &std::path::Path,
) -> std::io::Result<()> {
    fs::rename(temporary, path)
}

#[cfg(windows)]
fn replace_identity_file(
    temporary: &std::path::Path,
    path: &std::path::Path,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            temporary.as_ptr(),
            path.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_name_validation_rejects_untrusted_boundary_characters_and_limits() {
        let over_scalars = "x".repeat(65);
        let over_width = "界".repeat(17);
        let over_bytes = "é".repeat(33);
        for invalid in [
            " leading",
            "trailing ",
            "[brackets]",
            "line\nbreak",
            "tab\tname",
            "escape\u{1b}",
            "bidi\u{202e}name",
            "isolate\u{2067}name",
            "zero\u{200b}width",
            "join\u{200d}er",
            "word\u{2060}joiner",
            "function\u{2061}application",
            "invisible\u{2062}times",
            "separator\u{2063}name",
            "plus\u{2064}name",
            "bom\u{feff}name",
            &over_scalars,
            &over_width,
            &over_bytes,
        ] {
            assert!(
                validate_display_name(invalid).is_err(),
                "{invalid:?} must be rejected"
            );
        }

        let max_width = "界".repeat(16);
        let max_bytes = "é".repeat(32);
        for valid in ["Ada", max_width.as_str(), max_bytes.as_str()] {
            assert!(
                validate_display_name(valid).is_ok(),
                "{valid:?} must be accepted"
            );
        }
    }
    #[test]
    fn stale_pid_temporary_file_does_not_block_identity_save() {
        let root = unique_test_directory("stale-temp");
        let identity = ClientIdentity {
            display_name: Some("Ada".to_string()),
            frontend_profile_id: "a".repeat(43),
            renderer_binding_token: "b".repeat(43),
        };
        fs::write(
            root.join(format!(".{IDENTITY_FILE}.{}.tmp", std::process::id())),
            "stale",
        )
        .unwrap();

        save_identity_in(&root, &identity).unwrap();

        assert!(fs::read_to_string(root.join(IDENTITY_FILE))
            .unwrap()
            .contains("Ada"));
        let _ = fs::remove_dir_all(root);
    }

    fn unique_test_directory(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "herdr-identity-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }
    #[test]
    fn identity_save_overwrites_existing_file() {
        let root = unique_test_directory("replace");
        let identity = ClientIdentity {
            display_name: Some("Ada".to_string()),
            frontend_profile_id: "a".repeat(43),
            renderer_binding_token: "b".repeat(43),
        };
        fs::write(root.join(IDENTITY_FILE), "old").unwrap();

        save_identity_in(&root, &identity).unwrap();

        let saved = fs::read_to_string(root.join(IDENTITY_FILE)).unwrap();
        assert!(saved.contains("Ada"));
        assert!(!saved.contains("old"));
        let _ = fs::remove_dir_all(root);
    }
}
