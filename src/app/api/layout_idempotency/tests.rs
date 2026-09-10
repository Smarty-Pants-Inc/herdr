use super::*;
use crate::api::schema::{
    ErrorResponse, LayoutApplyParams, LayoutNode, LayoutPane, SuccessResponse,
};
use crate::app::api::test_support::{exiting_test_command, shutdown_test_runtimes};
use crate::app::AppPolicy;
use crate::config::{Config, ShellModeConfig};
use crate::workspace::Workspace;

fn empty_app(persistent: bool) -> App {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &Config::default(),
        AppPolicy {
            restore_session: persistent,
            persist_session: persistent,
            ..AppPolicy::TEST
        },
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
    app.state.default_shell = exiting_test_command().into();
    app.state.shell_mode = ShellModeConfig::NonLogin;
    app
}

fn persistent_empty_app() -> App {
    empty_app(true)
}

fn add_workspace(app: &mut App) {
    app.state.workspaces = vec![Workspace::test_new("layout")];
    app.state.active = Some(0);
    app.state.selected = 0;
    app.state.ensure_test_terminals();
    app.state.assert_invariants_for_test();
}

fn persistent_app_with_workspace() -> App {
    let mut app = persistent_empty_app();
    add_workspace(&mut app);
    app
}

fn with_test_config_home<T>(name: &str, run: impl FnOnce(&std::path::Path) -> T) -> T {
    let _guard = crate::config::test_config_env_lock().lock().unwrap();
    let previous_config_home = std::env::var_os("XDG_CONFIG_HOME");
    let previous_session = std::env::var_os(crate::session::SESSION_ENV_VAR);
    let base = std::env::temp_dir().join(format!(
        "herdr-layout-idempotency-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::env::set_var("XDG_CONFIG_HOME", &base);
    std::env::remove_var(crate::session::SESSION_ENV_VAR);
    crate::session::clear_explicit_session_for_test();
    let result = run(&base);
    match previous_config_home {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    match previous_session {
        Some(value) => std::env::set_var(crate::session::SESSION_ENV_VAR, value),
        None => std::env::remove_var(crate::session::SESSION_ENV_VAR),
    }
    let _ = std::fs::remove_dir_all(base);
    result
}

fn idempotent_layout_params(
    workspace_id: Option<String>,
    idempotency_key: &str,
    tab_label: &str,
) -> LayoutIdempotentParams {
    LayoutIdempotentParams {
        idempotency_key: idempotency_key.into(),
        layout: LayoutApplyParams {
            workspace_id,
            tab_id: None,
            tab_label: Some(tab_label.into()),
            focus: false,
            root: LayoutNode::Pane {
                pane: LayoutPane {
                    command: Some(vec![exiting_test_command().into()]),
                    ..Default::default()
                },
            },
        },
    }
}

fn error_code(response: &str) -> String {
    serde_json::from_str::<ErrorResponse>(response)
        .unwrap()
        .error
        .code
}

fn pending_receipt(app: &App, params: &LayoutIdempotentParams, nonce: &str) -> LayoutApplyReceipt {
    LayoutApplyReceipt {
        session_epoch: app.layout_apply_epoch.clone(),
        request_digest: app.layout_apply_request_digest(&params.layout).unwrap(),
        effect_nonce: nonce.into(),
        outcome: LayoutApplyOutcome::pending(app.public_tab_id(0, 0).unwrap()),
    }
}

// Donor characterization: read-only absence must never become a cancellation.
#[tokio::test]
async fn reconcile_without_receipt_does_not_fence_later_apply() {
    with_test_config_home("reconcile-no-fence", |_| {
        let mut app = persistent_empty_app();
        let params = idempotent_layout_params(None, "cleanup-first", "cleanup");
        let absent: ErrorResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
            "cleanup".into(),
            params.clone(),
            true,
        ))
        .unwrap();
        assert_eq!(absent.error.code, "idempotency_no_effect");
        assert!(app.layout_apply_receipts.is_empty());
        add_workspace(&mut app);
        let applied: SuccessResponse =
            serde_json::from_str(&app.handle_layout_apply_idempotent("late".into(), params, false))
                .unwrap();
        assert!(matches!(applied.result, ResponseResult::LayoutApply { .. }));
        assert_eq!(app.state.workspaces[0].tabs.len(), 2);
        app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut app);
    });
}

