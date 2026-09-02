use crate::api::schema::LayoutDescription;
use crate::app::App;
use crate::persist::{
    LayoutApplyLedger, LayoutApplyOutcome, LayoutApplyReceipt, LayoutApplyReceipts, SessionLoad,
};

pub(super) enum PendingResolution {
    Committed(Box<LayoutDescription>),
    Ambiguous(String),
}

impl App {
    pub(crate) fn initialize_layout_apply_idempotency(
        &mut self,
        restored_epoch: Option<Option<&str>>,
    ) {
        self.initialize_layout_apply_idempotency_inner(restored_epoch, true);
    }

    #[cfg(unix)]
    pub(crate) fn initialize_layout_apply_idempotency_after_handoff(
        &mut self,
        restored_epoch: Option<Option<&str>>,
    ) {
        self.initialize_layout_apply_idempotency_inner(restored_epoch, false);
    }

    fn initialize_layout_apply_idempotency_inner(
        &mut self,
        restored_epoch: Option<Option<&str>>,
        quarantine_on_reconcile_failure: bool,
    ) {
        if self.no_session {
            return;
        }

        let mut ledger = match crate::persist::load_layout_apply_ledger() {
            Ok(ledger) => ledger,
            Err(err) => {
                self.layout_apply_receipts.clear();
                self.mark_layout_apply_idempotency_unavailable(format!(
                    "failed to load layout idempotency receipts: {err}"
                ));
                return;
            }
        };

        let restored_epoch = if let Some(epoch) = restored_epoch {
            Some(epoch.map(str::to_owned))
        } else {
            match crate::persist::load() {
                SessionLoad::Loaded(snapshot) => Some(snapshot.idempotency_epoch),
                SessionLoad::Missing => None,
                SessionLoad::Unsupported { .. } => {
                    self.mark_layout_apply_idempotency_unavailable(
                        "layout idempotency cannot inspect an unsupported session snapshot".into(),
                    );
                    return;
                }
            }
        };

        match restored_epoch {
            Some(Some(epoch)) if ledger.receipts.is_empty() => ledger.session_epoch = epoch,
            Some(Some(epoch)) if epoch == ledger.session_epoch => {}
            Some(Some(_)) => {
                self.mark_layout_apply_idempotency_unavailable(
                    "layout idempotency ledger belongs to a different session epoch".into(),
                );
                return;
            }
            Some(None) if ledger.receipts.is_empty() => {}
            Some(None) => {
                self.mark_layout_apply_idempotency_unavailable(
                    "layout idempotency receipts are not bound to the restored session".into(),
                );
                return;
            }
            None if ledger.receipts.is_empty() => {}
            None => {
                self.mark_layout_apply_idempotency_unavailable(
                    "layout idempotency receipts exist without a durable session".into(),
                );
                return;
            }
        }

        self.layout_apply_epoch = ledger.session_epoch;
        self.layout_apply_receipts = ledger.receipts;
        self.layout_apply_receipts_error = None;
        if let Err(err) =
            self.reconcile_pending_layout_apply_receipts(quarantine_on_reconcile_failure)
        {
            self.mark_layout_apply_idempotency_unavailable(err);
            if !quarantine_on_reconcile_failure {
                self.state.session_dirty = true;
                self.sync_session_save_schedule();
            }
        }
    }

    pub(super) fn layout_apply_request_digest(
        &self,
        params: &crate::api::schema::LayoutApplyParams,
    ) -> Result<String, String> {
        crate::persist::layout_apply_request_digest(params)
    }

    pub(super) fn new_layout_effect_nonce(&self) -> Result<String, String> {
        crate::persist::new_layout_effect_nonce()
    }

    pub(super) fn validate_layout_idempotency_key(&self, key: &str) -> Result<(), String> {
        crate::persist::validate_layout_idempotency_key(key)
    }

