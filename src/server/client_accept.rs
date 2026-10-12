use std::io;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::ipc::{LocalListener, LocalStream};
use crate::server::client_transport::{self, ServerEvent};

/// Accepts pending thin-client connections and starts their handshake readers.
pub(crate) fn accept_pending_client_connections(
    listener: &LocalListener,
    next_client_id: &mut u64,
    should_quit: &Arc<AtomicBool>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    if let Err(err) = accept_pending_client_connections_with(
        || listener.accept(),
        next_client_id,
        should_quit,
        server_event_tx,
        false,
    ) {
        // Preserve the synchronous initial drain's log-and-continue behavior.
        error!(err = %err, "client listener accept failed");
    }
    Ok(())
}

/// Shared drain/handshake path for synchronous and reactor-driven accepts.
/// The reactor caller owns error-only retry scheduling; WouldBlock means drained.
pub(crate) fn accept_pending_client_connections_with(
    mut accept: impl FnMut() -> io::Result<LocalStream>,
    next_client_id: &mut u64,
    should_quit: &Arc<AtomicBool>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
    reject: bool,
) -> io::Result<()> {
    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }
        match accept() {
            Ok(stream) => {
                if reject {
                    continue;
                }
                // Pin the original accepted process incarnation exactly once,
                // before scheduling any handshake or reading client data.
                let peer = crate::ipc::local_stream_peer_identity(&stream);
                let client_id = *next_client_id;
                *next_client_id = next_client_id.saturating_add(1);

                if let Err(err) = stream.set_nonblocking(true) {
                    warn!(err = %err, "failed to set client stream nonblocking");
                    continue;
                }

                let should_quit = should_quit.clone();
                let server_event_tx = server_event_tx.clone();
                let spawned = crate::thread_spawn::spawn_named("herdr-client-conn", move || {
                    if let Err(err) = client_transport::handle_client_handshake(
                        stream,
                        client_id,
                        peer,
                        &server_event_tx,
                        &should_quit,
                    ) {
                        debug!(client_id, err = %err, "client handshake failed");
                    }
                });
                if let Err(err) = spawned {
                    warn!(client_id, err = %err, "failed to spawn client connection thread; dropping connection");
                }
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(ref err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

/// Drains pending thin-client connections without starting handshakes.
///
/// During live handoff the old server must not let clients sit in the Unix
/// listener backlog waiting for a welcome frame that will never be sent.
pub(crate) fn reject_pending_client_connections(listener: &LocalListener) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok(_stream) => {}
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "client listener reject failed");
                break;
            }
        }
    }

    Ok(())
}