#[tokio::test]
async fn layout_apply_replays_same_key_and_rejects_divergent_payload() {
    with_test_config_home("replay-conflict", |_| {
        let mut app = persistent_app_with_workspace();
        let params =
            idempotent_layout_params(Some(app.public_workspace_id(0)), "operation", "applied");
        let first: SuccessResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
            "first".into(),
            params.clone(),
            false,
        ))
        .unwrap();
        let ResponseResult::LayoutApply { layout } = first.result else {
            panic!("expected layout apply response");
        };
        let tab_count = app.state.workspaces[0].tabs.len();
        for operation in [
            LayoutOperation::Apply,
            LayoutOperation::Reconcile,
            LayoutOperation::Cancel,
        ] {
            let replay: SuccessResponse = serde_json::from_str(&app.handle_layout_operation(
                "replay".into(),
                params.clone(),
                operation,
            ))
            .unwrap();
            assert_eq!(
                replay.result,
                ResponseResult::LayoutApply {
                    layout: layout.clone()
                }
            );
            assert_eq!(app.state.workspaces[0].tabs.len(), tab_count);
        }
        let mut divergent = params;
        divergent.layout.tab_label = Some("different".into());
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("conflict".into(), divergent)),
            "idempotency_conflict"
        );
        assert_eq!(app.state.workspaces[0].tabs.len(), tab_count);
        app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut app);
    });
}

#[tokio::test]
async fn fresh_reconcile_keys_do_not_exhaust_idempotency_capacity() {
    with_test_config_home("reconcile-capacity", |_| {
        let mut app = persistent_app_with_workspace();
        let workspace_id = app.public_workspace_id(0);
        for index in 0..=crate::persist::MAX_LAYOUT_IDEMPOTENCY_RECEIPTS {
            let params = idempotent_layout_params(
                Some(workspace_id.clone()),
                &format!("reconcile-miss-{index}"),
                "reconcile",
            );
            assert_eq!(
                error_code(&app.handle_layout_apply_idempotent(
                    format!("reconcile-{index}"),
                    params,
                    true
                )),
                "idempotency_no_effect"
            );
        }
        assert!(app.layout_apply_receipts.is_empty());
        let applied: SuccessResponse = serde_json::from_str(&app.handle_layout_apply_idempotent(
            "apply".into(),
            idempotent_layout_params(Some(workspace_id), "real-apply", "applied"),
            false,
        ))
        .unwrap();
        assert!(matches!(applied.result, ResponseResult::LayoutApply { .. }));
        assert_eq!(app.layout_apply_receipts.len(), 1);
        shutdown_test_runtimes(&mut app);
    });
}

#[test]
fn absent_cancel_is_durable_payload_bound_and_fences_late_apply_after_restart() {
    with_test_config_home("cancel-restart", |_| {
        let params = idempotent_layout_params(None, "cancelled", "late");
        let mut app = persistent_empty_app();
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("cancel".into(), params.clone())),
            "idempotency_no_effect"
        );
        let ledger = crate::persist::load_layout_apply_ledger().unwrap().unwrap();
        assert!(matches!(
            ledger.receipts["cancelled"].outcome,
            LayoutApplyOutcome::Cancelled
        ));
        let snapshot = crate::persist::load_checked().unwrap().unwrap();
        assert_eq!(
            snapshot.idempotency_epoch.as_deref(),
            Some(ledger.session_epoch.as_str())
        );
        let receipt = ledger.receipts["cancelled"].clone();
        // An empty session checkpoint must retain the cancellation, not rotate the epoch.
        app.save_session_now();
        drop(app);
        let mut app = persistent_empty_app();
        assert_eq!(app.layout_apply_epoch, ledger.session_epoch);
        add_workspace(&mut app);
        for operation in [
            LayoutOperation::Cancel,
            LayoutOperation::Apply,
            LayoutOperation::Reconcile,
        ] {
            assert_eq!(
                error_code(&app.handle_layout_operation("late".into(), params.clone(), operation)),
                "idempotency_no_effect"
            );
            assert_eq!(app.state.workspaces[0].tabs.len(), 1);
            assert!(app.terminal_runtimes.is_empty());
        }
        assert_eq!(
            crate::persist::load_layout_apply_ledger()
                .unwrap()
                .unwrap()
                .receipts["cancelled"],
            receipt
        );
        let mut divergent = params.clone();
        divergent.layout.focus = true;
        for operation in [
            LayoutOperation::Cancel,
            LayoutOperation::Apply,
            LayoutOperation::Reconcile,
        ] {
            assert_eq!(
                error_code(&app.handle_layout_operation(
                    "conflict".into(),
                    divergent.clone(),
                    operation
                )),
                "idempotency_conflict"
            );
        }
        app.state.assert_invariants_for_test();
        drop(app);

        // Loss of only the ledger must not turn a bound epoch into unused keys.
        let ledger_path = crate::session::data_dir().join("api-idempotency.json");
        let snapshot_path = crate::session::data_dir().join("session.json");
        let before = std::fs::read(&snapshot_path).unwrap();
        std::fs::remove_file(&ledger_path).unwrap();
        let mut restarted = persistent_empty_app();
        add_workspace(&mut restarted);
        for operation in [
            LayoutOperation::Cancel,
            LayoutOperation::Apply,
            LayoutOperation::Reconcile,
        ] {
            assert_eq!(
                error_code(&restarted.handle_layout_operation(
                    "lost-history".into(),
                    params.clone(),
                    operation
                )),
                "idempotency_unavailable"
            );
            assert_eq!(restarted.state.workspaces[0].tabs.len(), 1);
            assert!(restarted.terminal_runtimes.is_empty());
        }
        assert!(crate::persist::load_layout_apply_ledger()
            .unwrap()
            .is_none());
        assert!(!ledger_path.exists());
        assert_eq!(std::fs::read(snapshot_path).unwrap(), before);
        restarted.state.assert_invariants_for_test();
    });
}

