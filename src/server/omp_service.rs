//! OMP bridge ownership and route delivery for the headless server.

use std::collections::HashMap;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::SyncSender;

use tokio::sync::mpsc;

use crate::protocol::{
    select_omp_renderer, OmpControlAction, OmpFrameDirection, OmpRendererMode, ServerMessage,
};
use crate::server::client_transport::{OmpHostAdmission, ServerEvent};
use crate::server::clients::{ClientConnection, CommittedIdentity};
use crate::server::omp_bridge;
#[cfg(unix)]
use crate::server::omp_maintenance::OmpMaintenanceHandoffState;
use crate::server::omp_maintenance::{
    OmpMaintenance, OmpMaintenanceAdmissionError, OmpMaintenanceError,
};
use crate::server::omp_route::{OmpRouteDelivery, OmpRouteError, OmpRouteKey, OmpRouteRegistry};

type OmpHostKey = (String, String, u64);
type OmpHostConnection = (u64, SyncSender<String>, TcpStream);

pub(crate) struct OmpService {
    listener: TcpListener,
    bridge: crate::pane::OmpBridgeEnv,
    handshakes: omp_bridge::HandshakeLimiter,
    hosts: HashMap<OmpHostKey, OmpHostConnection>,
    renderer_modes: HashMap<u64, OmpRendererMode>,
    bound_apps: HashMap<u64, u64>,
    route_bindings: HashMap<u64, OmpRouteKey>,
    /// Validated display-only identities for external guest bridge peers.
    external_peer_identities: HashMap<u64, CommittedIdentity>,
    routes: OmpRouteRegistry,
    maintenance: Result<OmpMaintenance, OmpMaintenanceError>,
    pending_maintenance_unregisters: Vec<OmpRouteKey>,
}