    pub(super) fn layout_apply_receipt(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<LayoutApplyReceipt>, String> {
        if let Some(err) = &self.layout_apply_receipts_error {
            return Err(err.clone());
        }
        let receipt = self.layout_apply_receipts.get(idempotency_key).cloned();
        if receipt
            .as_ref()
            .is_some_and(|receipt| receipt.session_epoch != self.layout_apply_epoch)
        {
            return Err("layout idempotency receipt belongs to a different session epoch".into());
        }
        Ok(receipt)
    }

    pub(super) fn store_layout_apply_receipt(
        &mut self,
        idempotency_key: String,
        receipt: LayoutApplyReceipt,
    ) -> Result<(), String> {
        self.validate_layout_idempotency_key(&idempotency_key)?;
        if receipt.session_epoch != self.layout_apply_epoch {
            return Err("layout idempotency receipt belongs to a different session epoch".into());
        }
        if !self.layout_apply_receipts.contains_key(&idempotency_key)
            && self.layout_apply_receipts.len() >= crate::persist::MAX_LAYOUT_IDEMPOTENCY_RECEIPTS
        {
            return Err("layout idempotency receipt capacity is exhausted".into());
        }
        let mut candidate = self.layout_apply_receipts.clone();
        candidate.insert(idempotency_key, receipt);
        self.store_layout_apply_receipts(candidate)
    }

    fn store_layout_apply_receipts(
        &mut self,
        candidate: LayoutApplyReceipts,
    ) -> Result<(), String> {
        if let Some(err) = &self.layout_apply_receipts_error {
            return Err(err.clone());
        }
        if !self.no_session {
            crate::persist::save_layout_apply_ledger(&LayoutApplyLedger {
                session_epoch: self.layout_apply_epoch.clone(),
                receipts: candidate.clone(),
            })
            .map_err(|err| err.to_string())?;
        }
        self.layout_apply_receipts = candidate;
        Ok(())
    }

    pub(crate) fn mark_layout_apply_idempotency_unavailable(&mut self, err: String) {
        tracing::warn!(err = %err, "layout idempotency is unavailable");
        self.layout_apply_receipts_error = Some(err);
    }

    pub(crate) fn reset_layout_apply_idempotency_for_session_clear(&mut self) {
        self.layout_apply_receipts.clear();
        self.layout_apply_receipts_error = None;
        match crate::persist::new_layout_session_epoch() {
            Ok(epoch) => self.layout_apply_epoch = epoch,
            Err(err) => self.mark_layout_apply_idempotency_unavailable(format!(
                "failed to rotate layout idempotency session epoch: {err}"
            )),
        }
    }

    pub(crate) fn save_layout_apply_session_snapshot_now(&mut self) -> Result<(), String> {
        let previous_save_deadline = self.session_save_deadline;
        self.join_background_session_save();
        self.session_save_deadline = None;
        if self.no_session {
            return Ok(());
        }
        if self.session_persistence_blocked {
            if previous_save_deadline.is_some() {
                self.session_save_deadline = previous_save_deadline;
            }
            return Err("session persistence is blocked by an unsupported snapshot".into());
        }

        let mut snapshot = crate::persist::capture(
            &self.state.workspaces,
            &self.state.terminals,
            &self.terminal_runtimes,
            self.state.active,
            self.state.selected,
            self.state.sidebar_width,
            self.state.sidebar_section_split,
            self.state.collapsed_space_keys.clone(),
        );
        snapshot.idempotency_epoch = Some(self.layout_apply_epoch.clone());
        let history = self.persist_pane_history.then(|| {
            crate::persist::capture_history(
                &self.state.workspaces,
                &self.terminal_runtimes,
                snapshot.version,
            )
        });
        let result =
            crate::persist::save_layout_apply_session_snapshot(&snapshot, history.as_ref())
                .map_err(|err| err.to_string());
        if result.is_err() && previous_save_deadline.is_some() {
            self.session_save_deadline = previous_save_deadline;
        }
        result
    }

    pub(super) fn quarantine_layout_apply_after_effect(&mut self, message: String) -> String {
        self.layout_apply_quarantined = true;
        self.state.should_quit = true;
        self.state.session_dirty = false;
        self.session_save_deadline = None;
        self.api_rx.close();
        while let Ok(msg) = self.api_rx.try_recv() {
            let response = super::responses::encode_error(
                msg.request.id,
                "server_unavailable",
                "server is shutting down after an ambiguous layout persistence failure",
            );
            let _ = msg.respond_to.send(response);
        }

        let preserve_error = self.save_layout_apply_session_snapshot_now().err();
        self.no_session = true;
        let message = if let Some(err) = preserve_error {
            format!("{message}; failed to preserve the current session snapshot: {err}")
        } else {
            message
        };
        self.mark_layout_apply_idempotency_unavailable(message.clone());
        message
    }

    pub(super) fn reconcile_layout_apply_receipt(
        &self,
        receipt: &LayoutApplyReceipt,
    ) -> PendingResolution {
        if receipt.session_epoch != self.layout_apply_epoch {
            return PendingResolution::Ambiguous(
                "layout idempotency receipt belongs to a different session epoch".into(),
            );
        }
        let expected_tab_id = receipt.outcome.expected_tab_id();
        self.layout_for_effect_nonce(&receipt.effect_nonce, expected_tab_id)
    }

    pub(super) fn replay_committed_layout_apply_receipt(
        &self,
        receipt: &LayoutApplyReceipt,
    ) -> PendingResolution {
        let LayoutApplyOutcome::Committed { tab_id } = &receipt.outcome else {
            return PendingResolution::Ambiguous(
                "layout idempotency receipt is not committed".into(),
            );
        };
        self.layout_for_effect_nonce(&receipt.effect_nonce, Some(tab_id))
    }

    fn layout_for_effect_nonce(
        &self,
        effect_nonce: &str,
        expected_tab_id: Option<&str>,
    ) -> PendingResolution {
        let mut matched = None;
        for (ws_idx, workspace) in self.state.workspaces.iter().enumerate() {
            for (tab_idx, tab) in workspace.tabs.iter().enumerate() {
                if tab.layout_effect_nonce.as_deref() != Some(effect_nonce) {
                    continue;
                }
                if matched.is_some() {
                    return PendingResolution::Ambiguous(
                        "layout effect nonce is attached to more than one live tab".into(),
                    );
                }
                matched = Some((ws_idx, tab_idx));
            }
        }

        let Some((ws_idx, tab_idx)) = matched else {
            return PendingResolution::Ambiguous(
                "the durable layout effect nonce is not present in the live session".into(),
            );
        };
        let Some(layout) = self.layout_description(ws_idx, tab_idx) else {
            return PendingResolution::Ambiguous(
                "the durable layout effect has no live layout description".into(),
            );
        };
        if expected_tab_id.is_some_and(|expected| expected != layout.tab_id) {
            return PendingResolution::Ambiguous(
                "the durable layout effect nonce is attached to a different tab identity".into(),
            );
        }
        PendingResolution::Committed(Box::new(layout))
    }

    fn reconcile_pending_layout_apply_receipts(
        &mut self,
        quarantine_on_failure: bool,
    ) -> Result<(), String> {
        let mut candidate = self.layout_apply_receipts.clone();
        let mut changed = false;

        for (key, receipt) in &mut candidate {
            if !matches!(receipt.outcome, LayoutApplyOutcome::Pending { .. }) {
                continue;
            }
            match self.reconcile_layout_apply_receipt(receipt) {
                PendingResolution::Committed(layout) => {
                    let layout = *layout;
                    receipt.outcome = LayoutApplyOutcome::Committed {
                        tab_id: layout.tab_id,
                    };
                    changed = true;
                }
                PendingResolution::Ambiguous(err) => {
                    tracing::warn!(idempotency_key = %key, err = %err, "leaving layout idempotency receipt pending");
                }
            }
        }

        if !changed {
            return Ok(());
        }
        if let Err(err) = self.store_layout_apply_receipts(candidate) {
            let message =
                format!("failed to persist reconciled layout idempotency receipts: {err}");
            return if quarantine_on_failure {
                Err(self.quarantine_layout_apply_after_effect(message))
            } else {
                Err(message)
            };
        }
        Ok(())
    }
}
