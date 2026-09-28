use super::*;

const LIVE_HANDOFF_RESPONSE_WRITE_TIMEOUT: Duration = Duration::from_secs(6);

pub(super) fn wait_for_live_handoff_response_write(
    response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
) {
    let Some(response_write_complete) = response_write_complete else {
        return;
    };

    match response_write_complete.recv_timeout(LIVE_HANDOFF_RESPONSE_WRITE_TIMEOUT) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            warn!("timed out waiting for live handoff response write; old server exiting");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            warn!("live handoff response writer disconnected; old server exiting");
        }
    }
}

impl HeadlessServer {
    #[cfg(unix)]
    pub(super) fn perform_live_handoff(
        &mut self,
        params: crate::api::schema::ServerLiveHandoffParams,
        guarded_method: bool,
        pull: bool,
    ) -> io::Result<()> {
        info!(guarded = guarded_method, pull, "starting live handoff");
        let has_source_guard =
            params.expected_source_pid.is_some() || params.expected_socket_inode.is_some();
        if guarded_method && !has_source_guard {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "server.live_handoff_guarded needs expected_source_pid or expected_socket_inode",
            ));
        }
        // A request that names its source also requires a replacement that can
        // prove which sockets it binds, whichever method carried it.
        let guarded = guarded_method || has_source_guard;
        let own_socket_inode = self
            .api_server
            .as_ref()
            .map(|server| server.identity().inode());
        let public_socket_inode = socket_file_identity(&api::socket_path())
            .ok()
            .map(|identity| identity.inode());
        crate::server::handoff::check_expected_source(
            params.expected_source_pid,
            params.expected_socket_inode,
            std::process::id(),
            own_socket_inode,
            public_socket_inode,
        )?;
        let import_exe = params.import_exe.as_deref().map(std::path::PathBuf::from);
        let socket_path = crate::server::handoff::handoff_socket_path();
        // A pull handoff goes to the importer that sent the request: it
        // connects with its own token, and no replacement is spawned here.
        let token = if pull {
            match params.import_token.as_deref().map(str::trim) {
                Some(token) if !token.is_empty() => token.to_string(),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "server.live_handoff_pull needs import_token",
                    ))
                }
            }
        } else {
            format!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            )
        };
        let listener = match crate::server::handoff::bind_listener(&socket_path) {
            Ok(listener) => listener,
            Err(err) => {
                self.handoff_in_progress = false;
                return Err(err);
            }
        };

        let mut pane_by_terminal = HashMap::new();
        for ws in &self.app.state.workspaces {
            for tab in &ws.tabs {
                for (pane_id, pane) in &tab.panes {
                    pane_by_terminal.insert(pane.attached_terminal_id.clone(), pane_id.raw());
                }
            }
        }

        self.handoff_in_progress = true;
        self.disconnect_all_clients_for_handoff();
        let _ = reject_pending_client_connections(&self.client_listener);

        let mut paused_terminal_ids = Vec::new();
        for terminal_id in pane_by_terminal.keys() {
            if let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) {
                if let Err(err) = runtime.pause_handoff_reader(Duration::from_secs(2)) {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                    return Err(err);
                }
                paused_terminal_ids.push(terminal_id.clone());
            }
        }

        let snapshot = crate::persist::capture(
            &self.app.state.workspaces,
            &self.app.state.terminals,
            &self.app.terminal_runtimes,
            self.app.state.active,
            self.app.state.selected,
        );

        let mut handoff_entries = Vec::new();
        for (terminal_id, runtime) in self.app.terminal_runtimes.iter() {
            let Some(pane_id) = pane_by_terminal.get(terminal_id).copied() else {
                continue;
            };
            let mut handoff_runtime = runtime.handoff_runtime_state(pane_id);
            handoff_runtime.agent_state = self
                .app
                .state
                .terminals
                .get(terminal_id)
                .and_then(|terminal| terminal.handoff_agent_state());
            let has_agent_session = self
                .app
                .state
                .terminals
                .get(terminal_id)
                .is_some_and(|terminal| terminal.persisted_agent_session.is_some());
            if !has_agent_session {
                handoff_runtime.initial_history_ansi = runtime.handoff_history_ansi();
            }
            handoff_entries.push((terminal_id.clone(), handoff_runtime));
        }

        let panes = handoff_entries
            .iter()
            .map(|(_, runtime)| runtime.clone())
            .collect();
        let manifest = crate::server::handoff::manifest_for(
            snapshot,
            panes,
            params.expected_protocol,
            params.expected_version,
            self.api_window_title.clone(),
        );
        let mut import_child = if pull {
            info!(socket = %socket_path.display(), "waiting for pulling handoff importer");
            None
        } else {
            match crate::server::handoff::spawn_handoff_import(
                import_exe.as_deref(),
                &socket_path,
                &token,
            ) {
                Ok(child) => {
                    info!(pid = child.id(), socket = %socket_path.display(), "spawned handoff import server");
                    Some(child)
                }
                Err(err) => {
                    self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                    return Err(err);
                }
            }
        };

        let mut fds = Vec::new();
        let duplicate_result = (|| {
            for (terminal_id, _) in &handoff_entries {
                let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) else {
                    continue;
                };
                fds.push(runtime.duplicate_handoff_fd()?);
            }
            Ok::<(), io::Error>(())
        })();
        if let Err(err) = duplicate_result {
            for fd in fds {
                let _ = unsafe { libc::close(fd) };
            }
            if let Some(child) = import_child.as_mut() {
                crate::server::handoff::cleanup_failed_import_child(child);
            }
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(err);
        }

        let (mut stream, replacement) = match crate::server::handoff::accept_and_validate_on(
            listener,
            &socket_path,
            &token,
            &manifest,
        ) {
            Ok(accepted) => accepted,
            Err(err) => {
                for fd in fds {
                    let _ = unsafe { libc::close(fd) };
                }
                // A pulling importer that connected gets EOF and exits: it has
                // bound nothing yet.
                if let Some(child) = import_child.as_mut() {
                    crate::server::handoff::cleanup_failed_import_child(child);
                }
                self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
                return Err(err);
            }
        };

        // Taken while the importer is still connected: a numeric peer pid
        // could name another process by the time a rollback signals it.
        let peer = if pull {
            crate::platform::local_socket_peer_process(std::os::fd::AsRawFd::as_raw_fd(&stream))
        } else {
            None
        };
        let mut bound = crate::server::handoff::BoundSockets::default();
        if guarded && !replacement.reports_bound_sockets {
            // Never downgrade a guarded handoff: without socket reports a failed
            // replacement's socket files cannot be told apart from another
            // server's. Nothing has been handed over or moved yet.
            for fd in fds {
                let _ = unsafe { libc::close(fd) };
            }
            crate::server::handoff::stop_failed_replacement(
                import_child.as_mut(),
                peer.as_ref(),
                &mut stream,
                &mut bound,
            );
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "refusing guarded handoff: the replacement server does not support guarded handoff (it does not report the sockets it binds); no handoff was performed",
            ));
        }

        let send_result = crate::server::handoff::send_fds_and_wait_restored(&mut stream, &fds);
        for fd in fds {
            let _ = unsafe { libc::close(fd) };
        }
        if let Err(err) = send_result {
            crate::server::handoff::stop_failed_replacement(
                import_child.as_mut(),
                peer.as_ref(),
                &mut stream,
                &mut bound,
            );
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(err);
        }

        // Free the public paths for the replacement without closing or
        // unlinking the old listeners: each socket file is renamed aside and
        // renamed back if the handoff does not commit.
        let parked = self.park_public_sockets_for_handoff();
        if let Err(err) = crate::server::handoff::wait_ready(&mut stream, &mut bound) {
            // Stop the replacement for certain before touching the public paths.
            crate::server::handoff::stop_failed_replacement(
                import_child.as_mut(),
                peer.as_ref(),
                &mut stream,
                &mut bound,
            );
            let restored = self.restore_public_sockets_after_failed_handoff(parked, &bound);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(match restored {
                Ok(()) => io::Error::other(format!(
                    "handoff replacement server did not become ready: {err}"
                )),
                Err(restore_err) => io::Error::other(format!(
                    "handoff replacement server did not become ready: {err}; old server could not restore public sockets: {restore_err}"
                )),
            });
        }
        if let Err(err) = crate::server::handoff::report_committed(&mut stream) {
            crate::server::handoff::stop_failed_replacement(
                import_child.as_mut(),
                peer.as_ref(),
                &mut stream,
                &mut bound,
            );
            let restored = self.restore_public_sockets_after_failed_handoff(parked, &bound);
            self.rollback_handoff_before_commit(&socket_path, &paused_terminal_ids);
            return Err(match restored {
                Ok(()) => err,
                Err(restore_err) => io::Error::other(format!(
                    "handoff replacement server was ready, but commit failed: {err}; old server could not restore public sockets: {restore_err}"
                )),
            });
        }
        // Committed: the replacement owns the public paths now. Drop only our
        // own parked files; anything else at those names is left alone.
        for socket in &parked {
            let _ = remove_socket_file_if_owned(&socket.parked, &socket.identity);
        }

        for (terminal_id, runtime) in self.app.terminal_runtimes.drain_for_handoff() {
            if !pane_by_terminal.contains_key(&terminal_id) {
                continue;
            }
            debug!(terminal = %terminal_id, "preserving pane runtime for handoff");
            runtime.preserve_for_handoff();
        }
        crate::server::handoff::wait_owned_ack(&mut stream);

        Ok(())
    }

    pub(super) fn finish_live_handoff_shutdown(&mut self) {
        self.shutting_down = true;
        self.app.state.should_quit = true;
        self.app.policy.persist_session = false;
        info!("live handoff completed; old server exiting");
    }

    #[cfg(not(unix))]
    pub(super) fn perform_live_handoff(
        &mut self,
        _params: crate::api::schema::ServerLiveHandoffParams,
        _guarded_method: bool,
        _pull: bool,
    ) -> io::Result<()> {
        Err(io::Error::other("live handoff is only supported on Unix"))
    }

    /// Moves the old server's public socket files aside so the replacement can
    /// bind the public paths, while the old listeners stay open.
    #[cfg(unix)]
    fn park_public_sockets_for_handoff(&mut self) -> Vec<ParkedSocket> {
        let mut parked = Vec::new();
        match &self.api_server {
            Some(api_server) => {
                if let Some(socket) = park_public_socket(
                    BoundSocketKind::Api,
                    api_server.path(),
                    api_server.identity(),
                ) {
                    parked.push(socket);
                }
            }
            None => {
                let _ = std::fs::remove_file(crate::api::socket_path());
            }
        }
        if let Some(socket) = park_public_socket(
            BoundSocketKind::Client,
            &self.client_socket_path,
            &self.client_socket_identity,
        ) {
            parked.push(socket);
        }
        parked
    }

    /// Returns the public paths to the old server's still-open listeners after
    /// a handoff that did not commit. The replacement must already be stopped.
    ///
    /// Only a socket file the replacement reported binding is removed. Anything
    /// else at a public path, such as another server's socket, is left alone.
    /// If a listener cannot be put back at its public path, the failure is
    /// logged as an error and a fresh listener is bound at `<socket>.recover`,
    /// so the old server, which still owns every pane, stays reachable.
    #[cfg(unix)]
    fn restore_public_sockets_after_failed_handoff(
        &mut self,
        parked: Vec<ParkedSocket>,
        bound: &crate::server::handoff::BoundSockets,
    ) -> io::Result<()> {
        let mut failures = Vec::new();
        for socket in parked {
            let restored = unpark_public_socket(&socket, bound.get(socket.kind));
            let Err(err) = restored else {
                info!(path = %socket.public.display(), "restored public socket after failed handoff");
                continue;
            };
            let recover = recovery_socket_path(&socket.public);
            tracing::error!(
                path = %socket.public.display(),
                recover = %recover.display(),
                err = %err,
                "OLD SERVER LOST ITS PUBLIC SOCKET after failed handoff; binding recovery socket"
            );
            let recovered = match socket.kind {
                BoundSocketKind::Api => self.bind_api_recovery_socket(recover.clone()),
                BoundSocketKind::Client => self.bind_client_recovery_socket(recover.clone()),
            };
            if recovered.is_ok() {
                // The parked file's listener was replaced by the recovery one.
                let _ = remove_socket_file_if_owned(&socket.parked, &socket.identity);
            }
            match recovered {
                Ok(()) => failures.push(format!(
                    "{} not restored ({err}); server reachable at {}",
                    socket.public.display(),
                    recover.display()
                )),
                Err(recover_err) => {
                    tracing::error!(
                        path = %socket.public.display(),
                        recover = %recover.display(),
                        err = %recover_err,
                        "OLD SERVER COULD NOT BIND RECOVERY SOCKET; it has no listener at this path"
                    );
                    failures.push(format!(
                        "{} not restored ({err}); recovery socket {} failed: {recover_err}",
                        socket.public.display(),
                        recover.display()
                    ));
                }
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(failures.join("; ")))
        }
    }

    #[cfg(unix)]
    fn bind_api_recovery_socket(&mut self, path: PathBuf) -> io::Result<()> {
        let api_tx = self
            .api_tx
            .clone()
            .ok_or_else(|| io::Error::other("cannot restore api socket without api sender"))?;
        let api_server = api::start_server_at_with_stop_control(
            path,
            api_tx,
            self.app.event_hub.clone(),
            self.should_quit.clone(),
        )?;
        self.api_server = Some(api_server);
        Ok(())
    }

    #[cfg(unix)]
    fn bind_client_recovery_socket(&mut self, path: PathBuf) -> io::Result<()> {
        prepare_socket_path(&path)?;
        let listener = bind_local_listener(&path)?;
        restrict_socket_permissions(&path)?;
        let identity = socket_file_identity(&path)?;
        listener.set_nonblocking(ListenerNonblockingMode::Accept)?;
        self.client_listener = listener;
        self.client_socket_path = path;
        self.client_socket_identity = identity;
        Ok(())
    }

    #[cfg(unix)]
    fn rollback_handoff_before_commit(
        &mut self,
        socket_path: &Path,
        paused_terminal_ids: &[crate::terminal::TerminalId],
    ) {
        for terminal_id in paused_terminal_ids {
            if let Some(runtime) = self.app.terminal_runtimes.get(terminal_id) {
                runtime.set_handoff_reader_paused(false);
            }
        }
        self.handoff_in_progress = false;
        let _ = std::fs::remove_file(socket_path);
    }

    #[cfg(unix)]
    pub(super) fn nudge_handoff_panes_on_first_client_attach(&mut self) {
        if !self.pending_handoff_repaint_nudge {
            return;
        }
        self.pending_handoff_repaint_nudge = false;
        self.app
            .terminal_runtimes
            .nudge_child_redraw_after_handoff();
    }

    #[cfg(not(unix))]
    pub(super) fn nudge_handoff_panes_on_first_client_attach(&mut self) {}
    /// Initiates graceful shutdown.
    pub(super) fn initiate_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        info!("server shutdown initiated");
        self.shutting_down = true;

        // Clear client-local host graphics, then send ServerShutdown to all connected clients.
        let shutdown_msg = ServerMessage::ServerShutdown {
            reason: Some("server is shutting down".to_owned()),
        };
        self.send_to_all_clients(shutdown_msg);

        // Give client writer threads a moment to flush the shutdown message.
        // A short sleep ensures the message is written to the socket before
        // we close the connections.
        std::thread::sleep(Duration::from_millis(50));

        // Signal the main loop to exit.
        self.should_quit.store(true, Ordering::Release);
        self.app.state.should_quit = true;
    }

    /// Completes the shutdown sequence: send ServerShutdown to clients,
    /// close client connections, remove socket files, and clean up.
    pub(super) async fn complete_shutdown(&mut self) -> io::Result<()> {
        info!("completing server shutdown");
        self.reject_late_client_connections().await;

        // Send ServerShutdown to all remaining clients.
        if !self.clients.is_empty() {
            let shutdown_msg = ServerMessage::ServerShutdown {
                reason: Some("server is shutting down".to_owned()),
            };
            self.send_to_all_clients(shutdown_msg);

            // Give writer threads a moment to flush before closing.
            std::thread::sleep(Duration::from_millis(50));
        }

        // Reject only the requests already queued when shutdown reached cleanup.
        self.reject_queued_api_requests_for_shutdown();

        // Close all client connections.
        let staged_files = self
            .clients
            .drain()
            .flat_map(|(_, client)| client.staged_clipboard_files)
            .collect::<Vec<_>>();
        crate::server::clipboard_image::remove_files(staged_files);

        // Remove socket files.
        self.cleanup_sockets()?;

        Ok(())
    }

    /// Removes socket files created by the server.
    pub(super) fn cleanup_sockets(&self) -> io::Result<()> {
        if let Err(err) =
            remove_socket_file_if_owned(&self.client_socket_path, &self.client_socket_identity)
        {
            if err.kind() != io::ErrorKind::NotFound {
                warn!(
                    path = %self.client_socket_path.display(),
                    err = %err,
                    "failed to remove client socket on shutdown"
                );
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
use crate::server::handoff::BoundSocketKind;

/// An old server's public socket file moved aside during a handoff. Its
/// listener stays open, so renaming the file back restores the same socket.
#[cfg(unix)]
struct ParkedSocket {
    kind: BoundSocketKind,
    public: PathBuf,
    parked: PathBuf,
    identity: SocketFileIdentity,
}

#[cfg(unix)]
fn sibling_socket_path(public: &Path, suffix: &str) -> PathBuf {
    let mut path = public.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

#[cfg(unix)]
fn recovery_socket_path(public: &Path) -> PathBuf {
    sibling_socket_path(public, ".recover")
}

#[cfg(unix)]
fn park_public_socket(
    kind: BoundSocketKind,
    public: &Path,
    identity: &SocketFileIdentity,
) -> Option<ParkedSocket> {
    match socket_file_identity(public) {
        Ok(current) if current == *identity => {}
        Ok(_) => {
            warn!(path = %public.display(), "public socket is not ours; not parking it for handoff");
            return None;
        }
        Err(err) => {
            warn!(path = %public.display(), err = %err, "public socket missing; not parking it for handoff");
            return None;
        }
    }
    let parked = match move_socket_aside(public, "handoff") {
        Ok(parked) => parked,
        Err(err) => {
            warn!(path = %public.display(), err = %err, "failed to park public socket for handoff");
            return None;
        }
    };
    if socket_file_identity(&parked).ok().as_ref() != Some(identity) {
        // Another socket took the public path between the check and the move.
        // Put it back; if that is impossible, keep it where it is.
        return match put_back_foreign_socket(&parked, public) {
            Ok(()) => {
                warn!(path = %public.display(), "public socket is not ours; not parking it for handoff");
                None
            }
            Err(err) => {
                warn!(path = %public.display(), err = %err, "public socket changed while parking it for handoff");
                None
            }
        };
    }
    Some(ParkedSocket {
        kind,
        public: public.to_path_buf(),
        parked,
        identity: identity.clone(),
    })
}

/// Puts the old server's own socket file back at its public path.
///
/// Whatever is at the public path is removed only if it is the socket file the
/// stopped replacement reported binding (`replacement`). Anything else, for
/// example a server started independently while the path was free, is left in
/// place and reported as a conflict. Neither step can clobber a competing bind:
/// the replacement's file is moved aside and checked before it is unlinked, and
/// the old file is linked back with no-replace semantics.
#[cfg(unix)]
fn unpark_public_socket(
    socket: &ParkedSocket,
    replacement: Option<&SocketFileIdentity>,
) -> io::Result<()> {
    match socket_file_identity(&socket.public) {
        Ok(current) if current == socket.identity => return Ok(()),
        Ok(current) => {
            if replacement != Some(&current) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!(
                        "{} is held by a socket that is not the failed replacement's (inode {}); left in place",
                        socket.public.display(),
                        current.inode()
                    ),
                ));
            }
            remove_replacement_socket(&socket.public, &current)?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    match socket_file_identity(&socket.parked) {
        Ok(current) if current == socket.identity => {}
        Ok(_) => return Err(io::Error::other("parked socket was replaced")),
        Err(err) => return Err(err),
    }
    link_no_replace(&socket.parked, &socket.public)?;
    let _ = remove_socket_file_if_owned(&socket.parked, &socket.identity);
    if socket_file_identity(&socket.public)? != socket.identity {
        return Err(io::Error::other("public socket changed while restoring"));
    }
    Ok(())
}

/// Removes the failed replacement's socket file at `public`, and only that.
/// The file is first moved to a fresh private name, so a socket bound at the
/// public path in the meantime is never the one unlinked.
#[cfg(unix)]
fn remove_replacement_socket(public: &Path, replacement: &SocketFileIdentity) -> io::Result<()> {
    remove_replacement_socket_with(public, replacement, || {}, || {})
}

/// [`remove_replacement_socket`] with hooks that run just before the move
/// aside and just before a foreign socket is linked back, so tests can race
/// other binds against both steps.
#[cfg(unix)]
fn remove_replacement_socket_with(
    public: &Path,
    replacement: &SocketFileIdentity,
    before_quarantine: impl FnOnce(),
    before_link_back: impl FnOnce(),
) -> io::Result<()> {
    before_quarantine();
    let quarantine = move_socket_aside(public, "failed")?;
    match socket_file_identity(&quarantine) {
        Ok(moved) if moved == *replacement => {
            info!(
                path = %public.display(),
                inode = moved.inode(),
                "removed failed replacement's public socket (the inode it reported binding)"
            );
            std::fs::remove_file(&quarantine)
        }
        _ => {
            // Not the replacement's after all: put it back untouched.
            before_link_back();
            put_back_foreign_socket(&quarantine, public)?;
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} changed owner while restoring; left in place",
                    public.display()
                ),
            ))
        }
    }
}