#[test]
fn fresh_empty_history_is_durable_before_epoch_binding() {
    with_test_config_home("fresh-history", |_| {
        let mut app = persistent_empty_app();
        let ledger = crate::persist::load_layout_apply_ledger().unwrap().unwrap();
        assert!(ledger.receipts.is_empty());
        assert_eq!(app.layout_apply_epoch, ledger.session_epoch);
        assert!(crate::persist::load_checked().unwrap().is_none());
        app.save_layout_apply_session_snapshot_now().unwrap();
        drop(app);

        let mut restarted = persistent_empty_app();
        assert!(restarted.layout_apply_receipts_error.is_none());
        assert_eq!(restarted.layout_apply_epoch, ledger.session_epoch);
        assert!(restarted.layout_apply_receipts.is_empty());
        let path = crate::session::data_dir().join("api-idempotency.json");
        let before = std::fs::read(&path).unwrap();
        let params = idempotent_layout_params(None, "fresh-cancel", "late");
        assert_eq!(
            error_code(&restarted.handle_layout_apply_idempotent(
                "read".into(),
                params.clone(),
                true
            )),
            "idempotency_no_effect"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            error_code(&restarted.handle_layout_cancel_idempotent("cancel".into(), params)),
            "idempotency_no_effect"
        );
    });
}

#[test]
fn failed_first_history_write_does_not_publish_a_bound_epoch() {
    with_test_config_home("first-history-failure", |_| {
        let obstruction = crate::session::data_dir().join("api-idempotency.json.tmp");
        std::fs::create_dir_all(&obstruction).unwrap();
        let mut app = persistent_empty_app();
        assert!(app.layout_apply_receipts_error.is_some());
        assert!(app.layout_apply_epoch.is_empty());
        assert!(app.layout_apply_receipts.is_empty());
        app.save_session_now();
        app.save_layout_apply_session_snapshot_now().unwrap();
        assert!(crate::persist::load_checked()
            .unwrap()
            .unwrap()
            .idempotency_epoch
            .is_none());
        assert!(crate::persist::load_layout_apply_ledger()
            .unwrap()
            .is_none());
        drop(app);

        std::fs::remove_dir(obstruction).unwrap();
        let restarted = persistent_empty_app();
        assert!(restarted.layout_apply_receipts_error.is_none());
        assert!(!restarted.layout_apply_epoch.is_empty());
        assert!(crate::persist::load_layout_apply_ledger()
            .unwrap()
            .unwrap()
            .receipts
            .is_empty());
    });
}

#[cfg(unix)]
#[test]
fn epoch_free_handoff_load_failure_does_not_publish_a_temporary_epoch() {
    with_test_config_home("epoch-free-handoff-load-failure", |_| {
        let source = empty_app(false);
        let snapshot = crate::persist::capture(
            &source.state.workspaces,
            &source.state.terminals,
            &source.terminal_runtimes,
            source.state.active,
            source.state.selected,
        );
        assert!(snapshot.idempotency_epoch.is_none());
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut imported = App::new_from_handoff(
            &Config::default(),
            None,
            rx,
            crate::api::EventHub::default(),
            &snapshot,
            &mut std::collections::HashMap::new(),
        )
        .unwrap();
        let obstruction = crate::session::data_dir().join("api-idempotency.json");
        std::fs::create_dir_all(&obstruction).unwrap();
        imported.assume_handoff_ownership();
        imported.initialize_layout_apply_idempotency_after_handoff(Some(None));
        imported.save_layout_apply_session_snapshot_now().unwrap();
        assert!(crate::persist::load_checked()
            .unwrap()
            .unwrap()
            .idempotency_epoch
            .is_none());
        imported.save_session_now();
        assert!(crate::persist::load_checked()
            .unwrap()
            .unwrap()
            .idempotency_epoch
            .is_none());
        assert!(imported.layout_apply_epoch.is_empty());
        assert!(imported.layout_apply_receipts_error.is_some());
        assert!(!imported.layout_apply_quarantined);
        assert!(!imported.state.should_quit);
        assert!(imported.policy.persist_session);
        assert!(imported.state.workspaces.is_empty());
        assert!(imported.terminal_runtimes.is_empty());
        let params = idempotent_layout_params(None, "first-after-repair", "unused");
        assert_eq!(
            error_code(&imported.handle_layout_cancel_idempotent("blocked".into(), params.clone())),
            "idempotency_unavailable"
        );
        imported.state.assert_invariants_for_test();
        drop(imported);

        std::fs::remove_dir(obstruction).unwrap();
        let mut restarted = persistent_empty_app();
        assert!(restarted.layout_apply_receipts_error.is_none());
        assert_eq!(
            error_code(&restarted.handle_layout_cancel_idempotent("recovered".into(), params)),
            "idempotency_no_effect"
        );
        assert!(restarted.state.workspaces.is_empty());
        assert!(restarted.terminal_runtimes.is_empty());
        restarted.state.assert_invariants_for_test();
    });
}

