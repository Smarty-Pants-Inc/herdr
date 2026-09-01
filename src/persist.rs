//! Session persistence — save/restore workspaces, layouts, and working directories.
//!
//! Stored at `~/.config/herdr/session.json`.
//! Optional pane screen history is stored separately at `session-history.json`.
//! Installed plugins are persisted separately at `plugins.json`.

mod idempotency;
mod io;
pub mod plugin_registry;
mod restore;
mod snapshot;

pub(crate) use self::idempotency::{
    layout_apply_request_digest, load_layout_apply_ledger, new_layout_effect_nonce,
    new_layout_session_epoch, save_layout_apply_ledger, save_layout_apply_session_snapshot,
    validate_layout_idempotency_key, LayoutApplyLedger, LayoutApplyOutcome, LayoutApplyReceipt,
    LayoutApplyReceipts, MAX_LAYOUT_IDEMPOTENCY_RECEIPTS,
};
pub use self::io::{clear, clear_history, load, load_history, save, SessionLoad};
pub use self::restore::restore;
#[cfg(unix)]
pub use self::restore::{handoff_pane_aliases, restore_handoff};
pub use self::snapshot::{
    capture, capture_history, DirectionSnapshot, LayoutSnapshot, SessionHistorySnapshot,
    SessionSnapshot, TabSnapshot, WorkspaceSnapshot,
};