/// Links a socket file that is not ours from `aside` back to `public`, and
/// drops the `aside` name only once that has worked. If `public` is taken or
/// the link fails, the socket keeps its `aside` name, which is logged and
/// returned in the error so its owner can still be reached.
#[cfg(unix)]
fn put_back_foreign_socket(aside: &Path, public: &Path) -> io::Result<()> {
    let foreign = socket_file_identity(aside).ok();
    if let Err(err) = link_no_replace(aside, public) {
        tracing::error!(
            path = %public.display(),
            kept = %aside.display(),
            err = %err,
            "ANOTHER SERVER'S SOCKET COULD NOT BE PUT BACK at its public path; it is kept at the recovery path"
        );
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "another server's socket was moved off {} and could not be linked back ({err}); it is preserved at {}",
                public.display(),
                aside.display()
            ),
        ));
    }
    // `public` names the foreign socket again; the extra name can go.
    if let Some(foreign) = foreign {
        let _ = remove_socket_file_if_owned(aside, &foreign);
    }
    Ok(())
}

/// Moves the socket file at `public` to a new sibling name that nothing else
/// uses, `<public>.<tag>-<pid>-<n>`, and returns that name. An existing file is
/// never replaced: a taken name is skipped.
#[cfg(unix)]
fn move_socket_aside(public: &Path, tag: &str) -> io::Result<PathBuf> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pid = std::process::id();
    for _ in 0..64 {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let aside = sibling_socket_path(public, &format!(".{tag}-{pid}-{n}"));
        match rename_no_replace(public, &aside) {
            Ok(()) => return Ok(aside),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free name to move {} aside", public.display()),
    ))
}