#[test]
fn an_empty_ledger_cannot_be_rebound_to_a_different_snapshot_epoch() {
    with_test_config_home("empty-epoch-mismatch", |_| {
        let mut app = persistent_empty_app();
        app.save_layout_apply_session_snapshot_now().unwrap();
        let path = crate::session::data_dir().join("api-idempotency.json");
        let before = std::fs::read(&path).unwrap();
        let other_epoch = if app.layout_apply_epoch == "ff".repeat(16) {
            "ee".repeat(16)
        } else {
            "ff".repeat(16)
        };
        app.initialize_layout_apply_idempotency(Some(Some(&other_epoch)));
        assert!(app.layout_apply_receipts_error.is_some());
        assert_eq!(std::fs::read(path).unwrap(), before);
    });
}

#[test]
fn cancelled_receipts_require_durable_revalidation_after_restart() {
    with_test_config_home("cancel-directory-sync", |_| {
        let mut app = persistent_empty_app();
        let old = idempotent_layout_params(None, "older-cancel", "old");
        let params = idempotent_layout_params(None, "uncertain-cancel", "late");
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("old".into(), old.clone())),
            "idempotency_no_effect"
        );
        let path = crate::session::data_dir().join("api-idempotency.json");
        let before = std::fs::read(&path).unwrap();
        std::env::set_var("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_DIRECTORY_SYNC", "1");
        let failed = app.handle_layout_cancel_idempotent("uncertain".into(), params.clone());
        let visible = crate::persist::load_layout_apply_ledger().unwrap().unwrap();
        assert_eq!(error_code(&failed), "idempotency_persist_failed");
        assert_ne!(std::fs::read(&path).unwrap(), before);
        assert!(matches!(
            visible.receipts["uncertain-cancel"].outcome,
            LayoutApplyOutcome::Cancelled
        ));
        assert!(visible.receipts.contains_key("older-cancel"));
        drop(app);

        // Reading the renamed bytes is not proof of a completed durability barrier.
        let mut restarted = persistent_empty_app();
        assert!(restarted.layout_apply_receipts_error.is_some());
        assert!(!restarted.layout_apply_quarantined);
        assert!(!restarted.state.should_quit);
        add_workspace(&mut restarted);
        for request in [&old, &params] {
            for operation in [
                LayoutOperation::Cancel,
                LayoutOperation::Apply,
                LayoutOperation::Reconcile,
            ] {
                assert_eq!(
                    error_code(&restarted.handle_layout_operation(
                        "unconfirmed".into(),
                        request.clone(),
                        operation
                    )),
                    "idempotency_unavailable"
                );
            }
        }
        assert_eq!(restarted.state.workspaces[0].tabs.len(), 1);
        assert!(restarted.terminal_runtimes.is_empty());
        drop(restarted);
        std::env::remove_var("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_DIRECTORY_SYNC");

        let mut confirmed = persistent_empty_app();
        assert!(confirmed.layout_apply_receipts_error.is_none());
        for request in [old, params] {
            assert_eq!(
                error_code(&confirmed.handle_layout_apply_idempotent(
                    "confirmed".into(),
                    request,
                    false
                )),
                "idempotency_no_effect"
            );
        }
        assert_eq!(
            crate::persist::load_layout_apply_ledger().unwrap(),
            Some(visible)
        );
    });
}

#[cfg(unix)]
#[test]
fn post_handoff_revalidation_failure_keeps_existing_owner_usable() {
    with_test_config_home("handoff-directory-sync", |_| {
        let mut app = persistent_app_with_workspace();
        let params = idempotent_layout_params(None, "handoff-cancel", "late");
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("cancel".into(), params.clone())),
            "idempotency_no_effect"
        );
        let epoch = app.layout_apply_epoch.clone();
        let tab_id = app.public_tab_id(0, 0).unwrap();
        std::env::set_var("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_DIRECTORY_SYNC", "1");
        app.initialize_layout_apply_idempotency_after_handoff(Some(Some(&epoch)));
        std::env::remove_var("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_DIRECTORY_SYNC");
        assert!(app.layout_apply_receipts_error.is_some());
        assert!(!app.layout_apply_quarantined);
        assert!(!app.state.should_quit);
        assert!(app.policy.persist_session);
        assert!(app.session_save_deadline.is_some());
        assert_eq!(app.public_tab_id(0, 0).as_deref(), Some(tab_id.as_str()));
        assert_eq!(
            error_code(&app.handle_layout_apply_idempotent("late".into(), params, false)),
            "idempotency_unavailable"
        );
        app.state.assert_invariants_for_test();
    });
}