impl OmpService {
    pub(crate) fn new(
        prepared: Option<(TcpListener, crate::pane::OmpBridgeEnv)>,
    ) -> io::Result<Self> {
        let session = crate::session::active_name()
            .unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_owned());
        #[cfg(not(test))]
        let maintenance = OmpMaintenance::host(session);
        #[cfg(test)]
        let maintenance = Ok(OmpMaintenance::for_test(
            &session,
            crate::server::omp_maintenance::TestOmpMaintenanceStore::new(),
        ));
        Self::with_maintenance(prepared, maintenance)
    }

    fn with_maintenance(
        prepared: Option<(TcpListener, crate::pane::OmpBridgeEnv)>,
        maintenance: Result<OmpMaintenance, OmpMaintenanceError>,
    ) -> io::Result<Self> {
        if let Err(error) = &maintenance {
            tracing::warn!(
                code = error.code(),
                message = %error.message(),
                "OMP maintenance store is unavailable; OMP host admissions are disabled"
            );
        }
        let (listener, bridge) = match prepared {
            Some(prepared) => prepared,
            None => omp_bridge::bind()?,
        };
        Ok(Self {
            listener,
            bridge,
            handshakes: omp_bridge::handshake_limiter(),
            hosts: HashMap::new(),
            renderer_modes: HashMap::new(),
            bound_apps: HashMap::new(),
            route_bindings: HashMap::new(),
            external_peer_identities: HashMap::new(),
            routes: OmpRouteRegistry::default(),
            maintenance,
            pending_maintenance_unregisters: Vec::new(),
        })
    }

    fn maintenance(&self) -> Result<&OmpMaintenance, OmpMaintenanceError> {
        self.maintenance.as_ref().map_err(Clone::clone)
    }

    #[cfg(test)]
    pub(crate) fn with_test_maintenance(
        prepared: Option<(TcpListener, crate::pane::OmpBridgeEnv)>,
        session: &str,
        store: crate::server::omp_maintenance::TestOmpMaintenanceStore,
    ) -> io::Result<Self> {
        Self::with_maintenance(prepared, Ok(OmpMaintenance::for_test(session, store)))
    }

    pub(crate) fn bridge(&self) -> &crate::pane::OmpBridgeEnv {
        &self.bridge
    }

    pub(crate) fn accept_pending(&self, event_tx: mpsc::Sender<ServerEvent>) {
        omp_bridge::accept_pending(&self.listener, &self.bridge, event_tx, &self.handshakes);
    }

    pub(crate) fn disconnect(
        &mut self,
        client_id: u64,
        clients: &HashMap<u64, ClientConnection>,
    ) -> Vec<(u64, ServerMessage)> {
        let peer_id = self
            .bound_apps
            .get(&client_id)
            .copied()
            .unwrap_or(client_id);
        let sideband_route = (client_id != peer_id)
            .then(|| self.route_bindings.get(&client_id).cloned())
            .flatten();
        let preserve_private_peer = sideband_route.is_some()
            && self.client_is_private_renderer(peer_id)
            && self.route_bindings.get(&peer_id) == sideband_route.as_ref();
        let renderers = self
            .bound_apps
            .iter()
            .filter_map(|(&renderer_id, &app_id)| {
                (renderer_id == client_id || app_id == client_id).then_some(renderer_id)
            })
            .collect::<Vec<_>>();
        for renderer_id in &renderers {
            self.renderer_modes.remove(renderer_id);
            self.bound_apps.remove(renderer_id);
            self.route_bindings.remove(renderer_id);
            self.external_peer_identities.remove(renderer_id);
        }
        if preserve_private_peer {
            return Vec::new();
        }
        let mut messages = Vec::new();
        if let Some(key) = sideband_route {
            if let Some(deliveries) = self.routes.disconnect_from_route(peer_id, &key) {
                if self.send_peer_left(&key, peer_id, &mut messages, clients)
                    && self.sync_authority(&key, &deliveries, &mut messages, clients)
                {
                    self.deliver(&key, deliveries, &mut messages, clients);
                }
            }
            self.routes.remove_if_inactive_and_empty(&key);
            return messages;
        }
        for (key, deliveries) in self.routes.disconnect(peer_id) {
            if self.send_peer_left(&key, peer_id, &mut messages, clients)
                && self.sync_authority(&key, &deliveries, &mut messages, clients)
            {
                self.deliver(&key, deliveries, &mut messages, clients);
            }
            self.routes.remove_if_inactive_and_empty(&key);
        }
        if client_id == peer_id {
            for renderer_id in renderers
                .into_iter()
                .filter(|renderer_id| *renderer_id != client_id)
            {
                messages.push((
                    renderer_id,
                    ServerMessage::ServerShutdown {
                        reason: Some("bound App disconnected".into()),
                    },
                ));
            }
        }
        messages
    }

    pub(crate) fn attach_private_app(
        &mut self,
        client_id: u64,
        key: OmpRouteKey,
        clients: &HashMap<u64, ClientConnection>,
    ) -> Vec<(u64, ServerMessage)> {
        let mut messages = Vec::new();
        let Some(client) = clients.get(&client_id) else {
            return messages;
        };
        if !client.is_full_app_client() || client.committed_identity().is_none() {
            self.send_error(
                &mut messages,
                client_id,
                key.pane_id,
                OmpRouteError::UnknownRoute,
            );
            return messages;
        }
        if self
            .route_bindings
            .get(&client_id)
            .is_some_and(|bound| bound != &key)
        {
            self.send_error(
                &mut messages,
                client_id,
                key.pane_id,
                OmpRouteError::RouteBusy,
            );
            return messages;
        }
        match self.routes.attach(client_id, &key) {
            Ok(deliveries) => {
                self.renderer_modes
                    .insert(client_id, OmpRendererMode::ServerPrivateGuestPty);
                self.bound_apps.insert(client_id, client_id);
                self.route_bindings.insert(client_id, key.clone());
                if self.sync_authority(&key, &deliveries, &mut messages, clients) {
                    self.deliver(&key, deliveries, &mut messages, clients);
                }
            }
            Err(error) => self.send_error(&mut messages, client_id, key.pane_id, error),
        }
        messages
    }
    pub(crate) fn live_route_keys(&self) -> Vec<OmpRouteKey> {
        self.hosts
            .keys()
            .map(|(pane_id, omp_session_id, route_generation)| OmpRouteKey {
                pane_id: pane_id.clone(),
                omp_session_id: omp_session_id.clone(),
                route_generation: *route_generation,
            })
            .collect()
    }

    pub(crate) fn maintenance_status(
        &self,
    ) -> Result<crate::api::schema::ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        self.maintenance()?.status()
    }

    pub(crate) fn inspect_maintenance(
        &self,
    ) -> Result<crate::api::schema::ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        self.maintenance()?.inspect()
    }

    pub(crate) fn route_set_revision(&self) -> Result<u64, OmpMaintenanceError> {
        self.maintenance()?.route_set_revision()
    }

    pub(crate) fn acquire_maintenance(
        &self,
        operation_id: &str,
    ) -> Result<crate::api::schema::ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        self.maintenance()?.acquire(operation_id)
    }

    pub(crate) fn grant_maintenance_permit(
        &self,
        operation_id: &str,
        session: &str,
        pane_id: &str,
    ) -> Result<crate::api::schema::ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        self.maintenance()?
            .grant_permit(operation_id, session, pane_id)
    }

    pub(crate) fn release_maintenance(
        &self,
        operation_id: &str,
    ) -> Result<crate::api::schema::ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        self.maintenance()?.release(operation_id)
    }

    pub(crate) fn enforce_maintenance(
        &mut self,
        clients: &HashMap<u64, ClientConnection>,
    ) -> Vec<(u64, ServerMessage)> {
        self.retry_maintenance_unregisters();
        let local_routes = self.live_route_keys();
        if local_routes.is_empty() {
            return Vec::new();
        }
        let drain = match self
            .maintenance()
            .and_then(|maintenance| maintenance.routes_to_drain(&local_routes))
        {
            Ok(routes) => routes,
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    message = %error.message(),
                    "OMP maintenance state unavailable; draining routes"
                );
                local_routes
            }
        };
        let mut messages = Vec::new();
        for key in drain {
            self.close_host_route(&key, &mut messages, clients);
        }
        messages
    }

    #[cfg(unix)]
    pub(crate) fn maintenance_handoff_state(
        &self,
    ) -> Result<Option<OmpMaintenanceHandoffState>, OmpMaintenanceError> {
        self.maintenance()?.handoff_state()
    }

    #[cfg(unix)]
    pub(crate) fn validate_maintenance_handoff_state(
        &self,
        expected: Option<&OmpMaintenanceHandoffState>,
    ) -> Result<(), OmpMaintenanceError> {
        self.maintenance()?.validate_handoff_state(expected)
    }

    pub(crate) fn detach_private_app(
        &mut self,
        client_id: u64,
        clients: &HashMap<u64, ClientConnection>,
    ) -> Vec<(u64, ServerMessage)> {
        if !self.client_is_private_renderer(client_id) {
            return Vec::new();
        }
        let peer_id = self.bound_apps.remove(&client_id).unwrap_or(client_id);
        self.renderer_modes.remove(&client_id);
        self.route_bindings.remove(&client_id);
        let mut messages = Vec::new();
        for (key, deliveries) in self.routes.disconnect(peer_id) {
            if self.send_peer_left(&key, peer_id, &mut messages, clients)
                && self.sync_authority(&key, &deliveries, &mut messages, clients)
            {
                self.deliver(&key, deliveries, &mut messages, clients);
            }
            self.routes.remove_if_inactive_and_empty(&key);
        }
        messages
    }
    pub(crate) fn retire_private_renderer(&mut self, client_id: u64) {
        if self.client_is_private_renderer(client_id) {
            self.renderer_modes.remove(&client_id);
            self.bound_apps.remove(&client_id);
            self.route_bindings.remove(&client_id);
        }
    }

    /// Handles an OMP-only server event.
    pub(crate) fn handle_event(
        &mut self,
        event: ServerEvent,
        client_is_omp_pane: bool,
        clients: &HashMap<u64, ClientConnection>,
    ) -> Vec<(u64, ServerMessage)> {
        let mut messages = Vec::new();
        match event {
            ServerEvent::OmpPaneAttach {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                target_app_client_id,
                renderer_capabilities,
                renderer_launch_id,
                renderer_request,
                external_peer_identity,
            } => {
                if !valid_route_id(&pane_id) || !valid_route_id(&omp_session_id) {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                if !client_is_omp_pane {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                if renderer_request == crate::protocol::OmpRendererRequest::LegacySharedHostPty {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                let external_guest_bridge =
                    renderer_request == crate::protocol::OmpRendererRequest::ExternalGuestBridge;
                let has_external_identity = external_peer_identity.is_some();
                let external_identity = external_peer_identity.and_then(|identity| {
                    crate::config::validate_display_name(&identity.display_name)
                        .ok()
                        .map(|()| CommittedIdentity {
                            name: identity.display_name,
                            revision: 1,
                        })
                });
                if (external_guest_bridge
                    && (renderer_capabilities.client_local_native
                        || target_app_client_id.is_some()
                        || renderer_launch_id.is_some()
                        || external_identity.is_none()))
                    || (!external_guest_bridge && has_external_identity)
                {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                let renderer_mode =
                    match select_omp_renderer(renderer_request, renderer_capabilities, false) {
                        Ok(renderer_mode) => renderer_mode,
                        Err(_) => {
                            self.send_error(
                                &mut messages,
                                client_id,
                                pane_id,
                                OmpRouteError::UnknownRoute,
                            );
                            return messages;
                        }
                    };
                let key = OmpRouteKey {
                    pane_id: pane_id.clone(),
                    omp_session_id,
                    route_generation,
                };
                if self
                    .route_bindings
                    .get(&client_id)
                    .is_some_and(|bound| bound != &key)
                {
                    self.send_error(&mut messages, client_id, pane_id, OmpRouteError::RouteBusy);
                    return messages;
                }
                if external_guest_bridge
                    && self
                        .external_peer_identities
                        .get(&client_id)
                        .is_some_and(|existing| Some(existing) != external_identity.as_ref())
                {
                    self.send_error(&mut messages, client_id, pane_id, OmpRouteError::RouteBusy);
                    return messages;
                }
                let app_client_id = if external_guest_bridge {
                    client_id
                } else {
                    match self.bind_app_client(
                        client_id,
                        target_app_client_id,
                        renderer_launch_id,
                        &key,
                        clients,
                    ) {
                        Some(app_client_id) => app_client_id,
                        None => {
                            self.send_error(
                                &mut messages,
                                client_id,
                                pane_id,
                                OmpRouteError::UnknownRoute,
                            );
                            return messages;
                        }
                    }
                };
                let attached = if external_guest_bridge {
                    self.routes.attach_observer(client_id, &key)
                } else {
                    self.routes.attach(client_id, &key)
                };
                match attached {
                    Ok(deliveries) => {
                        self.renderer_modes.insert(client_id, renderer_mode);
                        self.bound_apps.insert(client_id, app_client_id);
                        self.route_bindings.insert(client_id, key.clone());
                        if let Some(identity) = external_identity {
                            self.external_peer_identities
                                .entry(client_id)
                                .or_insert(identity);
                        }
                        if self.sync_authority(&key, &deliveries, &mut messages, clients) {
                            self.deliver(&key, deliveries, &mut messages, clients);
                        }
                    }
                    Err(error) => self.send_error(&mut messages, client_id, pane_id, error),
                }
            }
            ServerEvent::OmpPaneDetach {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
            } => {
                if !valid_route_id(&pane_id) || !valid_route_id(&omp_session_id) {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                if !client_is_omp_pane && !self.client_is_private_renderer(client_id) {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                let key = OmpRouteKey {
                    pane_id: pane_id.clone(),
                    omp_session_id,
                    route_generation,
                };
                let Some(peer_id) = self.bound_apps.get(&client_id).copied() else {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                };
                match self.routes.detach(peer_id, &key, attachment_epoch) {
                    Ok(deliveries) => {
                        self.renderer_modes.remove(&client_id);
                        self.bound_apps.remove(&client_id);
                        self.route_bindings.remove(&client_id);
                        self.external_peer_identities.remove(&client_id);
                        if self.send_peer_left(&key, peer_id, &mut messages, clients)
                            && self.sync_authority(&key, &deliveries, &mut messages, clients)
                        {
                            self.deliver(&key, deliveries, &mut messages, clients);
                        }
                        self.routes.remove_if_inactive_and_empty(&key);
                    }
                    Err(error) => self.send_error(&mut messages, client_id, pane_id, error),
                }
            }
            ServerEvent::OmpControl {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
                action,
            } => {
                if !valid_route_id(&pane_id) || !valid_route_id(&omp_session_id) {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                if !client_is_omp_pane && !self.client_is_private_renderer(client_id) {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                if self.renderer_modes.get(&client_id) == Some(&OmpRendererMode::ClientLocalNative)
                    && !self.native_renderer_is_ready(client_id, clients)
                {
                    return messages;
                }
                let key = OmpRouteKey {
                    pane_id: pane_id.clone(),
                    omp_session_id,
                    route_generation,
                };
                let Some(peer_id) = self.bound_apps.get(&client_id).copied() else {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                };
                match self.routes.control(peer_id, &key, attachment_epoch, action) {
                    Ok(deliveries) => {
                        if self.sync_authority(&key, &deliveries, &mut messages, clients) {
                            self.deliver(&key, deliveries, &mut messages, clients);
                        }
                    }
                    Err(error) => self.send_error(&mut messages, client_id, pane_id, error),
                }
            }
            ServerEvent::OmpFrame {
                client_id,
                pane_id,
                omp_session_id,
                route_generation,
                attachment_epoch,
                frame,
            } => {
                if !client_is_omp_pane && !self.client_is_private_renderer(client_id) {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                if self.renderer_modes.get(&client_id) == Some(&OmpRendererMode::ClientLocalNative)
                    && !self.native_renderer_is_ready(client_id, clients)
                {
                    return messages;
                }
                if !valid_route_id(&pane_id) || !valid_route_id(&omp_session_id) {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                }
                let key = OmpRouteKey {
                    pane_id: pane_id.clone(),
                    omp_session_id,
                    route_generation,
                };
                let Some(peer_id) = self.bound_apps.get(&client_id).copied() else {
                    self.send_error(
                        &mut messages,
                        client_id,
                        pane_id,
                        OmpRouteError::UnknownRoute,
                    );
                    return messages;
                };
                match self
                    .routes
                    .guest_frame(peer_id, &key, attachment_epoch, frame)
                {
                    Ok(delivery) => self.deliver(&key, vec![delivery], &mut messages, clients),
                    Err(error) => self.send_error(&mut messages, client_id, pane_id, error),
                }
            }
            ServerEvent::OmpHostStarted {
                pane_id,
                omp_session_id,
                route_generation,
                host_id,
                outbound,
                socket,
                admission,
            } => {
                if !valid_route_id(&pane_id) || !valid_route_id(&omp_session_id) {
                    tracing::warn!("rejected oversized OMP host route identifier");
                    let _ = admission.send(OmpHostAdmission::Rejected {
                        code: OmpRouteError::UnknownRoute.code().to_owned(),
                        message: "OMP host route identifier is invalid".into(),
                    });
                    return messages;
                }
                let announced_key = OmpRouteKey {
                    pane_id,
                    omp_session_id,
                    route_generation,
                };
                let admission_result: Result<
                    OmpRouteKey,
                    OmpMaintenanceAdmissionError<OmpRouteError>,
                > = match self.maintenance.as_ref() {
                    Ok(maintenance) => {
                        let routes = &self.routes;
                        maintenance.admit_replacement(&announced_key, || {
                            let key = routes.prepare_host_start(&announced_key)?;
                            Ok((key.clone(), key))
                        })
                    }
                    Err(error) => Err(OmpMaintenanceAdmissionError::State(error.clone())),
                };
                match admission_result {
                    Ok(key) => {
                        let route_key = (
                            key.pane_id.clone(),
                            key.omp_session_id.clone(),
                            key.route_generation,
                        );
                        let deliveries = self.routes.commit_host_start(key.clone());
                        self.pending_maintenance_unregisters
                            .retain(|pending| pending != &key);
                        self.replace_host(route_key, host_id, outbound, socket);
                        if admission
                            .send(OmpHostAdmission::Accepted {
                                route_generation: key.route_generation,
                            })
                            .is_err()
                        {
                            self.remove_host(&key);
                            if let Ok(deliveries) = self.routes.host_stopped(&key) {
                                self.deliver(&key, deliveries, &mut messages, clients);
                                self.routes.remove_if_inactive_and_empty(&key);
                            }
                            return messages;
                        }
                        if self.sync_authority(&key, &deliveries, &mut messages, clients) {
                            self.deliver(&key, deliveries, &mut messages, clients);
                        }
                    }
                    Err(OmpMaintenanceAdmissionError::Active) => {
                        tracing::warn!(
                            pane_id = %announced_key.pane_id,
                            omp_session_id = %announced_key.omp_session_id,
                            route_generation = announced_key.route_generation,
                            "rejected OMP bridge host during maintenance"
                        );
                        let _ = admission.send(OmpHostAdmission::Rejected {
                            code: "omp_maintenance_active".into(),
                            message: "OMP host admission is disabled during maintenance".into(),
                        });
                    }
                    Err(OmpMaintenanceAdmissionError::State(error)) => {
                        tracing::warn!(
                            pane_id = %announced_key.pane_id,
                            omp_session_id = %announced_key.omp_session_id,
                            route_generation = announced_key.route_generation,
                            code = error.code(),
                            message = %error.message(),
                            "rejected OMP bridge host because maintenance state is unavailable"
                        );
                        let _ = admission.send(OmpHostAdmission::Rejected {
                            code: error.code().into(),
                            message: error.message(),
                        });
                    }
                    Err(OmpMaintenanceAdmissionError::Route(error)) => {
                        tracing::warn!(
                            pane_id = %announced_key.pane_id,
                            omp_session_id = %announced_key.omp_session_id,
                            route_generation = announced_key.route_generation,
                            code = error.code(),
                            "rejected OMP bridge host"
                        );
                        let _ = admission.send(OmpHostAdmission::Rejected {
                            code: error.code().to_owned(),
                            message: host_rejection_message(&error).into(),
                        });
                    }
                }
            }
            ServerEvent::OmpHostFrame {
                pane_id,
                omp_session_id,
                route_generation,
                host_id,
                target_client_id,
                frame,
            } => {
                if !valid_route_id(&pane_id) || !valid_route_id(&omp_session_id) {
                    return messages;
                }
                let key = OmpRouteKey {
                    pane_id: pane_id.clone(),
                    omp_session_id,
                    route_generation,
                };
                let route_key = (
                    key.pane_id.clone(),
                    key.omp_session_id.clone(),
                    key.route_generation,
                );
                if self
                    .hosts
                    .get(&route_key)
                    .is_none_or(|(current, _, _)| *current != host_id)
                {
                    return messages;
                }
                match self.routes.host_frame(&key, target_client_id, frame) {
                    Ok(deliveries) => self.deliver(&key, deliveries, &mut messages, clients),
                    Err(error) => {
                        let invalid_frame = matches!(error, OmpRouteError::InvalidFrame(_));
                        if let Some(client_id) = target_client_id {
                            self.send_error(&mut messages, client_id, pane_id, error);
                        }
                        if invalid_frame {
                            tracing::warn!(
                                pane_id = %key.pane_id,
                                omp_session_id = %key.omp_session_id,
                                route_generation = key.route_generation,
                                "invalid OMP host frame; closing route"
                            );
                            self.close_host_route(&key, &mut messages, clients);
                        }
                    }
                }
            }
            ServerEvent::OmpHostStopped {
                pane_id,
                omp_session_id,
                route_generation,
                host_id,
            } => {
                if !valid_route_id(&pane_id) || !valid_route_id(&omp_session_id) {
                    return messages;
                }
                let key = OmpRouteKey {
                    pane_id,
                    omp_session_id,
                    route_generation,
                };
                let route_key = (
                    key.pane_id.clone(),
                    key.omp_session_id.clone(),
                    key.route_generation,
                );
                if self
                    .hosts
                    .get(&route_key)
                    .is_some_and(|(current, _, _)| *current == host_id)
                {
                    self.remove_host(&key);
                    if let Ok(deliveries) = self.routes.host_stopped(&key) {
                        self.deliver(&key, deliveries, &mut messages, clients);
                        self.routes.remove_if_inactive_and_empty(&key);
                    }
                }
            }
            _ => unreachable!("only OMP events are dispatched to OmpService"),
        }
        messages
    }

    #[cfg(test)]
    pub(crate) fn bound_app_for_renderer(&self, client_id: u64) -> Option<u64> {
        self.bound_apps.get(&client_id).copied()
    }

    pub(crate) fn app_has_native_renderer(&self, app_client_id: u64) -> bool {
        self.bound_apps.iter().any(|(renderer_id, bound_app_id)| {
            *bound_app_id == app_client_id
                && *renderer_id != app_client_id
                && self.renderer_modes.get(renderer_id) == Some(&OmpRendererMode::ClientLocalNative)
        })
    }

    pub(crate) fn app_has_native_renderer_for_pane(
        &self,
        app_client_id: u64,
        pane_id: &str,
    ) -> bool {
        self.bound_apps.iter().any(|(renderer_id, bound_app_id)| {
            *bound_app_id == app_client_id
                && *renderer_id != app_client_id
                && self.renderer_modes.get(renderer_id) == Some(&OmpRendererMode::ClientLocalNative)
                && self
                    .route_bindings
                    .get(renderer_id)
                    .is_some_and(|route| route.pane_id == pane_id)
        })
    }

    pub(crate) fn app_has_native_renderer_for_route(
        &self,
        app_client_id: u64,
        key: &OmpRouteKey,
    ) -> bool {
        self.bound_apps.iter().any(|(renderer_id, bound_app_id)| {
            *bound_app_id == app_client_id
                && *renderer_id != app_client_id
                && self.renderer_modes.get(renderer_id) == Some(&OmpRendererMode::ClientLocalNative)
                && self.route_bindings.get(renderer_id) == Some(key)
        })
    }
    pub(crate) fn app_client_for_renderer(
        &self,
        client_id: u64,
        target_app_client_id: Option<u64>,
        renderer_launch_id: Option<u64>,
        key: &OmpRouteKey,
        clients: &HashMap<u64, ClientConnection>,
    ) -> Option<u64> {
        self.bind_app_client(
            client_id,
            target_app_client_id,
            renderer_launch_id,
            key,
            clients,
        )
    }
    fn native_renderer_is_ready(
        &self,
        renderer_id: u64,
        clients: &HashMap<u64, ClientConnection>,
    ) -> bool {
        self.renderer_modes.get(&renderer_id) == Some(&OmpRendererMode::ClientLocalNative)
            && self
                .bound_apps
                .get(&renderer_id)
                .and_then(|app_id| clients.get(app_id))
                .and_then(|client| client.omp_renderer_target.as_ref())
                .is_some_and(|target| target.ready)
    }

    fn renderers_for_peer(
        &self,
        peer_id: u64,
        key: &OmpRouteKey,
        clients: &HashMap<u64, ClientConnection>,
    ) -> Vec<u64> {
        let renderers = self
            .bound_apps
            .iter()
            .filter_map(|(&renderer_id, &app_id)| {
                (app_id == peer_id && self.route_bindings.get(&renderer_id) == Some(key))
                    .then_some(renderer_id)
            })
            .collect::<Vec<_>>();
        if let Some(renderer_id) = renderers
            .iter()
            .copied()
            .find(|renderer_id| self.native_renderer_is_ready(*renderer_id, clients))
        {
            return vec![renderer_id];
        }
        renderers
    }

    fn client_is_private_renderer(&self, client_id: u64) -> bool {
        self.renderer_modes.get(&client_id) == Some(&OmpRendererMode::ServerPrivateGuestPty)
            && self.bound_apps.get(&client_id) == Some(&client_id)
    }

    fn send_error(
        &self,
        messages: &mut Vec<(u64, ServerMessage)>,
        client_id: u64,
        pane_id: String,
        error: OmpRouteError,
    ) {
        messages.push((
            client_id,
            ServerMessage::OmpError {
                pane_id,
                code: error.code().to_owned(),
                message: match error {
                    OmpRouteError::InvalidFrame(message) => message,
                    error => error.code().to_owned(),
                },
            },
        ));
    }

    fn bind_app_client(
        &self,
        pane_client_id: u64,
        target_app_client_id: Option<u64>,
        renderer_launch_id: Option<u64>,
        key: &OmpRouteKey,
        clients: &HashMap<u64, ClientConnection>,
    ) -> Option<u64> {
        let renderer_binding_token = clients
            .get(&pane_client_id)?
            .renderer_binding_token
            .as_deref()?;
        let target_app_client_id = target_app_client_id?;
        let renderer_launch_id = renderer_launch_id?;
        let client = clients.get(&target_app_client_id)?;
        let route = client.omp_renderer_target.as_ref()?;
        (client.is_full_app_client()
            && client.omp_renderer_capabilities.client_local_native
            && client.renderer_binding_token.as_deref() == Some(renderer_binding_token)
            && route.launch_id == renderer_launch_id
            && route.route.as_ref().is_some_and(|route| {
                route.pane_id == key.pane_id
                    && route.omp_session_id == key.omp_session_id
                    && route.route_generation == key.route_generation
            }))
        .then_some(target_app_client_id)
    }

    fn deliver(
        &mut self,
        key: &OmpRouteKey,
        deliveries: Vec<OmpRouteDelivery>,
        messages: &mut Vec<(u64, ServerMessage)>,
        clients: &HashMap<u64, ClientConnection>,
    ) {
        for delivery in deliveries {
            match delivery {
                OmpRouteDelivery::Guest {
                    client_id: peer_id,
                    attachment_epoch,
                    frame,
                } => {
                    for client_id in self.renderers_for_peer(peer_id, key, clients) {
                        messages.push((
                            client_id,
                            ServerMessage::OmpFrame {
                                pane_id: key.pane_id.clone(),
                                omp_session_id: key.omp_session_id.clone(),
                                route_generation: key.route_generation,
                                attachment_epoch,
                                frame: frame.clone(),
                            },
                        ));
                    }
                }
                OmpRouteDelivery::Pane {
                    client_id: peer_id,
                    attachment_epoch,
                    controller,
                    state,
                } => {
                    for client_id in self.renderers_for_peer(peer_id, key, clients) {
                        let Some(&renderer_mode) = self.renderer_modes.get(&client_id) else {
                            continue;
                        };
                        messages.push((
                            client_id,
                            ServerMessage::OmpPane {
                                pane_id: key.pane_id.clone(),
                                omp_session_id: key.omp_session_id.clone(),
                                route_generation: key.route_generation,
                                attachment_epoch,
                                renderer_mode,
                                controller,
                                state: state.clone(),
                            },
                        ));
                    }
                }
                OmpRouteDelivery::HostFrame {
                    client_id, frame, ..
                } => {
                    let frame = match crate::protocol::validate_omp_frame(
                        &frame,
                        OmpFrameDirection::GuestToHost,
                    ) {
                        Ok(frame) => frame,
                        Err(error) => {
                            tracing::warn!(%error, "invalid OMP guest frame; closing route");
                            self.close_host_route(key, messages, clients);
                            return;
                        }
                    };
                    if !self.send_guest_record(key, client_id, frame, clients, messages) {
                        return;
                    }
                }
                OmpRouteDelivery::HostControl {
                    client_id, action, ..
                } => {
                    if let OmpControlAction::Mutation { frame } = action {
                        let frame = match crate::protocol::validate_omp_frame(
                            &frame,
                            OmpFrameDirection::GuestToHost,
                        ) {
                            Ok(frame) => frame,
                            Err(error) => {
                                tracing::warn!(%error, "invalid OMP guest mutation; closing route");
                                self.close_host_route(key, messages, clients);
                                return;
                            }
                        };
                        if !self.send_guest_record(key, client_id, frame, clients, messages) {
                            return;
                        }
                    }
                }
            }
        }
    }

    fn send_guest_record(
        &mut self,
        key: &OmpRouteKey,
        client_id: u64,
        frame: &[u8],
        clients: &HashMap<u64, ClientConnection>,
        messages: &mut Vec<(u64, ServerMessage)>,
    ) -> bool {
        let record = if let Some(identity) = self.external_peer_identities.get(&client_id) {
            omp_bridge::guest_record(client_id, frame, &identity.name, identity.revision)
        } else if let Some(identity) = clients
            .get(&client_id)
            .and_then(ClientConnection::committed_identity)
        {
            omp_bridge::guest_record(client_id, frame, &identity.name, identity.revision)
        } else if self.renderer_modes.get(&client_id) == Some(&OmpRendererMode::ExternalGuestBridge)
        {
            tracing::warn!(client_id, "external OMP guest has no validated identity");
            self.close_host_route(key, messages, clients);
            return false;
        } else {
            return true;
        };
        let Some(record) = record else {
            tracing::warn!("invalid OMP guest JSON payload; closing route");
            self.close_host_route(key, messages, clients);
            return false;
        };
        self.send_host_record(key, record, messages, clients)
    }

    fn send_peer_left(
        &mut self,
        key: &OmpRouteKey,
        peer: u64,
        messages: &mut Vec<(u64, ServerMessage)>,
        clients: &HashMap<u64, ClientConnection>,
    ) -> bool {
        self.send_host_record(
            key,
            format!(r#"{{"t":"peer-left","peer":{peer}}}"#),
            messages,
            clients,
        )
    }

    fn sync_authority(
        &mut self,
        key: &OmpRouteKey,
        deliveries: &[OmpRouteDelivery],
        messages: &mut Vec<(u64, ServerMessage)>,
        clients: &HashMap<u64, ClientConnection>,
    ) -> bool {
        for delivery in deliveries {
            if let OmpRouteDelivery::Pane {
                client_id,
                controller,
                ..
            } = delivery
            {
                if !self.send_host_record(
                    key,
                    omp_bridge::peer_authority_record(*client_id, *controller),
                    messages,
                    clients,
                ) {
                    return false;
                }
            }
        }
        true
    }

    fn send_host_record(
        &mut self,
        key: &OmpRouteKey,
        record: String,
        messages: &mut Vec<(u64, ServerMessage)>,
        clients: &HashMap<u64, ClientConnection>,
    ) -> bool {
        let route_key = (
            key.pane_id.clone(),
            key.omp_session_id.clone(),
            key.route_generation,
        );
        let Some((_, host, _)) = self.hosts.get(&route_key) else {
            return true;
        };
        if host.try_send(record).is_ok() {
            return true;
        }
        tracing::warn!(
            pane_id = %key.pane_id,
            omp_session_id = %key.omp_session_id,
            route_generation = key.route_generation,
            "OMP host outbound queue unavailable; closing route"
        );
        self.close_host_route(key, messages, clients);
        false
    }

    fn close_host_route(
        &mut self,
        key: &OmpRouteKey,
        messages: &mut Vec<(u64, ServerMessage)>,
        clients: &HashMap<u64, ClientConnection>,
    ) {
        self.remove_host(key);
        if let Ok(deliveries) = self.routes.host_stopped(key) {
            self.deliver(key, deliveries, messages, clients);
            self.routes.remove_if_inactive_and_empty(key);
        }
    }
    fn replace_host(
        &mut self,
        key: (String, String, u64),
        host_id: u64,
        outbound: SyncSender<String>,
        socket: TcpStream,
    ) {
        if let Some((_, _, socket)) = self.hosts.insert(key, (host_id, outbound, socket)) {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    fn remove_host(&mut self, key: &OmpRouteKey) {
        if let Some((_, _, socket)) = self.hosts.remove(&(
            key.pane_id.clone(),
            key.omp_session_id.clone(),
            key.route_generation,
        )) {
            let _ = socket.shutdown(Shutdown::Both);
        }
        self.unregister_maintenance_route(key, "failed to unregister closed OMP route");
    }

    fn unregister_maintenance_route(&mut self, key: &OmpRouteKey, message: &'static str) {
        let Ok(maintenance) = self.maintenance.as_ref() else {
            return;
        };
        match maintenance.unregister_route(key) {
            Ok(()) => {
                self.pending_maintenance_unregisters
                    .retain(|pending| pending != key);
            }
            Err(error) => {
                if !self
                    .pending_maintenance_unregisters
                    .iter()
                    .any(|pending| pending == key)
                {
                    self.pending_maintenance_unregisters.push(key.clone());
                }
                tracing::warn!(
                    code = error.code(),
                    message = %error.message(),
                    "{message}"
                );
            }
        }
    }

    fn retry_maintenance_unregisters(&mut self) {
        let pending = std::mem::take(&mut self.pending_maintenance_unregisters);
        for key in pending {
            self.unregister_maintenance_route(
                &key,
                "failed to retry OMP maintenance route cleanup",
            );
        }
    }
}

impl Drop for OmpService {
    fn drop(&mut self) {
        let hosts = std::mem::take(&mut self.hosts);
        for ((pane_id, omp_session_id, route_generation), (_, _, socket)) in hosts {
            let _ = socket.shutdown(Shutdown::Both);
            let key = OmpRouteKey {
                pane_id,
                omp_session_id,
                route_generation,
            };
            self.unregister_maintenance_route(
                &key,
                "failed to unregister OMP route while stopping service",
            );
        }
        self.retry_maintenance_unregisters();
        if self.pending_maintenance_unregisters.is_empty() {
            if let Ok(maintenance) = self.maintenance.as_mut() {
                maintenance.retire_instance();
            }
        }
    }
}

fn valid_route_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= crate::protocol::MAX_OMP_ROUTE_ID_BYTES
        && !value.chars().any(char::is_control)
}

fn host_rejection_message(error: &OmpRouteError) -> &'static str {
    match error {
        OmpRouteError::UnknownRoute => "OMP host route is unknown",
        OmpRouteError::StaleGeneration => "OMP host route generation is stale",
        OmpRouteError::StaleAttachment => "OMP host attachment is stale",
        OmpRouteError::HostUnavailable => "OMP host is unavailable",
        OmpRouteError::RouteBusy => "OMP host route is already active",
        OmpRouteError::ControllerRequired => "OMP host controller is required",
        OmpRouteError::InvalidFrame(_) => "OMP host frame is invalid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const OWNER: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
    const OTHER_OWNER: &str = "ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8";

    fn host_socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    fn start_host(
        service: &mut OmpService,
        pane_id: &str,
        omp_session_id: &str,
        route_generation: u64,
        host_id: u64,
    ) -> (TcpStream, std::sync::mpsc::Receiver<OmpHostAdmission>) {
        let (peer, socket) = host_socket_pair();
        let (outbound, _outbound_rx) = std::sync::mpsc::sync_channel(1);
        let (admission, admitted) = std::sync::mpsc::sync_channel(1);
        service.handle_event(
            ServerEvent::OmpHostStarted {
                pane_id: pane_id.into(),
                omp_session_id: omp_session_id.into(),
                route_generation,
                host_id,
                outbound,
                socket,
                admission,
            },
            false,
            &HashMap::new(),
        );
        (peer, admitted)
    }

    fn stop_host(
        service: &mut OmpService,
        pane_id: &str,
        omp_session_id: &str,
        route_generation: u64,
        host_id: u64,
    ) {
        service.handle_event(
            ServerEvent::OmpHostStopped {
                pane_id: pane_id.into(),
                omp_session_id: omp_session_id.into(),
                route_generation,
                host_id,
            },
            false,
            &HashMap::new(),
        );
    }

    fn admission(receiver: std::sync::mpsc::Receiver<OmpHostAdmission>) -> OmpHostAdmission {
        receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("OMP host admission result")
    }

    #[test]
    fn host_route_replacement_advances_generation_and_rejects_stale_claims() {
        use std::io::Read as _;

        let store = crate::server::omp_maintenance::TestOmpMaintenanceStore::new();
        let mut service =
            OmpService::with_test_maintenance(None, "default", store).expect("service");
        let (mut first_peer, first_admitted) = start_host(&mut service, "pane", "session", 1, 1);
        assert!(matches!(
            admission(first_admitted),
            OmpHostAdmission::Accepted {
                route_generation: 1
            }
        ));
        assert_eq!(
            service.maintenance_status().unwrap().routes[0].route_generation,
            1
        );

        stop_host(&mut service, "pane", "session", 1, 1);
        assert_eq!(first_peer.read(&mut [0]).unwrap(), 0);

        let (mut replacement_peer, replacement_admitted) =
            start_host(&mut service, "pane", "session", 1, 2);
        assert!(matches!(
            admission(replacement_admitted),
            OmpHostAdmission::Accepted {
                route_generation: 2
            }
        ));
        assert_eq!(
            service.live_route_keys(),
            vec![OmpRouteKey {
                pane_id: "pane".into(),
                omp_session_id: "session".into(),
                route_generation: 2,
            }]
        );
        let status = service.maintenance_status().unwrap();
        assert_eq!(status.route_count, 1);
        assert_eq!(status.routes[0].route_generation, 2);

        let (stale_peer, stale) = start_host(&mut service, "pane", "session", 1, 3);
        assert!(matches!(
            admission(stale),
            OmpHostAdmission::Rejected { code, .. } if code == "stale_generation"
        ));
        let (forged_peer, forged) = start_host(&mut service, "pane", "session", 3, 4);
        assert!(matches!(
            admission(forged),
            OmpHostAdmission::Rejected { code, .. } if code == "stale_generation"
        ));
        let (busy_peer, busy) = start_host(&mut service, "pane", "session", 2, 5);
        assert!(matches!(
            admission(busy),
            OmpHostAdmission::Rejected { code, .. } if code == "route_busy"
        ));

        stop_host(&mut service, "pane", "session", 2, 2);
        assert_eq!(replacement_peer.read(&mut [0]).unwrap(), 0);

        let (mut third_peer, third_admitted) = start_host(&mut service, "pane", "session", 2, 6);
        assert!(matches!(
            admission(third_admitted),
            OmpHostAdmission::Accepted {
                route_generation: 3
            }
        ));
        assert_eq!(
            service.maintenance_status().unwrap().routes[0].route_generation,
            3
        );
        stop_host(&mut service, "pane", "session", 3, 6);
        assert_eq!(third_peer.read(&mut [0]).unwrap(), 0);
        drop((stale_peer, forged_peer, busy_peer));
    }

    #[test]
    fn unavailable_maintenance_keeps_core_service_running_and_rejects_hosts() {
        let mut service = OmpService::with_maintenance(
            None,
            Err(OmpMaintenanceError::StateIo(
                "untrusted OMP maintenance state directory".into(),
            )),
        )
        .expect("create core service without OMP maintenance");

        assert!(matches!(
            service.maintenance_status(),
            Err(OmpMaintenanceError::StateIo(_))
        ));
        let (peer, rejected) = start_host(&mut service, "w1:p1", "omp-unavailable", 1, 1);
        assert!(matches!(
            admission(rejected),
            OmpHostAdmission::Rejected { code, .. }
                if code == "omp_maintenance_state_unavailable"
        ));
        assert!(service.live_route_keys().is_empty());
        drop(peer);
    }

    #[test]
    fn state_error_before_duplicate_admission_preserves_existing_live_route() {
        let store = crate::server::omp_maintenance::TestOmpMaintenanceStore::new();
        let mut service =
            OmpService::with_test_maintenance(None, "default", store.clone()).expect("service");
        let key = OmpRouteKey {
            pane_id: "w1:p1".into(),
            omp_session_id: "omp-live".into(),
            route_generation: 1,
        };
        let (live_peer, admitted) = start_host(
            &mut service,
            &key.pane_id,
            &key.omp_session_id,
            key.route_generation,
            1,
        );
        assert!(matches!(
            admission(admitted),
            OmpHostAdmission::Accepted {
                route_generation: 1
            }
        ));

        store.fail_next_state_accesses(1);
        let (retry_peer, rejected) = start_host(
            &mut service,
            &key.pane_id,
            &key.omp_session_id,
            key.route_generation,
            2,
        );
        assert!(matches!(
            admission(rejected),
            OmpHostAdmission::Rejected { code, .. }
                if code == "omp_maintenance_state_unavailable"
        ));
        assert_eq!(service.live_route_keys(), vec![key.clone()]);

        let attachment_epoch = service
            .routes
            .attach(91, &key)
            .expect("existing route remains registered")
            .into_iter()
            .find_map(|delivery| match delivery {
                OmpRouteDelivery::Pane {
                    client_id: 91,
                    attachment_epoch,
                    ..
                } => Some(attachment_epoch),
                _ => None,
            })
            .expect("existing route attachment");
        let frame =
            crate::protocol::encode_omp_frame(OmpFrameDirection::GuestToHost, br#"{"t":"hello"}"#)
                .expect("guest frame");
        assert!(service
            .routes
            .guest_frame(91, &key, attachment_epoch, frame)
            .is_ok());

        stop_host(
            &mut service,
            &key.pane_id,
            &key.omp_session_id,
            key.route_generation,
            1,
        );
        drop((live_peer, retry_peer));
    }

    #[test]
    fn ordinary_admission_reports_exact_route_when_no_lease_exists() {
        let store = crate::server::omp_maintenance::TestOmpMaintenanceStore::new();
        let mut service =
            OmpService::with_test_maintenance(None, "default", store).expect("create service");

        let (peer, admitted) = start_host(&mut service, "w1:p1", "omp-1", 1, 7);
        assert!(matches!(
            admission(admitted),
            OmpHostAdmission::Accepted {
                route_generation: 1
            }
        ));
        let status = service.maintenance_status().unwrap();
        assert!(!status.held);
        assert_eq!(status.route_count, 1);
        assert_eq!(
            status.routes,
            vec![crate::api::schema::ServerOmpMaintenanceRoute {
                session: "default".into(),
                pane_id: "w1:p1".into(),
                omp_session_id: "omp-1".into(),
                route_generation: 1,
                proof: false,
            }]
        );

        stop_host(&mut service, "w1:p1", "omp-1", 1, 7);
        assert_eq!(service.maintenance_status().unwrap().route_count, 0);
        drop(peer);
    }

    #[test]
    fn failed_unregister_is_retried_without_removing_other_live_sessions() {
        let store = crate::server::omp_maintenance::TestOmpMaintenanceStore::new();
        let mut first = OmpService::with_test_maintenance(None, "first", store.clone())
            .expect("create first service");
        let mut second = OmpService::with_test_maintenance(None, "second", store.clone())
            .expect("create second service");
        let controller = OmpService::with_test_maintenance(None, "controller", store.clone())
            .expect("create controller service");
        let (first_peer, first_admitted) = start_host(&mut first, "w1:p1", "omp-1", 1, 1);
        let (second_peer, second_admitted) = start_host(&mut second, "w2:p1", "omp-2", 1, 2);
        assert!(matches!(
            admission(first_admitted),
            OmpHostAdmission::Accepted { .. }
        ));
        assert!(matches!(
            admission(second_admitted),
            OmpHostAdmission::Accepted { .. }
        ));
        assert_eq!(controller.route_set_revision().unwrap(), 2);

        store.fail_next_unregisters(1);
        stop_host(&mut first, "w1:p1", "omp-1", 1, 1);
        assert!(first.live_route_keys().is_empty());
        assert_eq!(controller.maintenance_status().unwrap().route_count, 2);
        assert_eq!(controller.route_set_revision().unwrap(), 2);

        assert!(first.enforce_maintenance(&HashMap::new()).is_empty());
        let status = controller.maintenance_status().unwrap();
        assert_eq!(status.route_count, 1);
        assert_eq!(status.routes[0].session, "second");
        assert_eq!(controller.route_set_revision().unwrap(), 3);
        assert_eq!(second.live_route_keys().len(), 1);

        stop_host(&mut second, "w2:p1", "omp-2", 1, 2);
        assert_eq!(controller.route_set_revision().unwrap(), 4);
        drop((first_peer, second_peer));
    }

    #[test]
    fn acquire_gates_before_existing_routes_drain_and_covers_new_sessions() {
        use std::io::Read as _;

        let store = crate::server::omp_maintenance::TestOmpMaintenanceStore::new();
        let mut existing = OmpService::with_test_maintenance(None, "existing", store.clone())
            .expect("create existing service");
        let controller = OmpService::with_test_maintenance(None, "controller", store.clone())
            .expect("create controller service");
        let (mut existing_peer, admitted) =
            start_host(&mut existing, "w1:p1", "omp-existing", 1, 1);
        assert!(matches!(
            admission(admitted),
            OmpHostAdmission::Accepted { .. }
        ));

        let acquired = controller.acquire_maintenance(OWNER).unwrap();
        assert!(acquired.held);
        assert_eq!(acquired.route_count, 1);

        let mut newly_created =
            OmpService::with_test_maintenance(None, "new", store).expect("create new service");
        let (_new_peer, denied) = start_host(&mut newly_created, "w2:p1", "omp-new", 1, 2);
        assert!(matches!(
            admission(denied),
            OmpHostAdmission::Rejected { code, .. } if code == "omp_maintenance_active"
        ));

        existing.enforce_maintenance(&HashMap::new());
        assert_eq!(controller.maintenance_status().unwrap().route_count, 0);
        assert_eq!(existing_peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn permit_is_exact_and_consumed_by_one_admission() {
        let store = crate::server::omp_maintenance::TestOmpMaintenanceStore::new();
        let controller = OmpService::with_test_maintenance(None, "controller", store.clone())
            .expect("create controller service");
        let mut proof = OmpService::with_test_maintenance(None, "proof", store.clone())
            .expect("create proof service");
        let mut wrong_session =
            OmpService::with_test_maintenance(None, "other", store).expect("create other service");
        controller.acquire_maintenance(OWNER).unwrap();
        controller
            .grant_maintenance_permit(OWNER, "proof", "w1:p1")
            .unwrap();

        let (_peer, denied) = start_host(&mut wrong_session, "w1:p1", "omp-wrong", 1, 1);
        assert!(matches!(
            admission(denied),
            OmpHostAdmission::Rejected { code, .. } if code == "omp_maintenance_active"
        ));
        assert!(controller.maintenance_status().unwrap().permit.is_some());

        let (_peer, denied) = start_host(&mut proof, "w1:p2", "omp-wrong-pane", 1, 2);
        assert!(matches!(
            admission(denied),
            OmpHostAdmission::Rejected { code, .. } if code == "omp_maintenance_active"
        ));
        assert!(controller.maintenance_status().unwrap().permit.is_some());

        let (proof_peer, admitted) = start_host(&mut proof, "w1:p1", "omp-proof", 1, 3);
        assert!(matches!(
            admission(admitted),
            OmpHostAdmission::Accepted { .. }
        ));
        let consumed = controller.maintenance_status().unwrap();
        assert!(consumed.permit.is_none());
        assert_eq!(consumed.route_count, 1);
        assert!(consumed.routes[0].proof);
        assert_eq!(
            controller
                .grant_maintenance_permit(OWNER, "proof", "w1:p1")
                .unwrap(),
            consumed
        );

        let (_peer, denied) = start_host(&mut proof, "w1:p1", "omp-second", 2, 4);
        assert!(matches!(
            admission(denied),
            OmpHostAdmission::Rejected { code, .. } if code == "omp_maintenance_active"
        ));
        stop_host(&mut proof, "w1:p1", "omp-proof", 1, 3);
        assert_eq!(controller.maintenance_status().unwrap().route_count, 0);
        drop(proof_peer);
    }

    #[test]
    fn ownership_is_idempotent_and_release_requires_zero_routes() {
        let store = crate::server::omp_maintenance::TestOmpMaintenanceStore::new();
        let mut service =
            OmpService::with_test_maintenance(None, "default", store).expect("create service");
        let (peer, admitted) = start_host(&mut service, "w1:p1", "omp-1", 1, 1);
        assert!(matches!(
            admission(admitted),
            OmpHostAdmission::Accepted { .. }
        ));

        let first = service.acquire_maintenance(OWNER).unwrap();
        let second = service.acquire_maintenance(OWNER).unwrap();
        assert_eq!(first, second);
        assert!(matches!(
            service.acquire_maintenance(OTHER_OWNER),
            Err(OmpMaintenanceError::Conflict(_))
        ));
        assert!(matches!(
            service.release_maintenance(OTHER_OWNER),
            Err(OmpMaintenanceError::NotOwner(_))
        ));
        assert!(matches!(
            service.release_maintenance(OWNER),
            Err(OmpMaintenanceError::RoutesLive(1))
        ));

        service.enforce_maintenance(&HashMap::new());
        assert_eq!(service.maintenance_status().unwrap().route_count, 0);
        let released = service.release_maintenance(OWNER).unwrap();
        assert!(!released.held);
        assert_eq!(service.release_maintenance(OWNER).unwrap(), released);
        drop(peer);
    }

    #[test]
    fn live_route_keys_reports_retained_host_routes() {
        let mut service = OmpService::new(None).unwrap();
        let (peer, socket) = host_socket_pair();
        let (outbound, _) = std::sync::mpsc::sync_channel(1);
        service.replace_host(("pane".into(), "session".into(), 1), 7, outbound, socket);

        assert_eq!(
            service.live_route_keys(),
            vec![OmpRouteKey {
                pane_id: "pane".into(),
                omp_session_id: "session".into(),
                route_generation: 1,
            }]
        );

        drop(peer);
    }

    #[test]
    fn stale_host_stop_cannot_shutdown_a_replacement_socket() {
        use std::io::Read;

        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let (mut old_peer, old_socket) = host_socket_pair();
        let (old_outbound, _) = std::sync::mpsc::sync_channel(1);
        service.replace_host(
            ("pane".into(), "session".into(), 1),
            1,
            old_outbound,
            old_socket,
        );

        let (mut replacement_peer, replacement_socket) = host_socket_pair();
        let (replacement_outbound, _) = std::sync::mpsc::sync_channel(1);
        service.replace_host(
            ("pane".into(), "session".into(), 1),
            2,
            replacement_outbound,
            replacement_socket,
        );
        assert_eq!(old_peer.read(&mut [0]).unwrap(), 0);

        service.handle_event(
            ServerEvent::OmpHostStopped {
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                host_id: 1,
            },
            false,
            &HashMap::new(),
        );
        assert!(service
            .hosts
            .contains_key(&("pane".into(), "session".into(), 1)));
        replacement_peer
            .set_read_timeout(Some(std::time::Duration::from_millis(10)))
            .unwrap();
        assert!(
            matches!(replacement_peer.read(&mut [0]), Err(error) if error.kind() == io::ErrorKind::WouldBlock || error.kind() == io::ErrorKind::TimedOut)
        );

        service.remove_host(&key);
        assert_eq!(replacement_peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn invalid_host_payload_closes_route_and_socket() {
        use std::io::Read;

        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        service.routes.host_started(key.clone()).unwrap();
        let (mut peer, socket) = host_socket_pair();
        let (outbound, _) = std::sync::mpsc::sync_channel(1);
        service.replace_host(("pane".into(), "session".into(), 1), 7, outbound, socket);
        let invalid =
            crate::protocol::encode_omp_frame(OmpFrameDirection::HostToGuest, b"not-json").unwrap();

        service.handle_event(
            ServerEvent::OmpHostFrame {
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                host_id: 7,
                target_client_id: None,
                frame: invalid,
            },
            false,
            &HashMap::new(),
        );

        assert!(!service
            .hosts
            .contains_key(&("pane".into(), "session".into(), 1)));
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        assert!(matches!(
            service.routes.attach(1, &key),
            Err(OmpRouteError::UnknownRoute | OmpRouteError::HostUnavailable)
        ));
    }

    #[test]
    fn dropping_service_shuts_down_every_retained_host_socket() {
        use std::io::Read;
        use std::time::Duration;

        let mut service = OmpService::new(None).unwrap();
        let (mut first_peer, first_socket) = host_socket_pair();
        let first_bridge_clone = first_socket.try_clone().unwrap();
        let (first_outbound, _) = std::sync::mpsc::sync_channel(1);
        service.replace_host(
            ("first-pane".into(), "session".into(), 1),
            1,
            first_outbound,
            first_socket,
        );

        let (mut second_peer, second_socket) = host_socket_pair();
        let second_bridge_clone = second_socket.try_clone().unwrap();
        let (second_outbound, _) = std::sync::mpsc::sync_channel(1);
        service.replace_host(
            ("second-pane".into(), "session".into(), 1),
            2,
            second_outbound,
            second_socket,
        );

        first_peer
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        second_peer
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        drop(service);

        assert_eq!(first_peer.read(&mut [0]).unwrap(), 0);
        assert_eq!(second_peer.read(&mut [0]).unwrap(), 0);
        drop((first_bridge_clone, second_bridge_clone));
    }

    #[test]
    fn observer_non_mutating_omp_frame_reaches_host() {
        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        service.routes.host_started(key.clone()).unwrap();
        service.routes.attach(22, &key).unwrap();
        let observer_epoch = service
            .routes
            .attach(33, &key)
            .unwrap()
            .into_iter()
            .find_map(|delivery| match delivery {
                OmpRouteDelivery::Pane {
                    client_id: 33,
                    attachment_epoch,
                    ..
                } => Some(attachment_epoch),
                _ => None,
            })
            .unwrap();
        let (peer, socket) = host_socket_pair();
        let (outbound, outbound_rx) = std::sync::mpsc::sync_channel(1);
        service.replace_host(
            (
                key.pane_id.clone(),
                key.omp_session_id.clone(),
                key.route_generation,
            ),
            7,
            outbound,
            socket,
        );
        service.bound_apps.insert(11, 33);

        let frame =
            crate::protocol::encode_omp_frame(OmpFrameDirection::GuestToHost, br#"{"t":"hello"}"#)
                .unwrap();
        let messages = service.handle_event(
            ServerEvent::OmpFrame {
                client_id: 11,
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                attachment_epoch: observer_epoch,
                frame,
            },
            true,
            &HashMap::from([
                (
                    11,
                    client(
                        crate::server::clients::ClientConnectionMode::OmpPane,
                        None,
                        Some("observer"),
                    ),
                ),
                (
                    33,
                    client(
                        crate::server::clients::ClientConnectionMode::App,
                        Some("Observer"),
                        Some("observer"),
                    ),
                ),
            ]),
        );

        assert!(messages.is_empty());
        let record = outbound_rx.try_recv().unwrap();
        let record: serde_json::Value = serde_json::from_str(&record).unwrap();
        assert_eq!(record["fromPeer"], 33);
        assert_eq!(record["frame"]["t"], "hello");
        drop(peer);
    }

    fn client_with_binding(
        mode: crate::server::clients::ClientConnectionMode,
        name: Option<&str>,
        profile: Option<&str>,
        binding: Option<&str>,
    ) -> ClientConnection {
        ClientConnection::new_with_mode(
            mode,
            None,
            name.map(str::to_owned),
            profile.map(str::to_owned),
            binding.map(str::to_owned),
            (80, 24),
            crate::kitty_graphics::HostCellSize::default(),
            crate::terminal_theme::TerminalTheme::default(),
            None,
            0,
            crate::protocol::RenderEncoding::SemanticFrame,
            false,
            None,
        )
    }

    fn client(
        mode: crate::server::clients::ClientConnectionMode,
        name: Option<&str>,
        profile: Option<&str>,
    ) -> ClientConnection {
        client_with_binding(
            mode,
            name,
            profile,
            profile
                .map(|profile| format!("binding-{profile}"))
                .as_deref(),
        )
    }

    #[test]
    fn renderer_binding_requires_the_exact_server_target_and_launch() {
        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let mut clients = HashMap::new();
        clients.insert(
            1,
            client_with_binding(
                crate::server::clients::ClientConnectionMode::OmpPane,
                None,
                Some("profile-a"),
                Some("shared-token"),
            ),
        );
        let mut app = client_with_binding(
            crate::server::clients::ClientConnectionMode::App,
            Some("Ada"),
            Some("profile-a"),
            Some("shared-token"),
        );
        app.omp_renderer_capabilities.client_local_native = true;
        app.omp_renderer_target = Some(crate::server::clients::OmpRendererTargetState {
            launch_id: 7,
            route: Some(crate::protocol::OmpRendererRoute {
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
            }),
            bound: false,
            ready: false,
            prefix: crate::protocol::OmpRendererPrefix {
                code: crate::protocol::ClientKeyCode::Char('a'),
                modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
            },
            surface_active: false,
        });
        clients.insert(3, app);

        assert_eq!(
            service.bind_app_client(1, Some(3), Some(7), &key, &clients),
            Some(3)
        );
        assert_eq!(
            service.bind_app_client(1, Some(3), Some(6), &key, &clients),
            None,
            "a stale launch cannot claim the current route"
        );
        assert_eq!(
            service.bind_app_client(1, None, Some(7), &key, &clients),
            None,
            "the native route never falls back to ambiguous profile matching"
        );

        service.routes.host_started(key.clone()).unwrap();
        let messages = service.handle_event(
            ServerEvent::OmpPaneAttach {
                client_id: 1,
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                target_app_client_id: Some(3),
                renderer_capabilities: crate::protocol::OmpRendererCapabilities {
                    client_local_native: true,
                },
                renderer_launch_id: Some(7),
                renderer_request: crate::protocol::OmpRendererRequest::Independent,
                external_peer_identity: None,
            },
            true,
            &clients,
        );
        assert_eq!(service.bound_app_for_renderer(1), Some(3));
        assert!(!messages
            .iter()
            .any(|(_, message)| matches!(message, ServerMessage::OmpError { .. })));
    }

    #[test]
    fn fresh_profile_external_guest_admission_uses_display_only_fallback() {
        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        service.routes.host_started(key.clone()).unwrap();
        let clients = HashMap::from([(
            1,
            client(
                crate::server::clients::ClientConnectionMode::OmpPane,
                None,
                None,
            ),
        )]);
        assert!(clients[&1].committed_identity().is_none());
        let fresh_identity = crate::config::ClientIdentity {
            display_name: None,
            frontend_profile_id: "fresh-profile".into(),
            renderer_binding_token: "fresh-renderer-token".into(),
        };
        let external_identity = crate::client::omp_external_peer_identity(&fresh_identity);
        assert!(crate::config::validate_display_name(&external_identity.display_name).is_ok());
        assert!(fresh_identity.display_name.is_none());
        let display_name = external_identity.display_name.clone();

        let messages = service.handle_event(
            ServerEvent::OmpPaneAttach {
                client_id: 1,
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                target_app_client_id: None,
                renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
                renderer_launch_id: None,
                renderer_request: crate::protocol::OmpRendererRequest::ExternalGuestBridge,
                external_peer_identity: Some(external_identity),
            },
            true,
            &clients,
        );

        assert!(messages.iter().any(|(_, message)| matches!(
            message,
            ServerMessage::OmpPane {
                route_generation: 1,
                renderer_mode: OmpRendererMode::ExternalGuestBridge,
                controller: false,
                ..
            }
        )));
        assert_eq!(service.bound_app_for_renderer(1), Some(1));
        let stored = service.external_peer_identities.get(&1).unwrap();
        assert_eq!(stored.name, display_name);
        assert_eq!(stored.revision, 1);
    }

    #[test]
    fn external_guest_bridge_attaches_only_to_the_live_generation_as_an_observer() {
        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        service.routes.host_started(key.clone()).unwrap();
        let clients = HashMap::from([(
            1,
            client(
                crate::server::clients::ClientConnectionMode::OmpPane,
                None,
                None,
            ),
        )]);
        let attach = |route_generation| ServerEvent::OmpPaneAttach {
            client_id: 1,
            pane_id: key.pane_id.clone(),
            omp_session_id: key.omp_session_id.clone(),
            route_generation,
            target_app_client_id: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
            renderer_launch_id: None,
            renderer_request: crate::protocol::OmpRendererRequest::ExternalGuestBridge,
            external_peer_identity: Some(crate::protocol::OmpExternalPeerIdentity {
                display_name: "Ada".into(),
            }),
        };
        let messages = service.handle_event(attach(0), true, &clients);
        assert!(messages.iter().any(|(_, message)| matches!(
            message,
            ServerMessage::OmpError { code, .. } if code == "stale_generation"
        )));

        let messages = service.handle_event(attach(1), true, &clients);
        assert!(messages.iter().any(|(_, message)| matches!(
            message,
            ServerMessage::OmpPane {
                renderer_mode: OmpRendererMode::ExternalGuestBridge,
                controller: false,
                ..
            }
        )));
    }

    #[test]
    fn external_guest_bridge_routes_both_directions_and_controller_mutation() {
        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let (peer, socket) = host_socket_pair();
        let (outbound, outbound_rx) = std::sync::mpsc::sync_channel(8);
        let (admission_tx, admitted) = std::sync::mpsc::sync_channel(1);

        let clients = HashMap::from([(
            1,
            client(
                crate::server::clients::ClientConnectionMode::OmpPane,
                None,
                None,
            ),
        )]);
        assert!(clients[&1].committed_identity().is_none());
        service.handle_event(
            ServerEvent::OmpHostStarted {
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                host_id: 7,
                outbound,
                socket,
                admission: admission_tx,
            },
            false,
            &clients,
        );
        assert!(matches!(
            admission(admitted),
            OmpHostAdmission::Accepted { .. }
        ));

        let attached = service.handle_event(
            ServerEvent::OmpPaneAttach {
                client_id: 1,
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                target_app_client_id: None,
                renderer_capabilities: crate::protocol::OmpRendererCapabilities::default(),
                renderer_launch_id: None,
                renderer_request: crate::protocol::OmpRendererRequest::ExternalGuestBridge,
                external_peer_identity: Some(crate::protocol::OmpExternalPeerIdentity {
                    display_name: "Ada".into(),
                }),
            },
            true,
            &clients,
        );
        let attachment_epoch = attached
            .iter()
            .find_map(|(_, message)| match message {
                ServerMessage::OmpPane {
                    attachment_epoch,
                    renderer_mode: OmpRendererMode::ExternalGuestBridge,
                    controller: false,
                    ..
                } => Some(*attachment_epoch),
                _ => None,
            })
            .expect("external guest attachment");
        let authority: serde_json::Value = serde_json::from_str(
            &outbound_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .expect("observer authority record"),
        )
        .unwrap();
        assert_eq!(authority["t"], "peer-authority");
        assert_eq!(authority["canWrite"], false);

        let ordinary = crate::protocol::encode_omp_frame(
            OmpFrameDirection::GuestToHost,
            br#"{"t":"hello","proto":1,"name":"external"}"#,
        )
        .unwrap();
        assert!(service
            .handle_event(
                ServerEvent::OmpFrame {
                    client_id: 1,
                    pane_id: key.pane_id.clone(),
                    omp_session_id: key.omp_session_id.clone(),
                    route_generation: key.route_generation,
                    attachment_epoch,
                    frame: ordinary,
                },
                true,
                &clients,
            )
            .is_empty());
        let ordinary: serde_json::Value = serde_json::from_str(
            &outbound_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .expect("ordinary guest frame"),
        )
        .unwrap();
        assert_eq!(ordinary["fromPeer"], 1);
        assert_eq!(ordinary["displayName"], "Ada");
        assert_eq!(ordinary["displayNameRevision"], 1);
        assert_eq!(ordinary["frame"]["t"], "hello");

        service.handle_event(
            ServerEvent::OmpControl {
                client_id: 1,
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                attachment_epoch,
                action: OmpControlAction::RequestController,
            },
            true,
            &clients,
        );
        let authority: serde_json::Value = serde_json::from_str(
            &outbound_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .expect("controller authority record"),
        )
        .unwrap();
        assert_eq!(authority["canWrite"], true);

        let mutation = crate::protocol::encode_omp_frame(
            OmpFrameDirection::GuestToHost,
            br#"{"t":"prompt","text":"hello"}"#,
        )
        .unwrap();
        assert!(service
            .handle_event(
                ServerEvent::OmpControl {
                    client_id: 1,
                    pane_id: key.pane_id.clone(),
                    omp_session_id: key.omp_session_id.clone(),
                    route_generation: key.route_generation,
                    attachment_epoch,
                    action: OmpControlAction::Mutation { frame: mutation },
                },
                true,
                &clients,
            )
            .is_empty());
        let mutation: serde_json::Value = serde_json::from_str(
            &outbound_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .expect("controller mutation"),
        )
        .unwrap();
        assert_eq!(mutation["displayName"], "Ada");
        assert_eq!(mutation["frame"]["t"], "prompt");

        let host_frame =
            crate::protocol::encode_omp_frame(OmpFrameDirection::HostToGuest, br#"{"t":"state"}"#)
                .unwrap();
        let delivered = service.handle_event(
            ServerEvent::OmpHostFrame {
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                host_id: 7,
                target_client_id: Some(1),
                frame: host_frame,
            },
            false,
            &clients,
        );
        assert!(matches!(
            delivered.as_slice(),
            [(1, ServerMessage::OmpFrame { attachment_epoch: epoch, .. })]
                if *epoch == attachment_epoch
        ));
        drop(peer);
    }
    #[test]
    fn warming_native_renderer_is_quarantined_until_ready() {
        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        service.routes.host_started(key.clone()).unwrap();
        let attachment_epoch = service
            .routes
            .attach(33, &key)
            .unwrap()
            .into_iter()
            .find_map(|delivery| match delivery {
                OmpRouteDelivery::Pane {
                    client_id: 33,
                    attachment_epoch,
                    ..
                } => Some(attachment_epoch),
                _ => None,
            })
            .unwrap();
        let (peer, socket) = host_socket_pair();
        let (outbound, outbound_rx) = std::sync::mpsc::sync_channel(4);
        service.replace_host(
            (
                key.pane_id.clone(),
                key.omp_session_id.clone(),
                key.route_generation,
            ),
            7,
            outbound,
            socket,
        );
        service
            .renderer_modes
            .insert(11, OmpRendererMode::ClientLocalNative);
        service
            .renderer_modes
            .insert(33, OmpRendererMode::ServerPrivateGuestPty);
        service.bound_apps.insert(11, 33);
        service.bound_apps.insert(33, 33);
        service.route_bindings.insert(11, key.clone());
        service.route_bindings.insert(33, key.clone());

        let mut app = client(
            crate::server::clients::ClientConnectionMode::App,
            Some("Ada"),
            Some("profile-a"),
        );
        app.omp_renderer_target = Some(crate::server::clients::OmpRendererTargetState {
            launch_id: 7,
            route: Some(crate::protocol::OmpRendererRoute {
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
            }),
            bound: true,
            ready: false,
            prefix: crate::protocol::OmpRendererPrefix {
                code: crate::protocol::ClientKeyCode::Char('a'),
                modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
            },
            surface_active: false,
        });
        let mut clients = HashMap::from([
            (
                11,
                client(
                    crate::server::clients::ClientConnectionMode::OmpPane,
                    None,
                    Some("profile-a"),
                ),
            ),
            (33, app),
        ]);
        let frame =
            crate::protocol::encode_omp_frame(OmpFrameDirection::GuestToHost, br#"{"t":"hello"}"#)
                .unwrap();

        for event in [
            ServerEvent::OmpControl {
                client_id: 11,
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                attachment_epoch,
                action: OmpControlAction::Mutation {
                    frame: frame.clone(),
                },
            },
            ServerEvent::OmpFrame {
                client_id: 11,
                pane_id: key.pane_id.clone(),
                omp_session_id: key.omp_session_id.clone(),
                route_generation: key.route_generation,
                attachment_epoch,
                frame: frame.clone(),
            },
        ] {
            assert!(service.handle_event(event, true, &clients).is_empty());
        }
        assert!(outbound_rx.try_recv().is_err());
        let mut warming = service.renderers_for_peer(33, &key, &clients);
        warming.sort_unstable();
        assert_eq!(warming, vec![11, 33]);

        clients
            .get_mut(&33)
            .unwrap()
            .omp_renderer_target
            .as_mut()
            .unwrap()
            .ready = true;
        assert_eq!(service.renderers_for_peer(33, &key, &clients), vec![11]);
        assert!(service
            .handle_event(
                ServerEvent::OmpControl {
                    client_id: 11,
                    pane_id: key.pane_id.clone(),
                    omp_session_id: key.omp_session_id.clone(),
                    route_generation: key.route_generation,
                    attachment_epoch,
                    action: OmpControlAction::Mutation { frame },
                },
                true,
                &clients,
            )
            .is_empty());
        assert!(outbound_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .unwrap()
            .contains("\"fromPeer\":33"));
        drop(peer);
    }

    #[test]
    fn warming_native_disconnect_preserves_private_route() {
        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        service.routes.host_started(key.clone()).unwrap();
        let attachment_epoch = service
            .routes
            .attach(33, &key)
            .unwrap()
            .into_iter()
            .find_map(|delivery| match delivery {
                OmpRouteDelivery::Pane {
                    client_id: 33,
                    attachment_epoch,
                    ..
                } => Some(attachment_epoch),
                _ => None,
            })
            .unwrap();
        service
            .renderer_modes
            .insert(11, OmpRendererMode::ClientLocalNative);
        service
            .renderer_modes
            .insert(33, OmpRendererMode::ServerPrivateGuestPty);
        service.bound_apps.insert(11, 33);
        service.bound_apps.insert(33, 33);
        service.route_bindings.insert(11, key.clone());
        service.route_bindings.insert(33, key.clone());
        let clients = HashMap::from([
            (
                11,
                client(
                    crate::server::clients::ClientConnectionMode::OmpPane,
                    None,
                    Some("profile-a"),
                ),
            ),
            (
                33,
                client(
                    crate::server::clients::ClientConnectionMode::App,
                    Some("Ada"),
                    Some("profile-a"),
                ),
            ),
        ]);

        assert!(service.disconnect(11, &clients).is_empty());
        assert!(!service.bound_apps.contains_key(&11));
        assert!(!service.renderer_modes.contains_key(&11));
        assert!(!service.route_bindings.contains_key(&11));
        assert_eq!(service.bound_apps.get(&33), Some(&33));
        assert_eq!(service.renderers_for_peer(33, &key, &clients), vec![33]);
        let frame =
            crate::protocol::encode_omp_frame(OmpFrameDirection::GuestToHost, br#"{"t":"hello"}"#)
                .unwrap();
        assert!(service
            .routes
            .guest_frame(33, &key, attachment_epoch, frame)
            .is_ok());
    }

    #[test]
    fn warming_sideband_disconnect_leaves_a_moved_private_route_usable() {
        let mut service = OmpService::new(None).unwrap();
        let old = OmpRouteKey {
            pane_id: "old".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let new = OmpRouteKey {
            pane_id: "new".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        service.routes.host_started(old.clone()).unwrap();
        service.routes.host_started(new.clone()).unwrap();
        let old_epoch = service
            .routes
            .attach(33, &old)
            .unwrap()
            .into_iter()
            .find_map(|delivery| match delivery {
                OmpRouteDelivery::Pane {
                    client_id: 33,
                    attachment_epoch,
                    ..
                } => Some(attachment_epoch),
                _ => None,
            })
            .unwrap();
        let new_epoch = service
            .routes
            .attach(33, &new)
            .unwrap()
            .into_iter()
            .find_map(|delivery| match delivery {
                OmpRouteDelivery::Pane {
                    client_id: 33,
                    attachment_epoch,
                    ..
                } => Some(attachment_epoch),
                _ => None,
            })
            .unwrap();
        service
            .renderer_modes
            .insert(11, OmpRendererMode::ClientLocalNative);
        service
            .renderer_modes
            .insert(33, OmpRendererMode::ServerPrivateGuestPty);
        service.bound_apps.insert(11, 33);
        service.bound_apps.insert(33, 33);
        service.route_bindings.insert(11, old.clone());
        service.route_bindings.insert(33, new.clone());
        let clients = HashMap::from([
            (
                11,
                client(
                    crate::server::clients::ClientConnectionMode::OmpPane,
                    None,
                    Some("profile-a"),
                ),
            ),
            (
                33,
                client(
                    crate::server::clients::ClientConnectionMode::App,
                    Some("Ada"),
                    Some("profile-a"),
                ),
            ),
        ]);

        service.disconnect(11, &clients);

        assert_eq!(service.route_bindings.get(&33), Some(&new));
        let frame =
            crate::protocol::encode_omp_frame(OmpFrameDirection::GuestToHost, br#"{"t":"hello"}"#)
                .unwrap();
        assert!(matches!(
            service
                .routes
                .guest_frame(33, &old, old_epoch, frame.clone()),
            Err(OmpRouteError::StaleAttachment)
        ));
        assert!(service
            .routes
            .guest_frame(33, &new, new_epoch, frame)
            .is_ok());
    }

    #[test]
    fn bound_app_disconnect_detaches_renderer_route_and_clears_peer_state() {
        let mut service = OmpService::new(None).unwrap();
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        service.routes.host_started(key.clone()).unwrap();
        let deliveries = service.routes.attach(22, &key).unwrap();
        service.routes.attach(33, &key).unwrap();
        let attachment_epoch = deliveries
            .iter()
            .find_map(|delivery| match delivery {
                OmpRouteDelivery::Pane {
                    client_id: 22,
                    attachment_epoch,
                    ..
                } => Some(*attachment_epoch),
                _ => None,
            })
            .unwrap();
        service
            .renderer_modes
            .insert(11, OmpRendererMode::ClientLocalNative);
        service
            .renderer_modes
            .insert(33, OmpRendererMode::ClientLocalNative);
        service.bound_apps.insert(11, 22);
        service.route_bindings.insert(11, key.clone());
        service.bound_apps.insert(33, 33);
        service.route_bindings.insert(33, key.clone());

        let clients = HashMap::from([
            (
                11,
                client(
                    crate::server::clients::ClientConnectionMode::OmpPane,
                    None,
                    Some("profile-a"),
                ),
            ),
            (
                33,
                client(
                    crate::server::clients::ClientConnectionMode::OmpPane,
                    None,
                    Some("profile-b"),
                ),
            ),
            (
                22,
                client(
                    crate::server::clients::ClientConnectionMode::App,
                    Some("Ada"),
                    Some("profile-a"),
                ),
            ),
        ]);
        let messages = service.disconnect(22, &clients);

        assert!(!service.bound_apps.contains_key(&11));
        assert!(!service.renderer_modes.contains_key(&11));
        assert!(!service.route_bindings.contains_key(&11));
        assert!(messages.iter().any(|(client_id, message)| {
            *client_id == 33 && matches!(message, ServerMessage::OmpPane { .. })
        }));
        assert!(messages.iter().any(|(client_id, message)| {
            *client_id == 11
                && matches!(
                    message,
                    ServerMessage::ServerShutdown { reason: Some(reason) }
                        if reason == "bound App disconnected"
                )
        }));
        let frame =
            crate::protocol::encode_omp_frame(OmpFrameDirection::GuestToHost, br#"{"t":"hello"}"#)
                .unwrap();
        assert!(matches!(
            service
                .routes
                .guest_frame(22, &key, attachment_epoch, frame),
            Err(OmpRouteError::StaleAttachment)
        ));
    }

    #[test]
    fn pane_client_cannot_attach_to_multiple_routes() {
        let mut service = OmpService::new(None).unwrap();
        let first = OmpRouteKey {
            pane_id: "first".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        let second = OmpRouteKey {
            pane_id: "second".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
        service.routes.host_started(first.clone()).unwrap();
        service.routes.host_started(second.clone()).unwrap();
        let target =
            |key: &OmpRouteKey, launch_id| crate::server::clients::OmpRendererTargetState {
                launch_id,
                route: Some(crate::protocol::OmpRendererRoute {
                    pane_id: key.pane_id.clone(),
                    omp_session_id: key.omp_session_id.clone(),
                    route_generation: key.route_generation,
                }),
                bound: false,
                ready: false,
                prefix: crate::protocol::OmpRendererPrefix {
                    code: crate::protocol::ClientKeyCode::Char('a'),
                    modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
                },
                surface_active: false,
            };
        let mut app = client(
            crate::server::clients::ClientConnectionMode::App,
            Some("Ada"),
            Some("profile-a"),
        );
        app.omp_renderer_capabilities.client_local_native = true;
        app.omp_renderer_target = Some(target(&first, 1));
        let mut clients = HashMap::from([
            (
                11,
                client(
                    crate::server::clients::ClientConnectionMode::OmpPane,
                    None,
                    Some("profile-a"),
                ),
            ),
            (22, app),
        ]);
        let attach = |key: &OmpRouteKey, launch_id| ServerEvent::OmpPaneAttach {
            client_id: 11,
            pane_id: key.pane_id.clone(),
            omp_session_id: key.omp_session_id.clone(),
            route_generation: key.route_generation,
            target_app_client_id: Some(22),
            renderer_capabilities: crate::protocol::OmpRendererCapabilities {
                client_local_native: true,
            },
            renderer_launch_id: Some(launch_id),
            renderer_request: crate::protocol::OmpRendererRequest::Independent,
            external_peer_identity: None,
        };
        service.handle_event(attach(&first, 1), true, &clients);
        clients.get_mut(&22).unwrap().omp_renderer_target = Some(target(&second, 2));
        let messages = service.handle_event(attach(&second, 2), true, &clients);
        assert!(matches!(
            messages.as_slice(),
            [(11, ServerMessage::OmpError { code, .. })] if code == "route_busy"
        ));
        assert_eq!(service.route_bindings.get(&11), Some(&first));
    }
}