/// Renames `from` to `to`, failing with `AlreadyExists` instead of replacing
/// anything at `to`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both paths are valid NUL-terminated strings for the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE as libc::c_uint,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_vendor = "apple")]
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both paths are valid NUL-terminated strings for the call.
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_vendor = "apple"))
))]
fn rename_no_replace(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no-replace rename is not available on this platform",
    ))
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn c_path(path: &Path) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(std::ffi::CString::new(path.as_os_str().as_bytes())?)
}

/// Makes `to` name the same socket file as `from` without replacing anything
/// already at `to`.
#[cfg(unix)]
fn link_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::hard_link(from, to)
}

#[cfg(unix)]
pub(super) fn wait_for_old_public_sockets_to_close(timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    let api_socket = api::socket_path();
    let client_socket = client_socket_path();
    while Instant::now() < deadline {
        let api_open = api_socket.exists() && crate::ipc::connect_local_stream(&api_socket).is_ok();
        let client_open =
            client_socket.exists() && crate::ipc::connect_local_stream(&client_socket).is_ok();
        if !api_open && !client_open {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "old server sockets did not close before handoff import bind",
    ))
}

#[cfg(all(test, unix))]
mod socket_restore_tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    fn scratch_dir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("hq-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names_with(dir: &Path, needle: &str) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().contains(needle))
            .collect()
    }

    fn identity(path: &Path) -> SocketFileIdentity {
        socket_file_identity(path).unwrap()
    }

    #[test]
    fn foreign_socket_survives_a_bind_that_takes_the_public_path_before_link_back() {
        let dir = scratch_dir();
        let public = dir.join("s.sock");
        let replacement = UnixListener::bind(&public).unwrap();
        let replacement_identity = identity(&public);

        let mut foreign = None;
        let mut occupier = None;
        let result = remove_replacement_socket_with(
            &public,
            &replacement_identity,
            || {
                // An independent server clears the stale socket and binds
                // after the replacement's socket was inspected.
                std::fs::remove_file(&public).unwrap();
                let listener = UnixListener::bind(&public).unwrap();
                foreign = Some((listener, identity(&public)));
            },
            || {
                // Another startup takes the public path while the foreign
                // socket is aside.
                let listener = UnixListener::bind(&public).unwrap();
                occupier = Some((listener, identity(&public)));
            },
        );
        drop(replacement);
        let (_foreign_listener, foreign_identity) = foreign.unwrap();
        let (_occupier_listener, occupier_identity) = occupier.unwrap();

        let err = result.expect_err("a foreign socket that cannot go back is an error");
        let kept = names_with(&dir, "s.sock.failed-");
        assert_eq!(kept.len(), 1, "foreign socket must keep one aside name");
        let kept = &kept[0];
        assert!(
            err.to_string().contains(&kept.display().to_string()),
            "the error must name where the foreign socket is kept: {err}"
        );
        // Same inode, still reachable: nothing unlinked it.
        assert_eq!(identity(kept), foreign_identity);
        UnixStream::connect(kept).expect("foreign listener reachable at its kept path");
        assert_eq!(identity(&public), occupier_identity);
        UnixStream::connect(&public).expect("occupier still reachable at the public path");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn foreign_socket_goes_back_to_a_free_public_path() {
        let dir = scratch_dir();
        let public = dir.join("s.sock");
        let replacement = UnixListener::bind(&public).unwrap();
        let replacement_identity = identity(&public);

        let mut foreign = None;
        let result = remove_replacement_socket_with(
            &public,
            &replacement_identity,
            || {
                std::fs::remove_file(&public).unwrap();
                let listener = UnixListener::bind(&public).unwrap();
                foreign = Some((listener, identity(&public)));
            },
            || {},
        );
        drop(replacement);
        let (_foreign_listener, foreign_identity) = foreign.unwrap();

        let err = result.expect_err("a changed owner is reported");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(identity(&public), foreign_identity);
        UnixStream::connect(&public).expect("foreign listener reachable at the public path");
        assert!(names_with(&dir, ".failed-").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacement_socket_is_the_only_one_removed() {
        let dir = scratch_dir();
        let public = dir.join("s.sock");
        let _replacement = UnixListener::bind(&public).unwrap();
        let replacement_identity = identity(&public);

        remove_replacement_socket(&public, &replacement_identity).unwrap();
        assert!(!public.exists());
        assert!(names_with(&dir, ".failed-").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn moving_a_socket_aside_never_replaces_an_existing_file() {
        let dir = scratch_dir();
        let from = dir.join("a.sock");
        let to = dir.join("b.sock");
        let _a = UnixListener::bind(&from).unwrap();
        let _b = UnixListener::bind(&to).unwrap();
        let (a, b) = (identity(&from), identity(&to));

        let err = rename_no_replace(&from, &to).expect_err("must not rename over b");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(identity(&from), a);
        assert_eq!(identity(&to), b);

        let first = move_socket_aside(&from, "failed").unwrap();
        assert_eq!(identity(&first), a);
        std::fs::hard_link(&first, &from).unwrap();
        let second = move_socket_aside(&from, "failed").unwrap();
        assert_ne!(first, second, "each move aside gets a fresh name");
        assert_eq!(identity(&first), a);
        assert_eq!(identity(&second), a);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