#[test]
fn cancellation_failure_never_returns_authoritative_no_effect() {
    for obstruction in ["session.json.tmp", "api-idempotency.json.tmp"] {
        with_test_config_home(obstruction, |_| {
            let mut app = persistent_app_with_workspace();
            let params = idempotent_layout_params(None, "cancel-failed", "late");
            std::fs::create_dir_all(crate::session::data_dir().join(obstruction)).unwrap();
            let code =
                error_code(&app.handle_layout_cancel_idempotent("cancel".into(), params.clone()));
            assert_eq!(
                code,
                if obstruction == "session.json.tmp" {
                    "session_persist_failed"
                } else {
                    "idempotency_persist_failed"
                }
            );
            assert!(app.layout_apply_receipts.is_empty());
            assert_eq!(app.state.workspaces[0].tabs.len(), 1);
            assert!(app.terminal_runtimes.is_empty());
            if obstruction == "api-idempotency.json.tmp" {
                // A sidecar failure can occur after rename. Fail closed in memory
                // rather than using a stale absent-key observation for a late apply.
                assert_eq!(
                    error_code(&app.handle_layout_apply_idempotent("late".into(), params, false)),
                    "idempotency_unavailable"
                );
            }
        });
    }
}

#[test]
fn cancellation_capacity_never_evicts_spent_keys() {
    with_test_config_home("cancel-capacity", |_| {
        let mut app = persistent_empty_app();
        let params = idempotent_layout_params(None, "spent-0", "cancel");
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("first".into(), params.clone())),
            "idempotency_no_effect"
        );
        let receipt = app.layout_apply_receipts["spent-0"].clone();
        let receipts = (0..crate::persist::MAX_LAYOUT_IDEMPOTENCY_RECEIPTS)
            .map(|index| (format!("spent-{index}"), receipt.clone()))
            .collect();
        app.store_layout_apply_receipts(receipts).unwrap();
        let before =
            std::fs::read(crate::session::data_dir().join("api-idempotency.json")).unwrap();
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent(
                "full".into(),
                idempotent_layout_params(None, "new-key", "cancel")
            )),
            "idempotency_capacity"
        );
        assert_eq!(
            app.layout_apply_receipts.len(),
            crate::persist::MAX_LAYOUT_IDEMPOTENCY_RECEIPTS
        );
        assert_eq!(
            std::fs::read(crate::session::data_dir().join("api-idempotency.json")).unwrap(),
            before
        );
        assert_eq!(
            error_code(&app.handle_layout_apply_idempotent("spent".into(), params, false)),
            "idempotency_no_effect"
        );
    });
}

#[test]
fn future_session_snapshot_blocks_mutation_and_preserves_bytes() {
    with_test_config_home("future-session", |_| {
        let path = crate::session::data_dir().join("session.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = br#"{"version":4294967295,"workspaces":[],"active":null,"selected":0}"#;
        std::fs::write(&path, content).unwrap();
        let mut app = persistent_empty_app();
        assert!(app.session_persistence_blocked);
        assert!(app.state.should_quit);
        assert!(app.state.workspaces.is_empty());
        let response = app.handle_api_request(crate::api::schema::Request {
            id: "blocked".into(),
            method: crate::api::schema::Method::LayoutApply(
                idempotent_layout_params(None, "unused", "blocked").layout,
            ),
        });
        assert_eq!(error_code(&response), "session_snapshot_unsupported");
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent(
                "cancel".into(),
                idempotent_layout_params(None, "unused", "blocked")
            )),
            "session_snapshot_unsupported"
        );
        app.save_session_now();
        assert_eq!(std::fs::read(path).unwrap(), content);
    });
}

#[test]
fn no_session_rejects_idempotent_layout_methods() {
    let mut app = empty_app(false);
    add_workspace(&mut app);
    let params = idempotent_layout_params(Some(app.public_workspace_id(0)), "unsupported", "keyed");
    for operation in [
        LayoutOperation::Apply,
        LayoutOperation::Reconcile,
        LayoutOperation::Cancel,
    ] {
        assert_eq!(
            error_code(&app.handle_layout_operation("request".into(), params.clone(), operation)),
            "unsupported_in_no_session"
        );
    }
    assert_eq!(app.state.workspaces[0].tabs.len(), 1);
    assert!(app.layout_apply_receipts.is_empty());
}

#[test]
fn failed_layout_apply_no_effect_is_payload_bound() {
    with_test_config_home("failed-apply-payload-binding", |_| {
        let mut app = persistent_app_with_workspace();
        let params =
            idempotent_layout_params(Some("missing-workspace".into()), "failed-apply", "failed");
        assert_eq!(
            error_code(&app.handle_layout_apply_idempotent("first".into(), params.clone(), false)),
            "workspace_not_found"
        );
        assert!(matches!(
            app.layout_apply_receipts["failed-apply"].outcome,
            LayoutApplyOutcome::NoEffect
        ));
        assert_eq!(
            error_code(&app.handle_layout_apply_idempotent("replay".into(), params.clone(), false)),
            "idempotency_no_effect"
        );
        let mut divergent = params;
        divergent.layout.tab_label = Some("different".into());
        assert_eq!(
            error_code(&app.handle_layout_apply_idempotent("conflict".into(), divergent, false)),
            "idempotency_conflict"
        );
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
    });
}

#[test]
fn keyed_layout_methods_fail_closed_after_unknown_ledger_load() {
    for content in [
        "{not-json",
        r#"{"version":1,"layout_apply":{"spent":{}}}"#,
        r#"{"version":4294967295,"layout_apply":{}}"#,
    ] {
        with_test_config_home("unavailable-load", |_| {
            let path = crate::session::data_dir().join("api-idempotency.json");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            let mut app = persistent_empty_app();
            assert!(app.layout_apply_receipts_error.is_some());
            add_workspace(&mut app);
            let params = idempotent_layout_params(
                Some(app.public_workspace_id(0)),
                "spent",
                "must-not-apply",
            );
            for operation in [
                LayoutOperation::Apply,
                LayoutOperation::Reconcile,
                LayoutOperation::Cancel,
            ] {
                assert_eq!(
                    error_code(&app.handle_layout_operation(
                        "req".into(),
                        params.clone(),
                        operation
                    )),
                    "idempotency_unavailable"
                );
            }
            app.save_session_now();
            assert_eq!(std::fs::read_to_string(path).unwrap(), content);
            assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        });
    }
}

#[test]
fn pre_effect_session_checkpoint_restores_pending_save_on_failure() {
    with_test_config_home("pre-effect-checkpoint-failure", |_| {
        let mut app = persistent_app_with_workspace();
        app.save_session_now();
        let path = crate::session::data_dir().join("session.json");
        let before = std::fs::read(&path).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        app.session_save_deadline = Some(deadline);
        app.persist_pane_history = true;
        std::fs::create_dir_all(crate::session::data_dir().join("session-history.json.tmp"))
            .unwrap();
        assert!(!app
            .save_layout_apply_session_snapshot_now()
            .unwrap_err()
            .is_empty());
        assert_eq!(app.session_save_deadline, Some(deadline));
        assert_eq!(std::fs::read(path).unwrap(), before);
    });
}

#[test]
fn baseline_session_save_failure_has_no_effect_or_receipt() {
    with_test_config_home("session-save-failure", |_| {
        let mut app = persistent_app_with_workspace();
        std::fs::create_dir_all(crate::session::data_dir().join("session.json.tmp")).unwrap();
        let params = idempotent_layout_params(
            Some(app.public_workspace_id(0)),
            "session-save-failure",
            "durable",
        );
        assert_eq!(
            error_code(&app.handle_layout_apply_idempotent("req".into(), params, false)),
            "session_persist_failed"
        );
        assert!(!app.layout_apply_quarantined);
        assert!(!app.state.should_quit);
        assert!(app.policy.persist_session);
        assert!(!app
            .layout_apply_receipts
            .contains_key("session-save-failure"));
        assert!(crate::persist::load_layout_apply_ledger()
            .unwrap()
            .unwrap()
            .receipts
            .is_empty());
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
        assert!(!crate::session::data_dir().join("session.json").exists());
    });
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn post_effect_receipt_failure_preserves_pending_and_quarantines() {
    with_test_config_home("commit-sidecar-failure", |_| {
        let mut app = persistent_app_with_workspace();
        let params =
            idempotent_layout_params(Some(app.public_workspace_id(0)), "commit-failed", "durable");
        std::env::set_var("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_WRITE_AT", "2");
        let response = app.handle_layout_apply_idempotent("apply".into(), params.clone(), false);
        std::env::remove_var("HERDR_TEST_LAYOUT_IDEMPOTENCY_FAIL_WRITE_AT");
        assert_eq!(error_code(&response), "idempotency_persist_failed");
        assert!(app.layout_apply_quarantined);
        assert!(app.state.should_quit);
        assert!(!app.policy.persist_session);
        let ledger = crate::persist::load_layout_apply_ledger().unwrap().unwrap();
        let pending = &ledger.receipts["commit-failed"];
        assert!(matches!(
            pending.outcome,
            LayoutApplyOutcome::Pending { .. }
        ));
        let snapshot = crate::persist::load_checked().unwrap().unwrap();
        assert_eq!(
            snapshot.idempotency_epoch.as_deref(),
            Some(ledger.session_epoch.as_str())
        );
        assert!(snapshot
            .workspaces
            .iter()
            .flat_map(|workspace| &workspace.tabs)
            .any(|tab| tab.layout_effect_nonce.as_deref() == Some(pending.effect_nonce.as_str())));
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("cancel".into(), params)),
            "server_unavailable"
        );
        app.state.assert_invariants_for_test();
        shutdown_test_runtimes(&mut app);
    });
}

#[test]
fn startup_pending_reconciliation_failure_uses_quarantine() {
    with_test_config_home("startup-sidecar-failure", |_| {
        let mut app = persistent_app_with_workspace();
        let params = idempotent_layout_params(None, "startup-pending", "pending");
        let nonce = "ab".repeat(16);
        app.state.workspaces[0].tabs[0].layout_effect_nonce = Some(nonce.clone());
        let receipt = pending_receipt(&app, &params, &nonce);
        app.store_layout_apply_receipt(params.idempotency_key.clone(), receipt)
            .unwrap();
        let epoch = app.layout_apply_epoch.clone();
        std::fs::create_dir_all(crate::session::data_dir().join("api-idempotency.json.tmp"))
            .unwrap();
        app.initialize_layout_apply_idempotency(Some(Some(&epoch)));
        assert!(app.layout_apply_quarantined);
        assert!(app.state.should_quit);
        assert!(!app.policy.persist_session);
        assert!(matches!(
            app.layout_apply_receipts["startup-pending"].outcome,
            LayoutApplyOutcome::Pending { .. }
        ));
    });
}

#[test]
fn pending_receipt_without_matching_live_nonce_stays_pending() {
    with_test_config_home("pending-without-nonce", |_| {
        let mut app = persistent_app_with_workspace();
        let params = idempotent_layout_params(
            Some(app.public_workspace_id(0)),
            "pending-without-nonce",
            "pending",
        );
        let receipt = pending_receipt(&app, &params, &"ab".repeat(16));
        app.store_layout_apply_receipt(params.idempotency_key.clone(), receipt)
            .unwrap();
        let before = crate::persist::load_layout_apply_ledger().unwrap();
        for operation in [
            LayoutOperation::Reconcile,
            LayoutOperation::Cancel,
            LayoutOperation::Apply,
        ] {
            assert_eq!(
                error_code(&app.handle_layout_operation(
                    "pending".into(),
                    params.clone(),
                    operation
                )),
                "idempotency_pending"
            );
        }
        assert!(matches!(
            app.layout_apply_receipts["pending-without-nonce"].outcome,
            LayoutApplyOutcome::Pending { .. }
        ));
        assert_eq!(crate::persist::load_layout_apply_ledger().unwrap(), before);
    });
}

#[test]
fn pending_reconciliation_snapshot_failure_quarantines() {
    with_test_config_home("pending-snapshot-failure", |_| {
        let mut app = persistent_app_with_workspace();
        let params = idempotent_layout_params(
            Some(app.public_workspace_id(0)),
            "pending-snapshot-failure",
            "pending",
        );
        let nonce = "ef".repeat(16);
        app.state.workspaces[0].tabs[0].layout_effect_nonce = Some(nonce.clone());
        let receipt = pending_receipt(&app, &params, &nonce);
        app.store_layout_apply_receipt(params.idempotency_key.clone(), receipt)
            .unwrap();
        std::fs::create_dir_all(crate::session::data_dir().join("session.json.tmp")).unwrap();
        assert_eq!(
            error_code(&app.handle_layout_apply_idempotent("reconcile".into(), params, true)),
            "session_persist_failed"
        );
        assert!(app.layout_apply_quarantined);
        assert!(app.state.should_quit);
        assert!(!app.policy.persist_session);
        assert!(matches!(
            app.layout_apply_receipts["pending-snapshot-failure"].outcome,
            LayoutApplyOutcome::Pending { .. }
        ));
        shutdown_test_runtimes(&mut app);
    });
}

#[cfg(unix)]
#[test]
fn post_commit_pending_reconciliation_failure_keeps_handoff_owner_usable() {
    with_test_config_home("post-commit-reconciliation-failure", |_| {
        let mut app = persistent_app_with_workspace();
        let params = idempotent_layout_params(
            Some(app.public_workspace_id(0)),
            "post-commit-reconciliation-failure",
            "pending",
        );
        let nonce = "ab".repeat(16);
        app.state.workspaces[0].tabs[0].layout_effect_nonce = Some(nonce.clone());
        let receipt = pending_receipt(&app, &params, &nonce);
        app.store_layout_apply_receipt(params.idempotency_key.clone(), receipt)
            .unwrap();
        let epoch = app.layout_apply_epoch.clone();
        std::fs::create_dir_all(crate::session::data_dir().join("api-idempotency.json.tmp"))
            .unwrap();
        app.initialize_layout_apply_idempotency_after_handoff(Some(Some(&epoch)));
        assert!(app.layout_apply_receipts_error.is_some());
        assert!(!app.layout_apply_quarantined);
        assert!(!app.state.should_quit);
        assert!(app.policy.persist_session);
        assert!(!app.state.session_dirty);
        assert!(app.session_save_deadline.is_some());
        assert!(matches!(
            app.layout_apply_receipts[&params.idempotency_key].outcome,
            LayoutApplyOutcome::Pending { .. }
        ));
        app.state.assert_invariants_for_test();
    });
}

#[test]
fn pending_receipt_commits_only_for_matching_live_nonce() {
    with_test_config_home("pending-matching-nonce", |_| {
        let mut app = persistent_app_with_workspace();
        let params = idempotent_layout_params(
            Some(app.public_workspace_id(0)),
            "pending-matching-nonce",
            "pending",
        );
        let nonce = "cd".repeat(16);
        app.state.workspaces[0].tabs[0].layout_effect_nonce = Some(nonce.clone());
        let tab_id = app.public_tab_id(0, 0).unwrap();
        let receipt = pending_receipt(&app, &params, &nonce);
        app.store_layout_apply_receipt(params.idempotency_key.clone(), receipt)
            .unwrap();
        let success: SuccessResponse =
            serde_json::from_str(&app.handle_layout_cancel_idempotent("recover".into(), params))
                .unwrap();
        let ResponseResult::LayoutApply { layout } = success.result else {
            panic!("expected reconciled layout");
        };
        assert_eq!(layout.tab_id, tab_id);
        assert!(matches!(
            app.layout_apply_receipts["pending-matching-nonce"].outcome,
            LayoutApplyOutcome::Committed { .. }
        ));
        assert_eq!(app.state.workspaces[0].tabs.len(), 1);
    });
}

#[test]
fn committed_recovery_rejects_missing_duplicate_and_wrong_tab_nonce() {
    with_test_config_home("committed-nonce", |_| {
        let mut app = persistent_app_with_workspace();
        let params = idempotent_layout_params(None, "committed", "recover");
        let nonce = "cd".repeat(16);
        let mut receipt = pending_receipt(&app, &params, &nonce);
        receipt.outcome = LayoutApplyOutcome::Committed {
            tab_id: app.public_tab_id(0, 0).unwrap(),
        };
        app.store_layout_apply_receipt(params.idempotency_key.clone(), receipt)
            .unwrap();
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("missing".into(), params.clone())),
            "idempotency_pending"
        );
        let second = app.state.workspaces[0].test_add_tab(None);
        app.state.ensure_test_terminals();
        app.state.workspaces[0].tabs[second].layout_effect_nonce = Some(nonce.clone());
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("wrong-tab".into(), params.clone())),
            "idempotency_pending"
        );
        app.state.workspaces[0].tabs[0].layout_effect_nonce = Some(nonce);
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("duplicate".into(), params)),
            "idempotency_pending"
        );
        app.state.assert_invariants_for_test();
    });
}

#[cfg(unix)]
#[test]
fn cancellation_survives_epoch_bound_handoff_initialization() {
    with_test_config_home("cancel-handoff", |_| {
        let mut source = persistent_empty_app();
        let params = idempotent_layout_params(None, "handoff-cancel", "late");
        assert_eq!(
            error_code(&source.handle_layout_cancel_idempotent("cancel".into(), params.clone())),
            "idempotency_no_effect"
        );
        let snapshot = crate::persist::load_checked().unwrap().unwrap();
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut imported = App::new_from_handoff(
            &Config::default(),
            None,
            rx,
            crate::api::EventHub::default(),
            &snapshot,
            &mut std::collections::HashMap::new(),
        )
        .unwrap();
        assert_eq!(imported.layout_apply_epoch, source.layout_apply_epoch);
        assert!(
            imported.layout_apply_receipts.is_empty(),
            "no ledger write before ownership commit"
        );
        imported.assume_handoff_ownership();
        imported.initialize_layout_apply_idempotency_after_handoff(Some(
            snapshot.idempotency_epoch.as_deref(),
        ));
        add_workspace(&mut imported);
        assert_eq!(
            error_code(&imported.handle_layout_apply_idempotent("late".into(), params, false)),
            "idempotency_no_effect"
        );
        assert_eq!(imported.state.workspaces[0].tabs.len(), 1);
        assert!(imported.terminal_runtimes.is_empty());
    });
}

#[test]
fn epoch_mismatch_and_missing_snapshot_cannot_reopen_spent_keys() {
    with_test_config_home("epoch-mismatch", |_| {
        let mut app = persistent_empty_app();
        let params = idempotent_layout_params(None, "cancelled", "late");
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("cancel".into(), params.clone())),
            "idempotency_no_effect"
        );
        let before =
            std::fs::read(crate::session::data_dir().join("api-idempotency.json")).unwrap();
        app.initialize_layout_apply_idempotency(Some(Some(&"ff".repeat(16))));
        assert_eq!(
            error_code(&app.handle_layout_cancel_idempotent("mismatch".into(), params.clone())),
            "idempotency_unavailable"
        );
        std::fs::remove_file(crate::session::data_dir().join("session.json")).unwrap();
        let mut restarted = persistent_empty_app();
        assert_eq!(
            error_code(&restarted.handle_layout_apply_idempotent("missing".into(), params, false)),
            "idempotency_unavailable"
        );
        assert_eq!(
            std::fs::read(crate::session::data_dir().join("api-idempotency.json")).unwrap(),
            before
        );
    });
}
