//! OMP bridge ownership and route delivery for the headless server.

use std::collections::HashMap;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::SyncSender;

use tokio::sync::mpsc;

use crate::protocol::{
    select_omp_renderer, OmpControlAction, OmpFrameDirection, OmpRendererMode, ServerMessage,
};
use crate::server::client_transport::ServerEvent;
use crate::server::clients::ClientConnection;
use crate::server::omp_bridge;
use crate::server::omp_route::{OmpRouteDelivery, OmpRouteError, OmpRouteKey, OmpRouteRegistry};

pub(crate) struct OmpService {
    listener: TcpListener,
    bridge: crate::pane::OmpBridgeEnv,
    handshakes: omp_bridge::HandshakeLimiter,
    hosts: HashMap<(String, String, u64), (u64, SyncSender<String>, TcpStream)>,
    renderer_modes: HashMap<u64, OmpRendererMode>,
    bound_apps: HashMap<u64, u64>,
    route_bindings: HashMap<u64, OmpRouteKey>,
    routes: OmpRouteRegistry,
}

impl OmpService {
    pub(crate) fn new(
        prepared: Option<(TcpListener, crate::pane::OmpBridgeEnv)>,
    ) -> io::Result<Self> {
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
            routes: OmpRouteRegistry::default(),
        })
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
        }
        let mut messages = Vec::new();
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
                renderer_request,
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
                let app_client_id =
                    match self.bind_app_client(client_id, target_app_client_id, clients) {
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
                    };
                if self.app_has_native_renderer(app_client_id) {
                    self.send_error(&mut messages, client_id, pane_id, OmpRouteError::RouteBusy);
                    return messages;
                }
                match self.routes.attach(app_client_id, &key) {
                    Ok(deliveries) => {
                        self.renderer_modes.insert(client_id, renderer_mode);
                        self.bound_apps.insert(client_id, app_client_id);
                        self.route_bindings.insert(client_id, key.clone());
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
            } => {
                if !valid_route_id(&pane_id) || !valid_route_id(&omp_session_id) {
                    tracing::warn!("rejected oversized OMP host route identifier");
                    let _ = socket.shutdown(Shutdown::Both);
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
                match self.routes.host_started(key.clone()) {
                    Ok(deliveries) => {
                        self.replace_host(route_key, host_id, outbound, socket);
                        if self.sync_authority(&key, &deliveries, &mut messages, clients) {
                            self.deliver(&key, deliveries, &mut messages, clients);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            pane_id = %key.pane_id,
                            omp_session_id = %key.omp_session_id,
                            route_generation = key.route_generation,
                            code = error.code(),
                            "rejected OMP bridge host"
                        );
                        let _ = socket.shutdown(Shutdown::Both);
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
    pub(crate) fn app_client_for_renderer(
        &self,
        client_id: u64,
        target_app_client_id: Option<u64>,
        clients: &HashMap<u64, ClientConnection>,
    ) -> Option<u64> {
        self.bind_app_client(client_id, target_app_client_id, clients)
    }
    fn renderer_for_peer(&self, peer_id: u64, key: &OmpRouteKey) -> Option<u64> {
        self.bound_apps
            .iter()
            .filter_map(|(&renderer_id, &app_id)| {
                (app_id == peer_id && self.route_bindings.get(&renderer_id) == Some(key))
                    .then_some(renderer_id)
            })
            .max_by_key(|renderer_id| {
                self.renderer_modes.get(renderer_id) == Some(&OmpRendererMode::ClientLocalNative)
            })
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
        clients: &HashMap<u64, ClientConnection>,
    ) -> Option<u64> {
        let renderer_binding_token = clients
            .get(&pane_client_id)?
            .renderer_binding_token
            .as_deref()?;
        let matches_renderer = |client: &ClientConnection| {
            client.is_full_app_client()
                && client.committed_identity().is_some()
                && client.renderer_binding_token.as_deref() == Some(renderer_binding_token)
        };
        if let Some(target_app_client_id) = target_app_client_id {
            return matches_renderer(clients.get(&target_app_client_id)?)
                .then_some(target_app_client_id);
        }
        let mut matches = clients
            .iter()
            .filter_map(|(&client_id, client)| matches_renderer(client).then_some(client_id));
        let app_client_id = matches.next()?;
        matches.next().is_none().then_some(app_client_id)
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
                    let Some(client_id) = self.renderer_for_peer(peer_id, key) else {
                        continue;
                    };
                    messages.push((
                        client_id,
                        ServerMessage::OmpFrame {
                            pane_id: key.pane_id.clone(),
                            omp_session_id: key.omp_session_id.clone(),
                            route_generation: key.route_generation,
                            attachment_epoch,
                            frame,
                        },
                    ));
                }
                OmpRouteDelivery::Pane {
                    client_id: peer_id,
                    attachment_epoch,
                    controller,
                    state,
                } => {
                    let Some(client_id) = self.renderer_for_peer(peer_id, key) else {
                        continue;
                    };
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
                            state,
                        },
                    ));
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
        let Some(identity) = clients
            .get(&client_id)
            .and_then(ClientConnection::committed_identity)
        else {
            return true;
        };
        let Some(record) =
            omp_bridge::guest_record(client_id, frame, &identity.name, identity.revision)
        else {
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
    }
}

impl Drop for OmpService {
    fn drop(&mut self) {
        for (_, (_, _, socket)) in self.hosts.drain() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

fn valid_route_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= crate::protocol::MAX_OMP_ROUTE_ID_BYTES
        && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn host_socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
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
    fn renderer_binding_selects_an_exact_live_committed_app() {
        let mut service = OmpService::new(None).unwrap();
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
        clients.insert(
            2,
            client_with_binding(
                crate::server::clients::ClientConnectionMode::App,
                Some("Other Name"),
                Some("profile-a"),
                Some("shared-token"),
            ),
        );
        clients.insert(
            3,
            client_with_binding(
                crate::server::clients::ClientConnectionMode::App,
                Some("Chosen Name"),
                Some("profile-a"),
                Some("shared-token"),
            ),
        );
        clients.insert(
            4,
            client_with_binding(
                crate::server::clients::ClientConnectionMode::App,
                Some("Chosen Name"),
                Some("profile-a"),
                Some("other-token"),
            ),
        );

        assert_eq!(
            service.bind_app_client(1, Some(3), &clients),
            Some(3),
            "the exact matching App is selected regardless of names"
        );
        let key = OmpRouteKey {
            pane_id: "pane".into(),
            omp_session_id: "session".into(),
            route_generation: 1,
        };
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
                renderer_request: crate::protocol::OmpRendererRequest::Independent,
            },
            true,
            &clients,
        );
        assert_eq!(service.bound_app_for_renderer(1), Some(3));
        assert!(!messages
            .iter()
            .any(|(_, message)| matches!(message, ServerMessage::OmpError { .. })));

        assert_eq!(
            service.bind_app_client(1, None, &clients),
            None,
            "an omitted target remains ambiguous"
        );
        assert_eq!(
            service.bind_app_client(1, Some(99), &clients),
            None,
            "a missing target is rejected"
        );
        assert_eq!(
            service.bind_app_client(1, Some(4), &clients),
            None,
            "a shared frontend profile ID cannot override a different binding token"
        );

        clients
            .get_mut(&3)
            .unwrap()
            .identity
            .as_mut()
            .unwrap()
            .committed = None;
        assert_eq!(
            service.bind_app_client(1, Some(3), &clients),
            None,
            "the exact App must have committed identity"
        );

        clients.remove(&3);
        assert_eq!(
            service.bind_app_client(1, None, &clients),
            Some(2),
            "the old single-match path still binds"
        );
        service.bound_apps.insert(1, 2);
        service.disconnect(2, &clients);
        assert!(
            !service.bound_apps.contains_key(&1),
            "App disconnect must invalidate an existing pane binding"
        );
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
                22,
                client(
                    crate::server::clients::ClientConnectionMode::App,
                    Some("Ada"),
                    Some("profile-a"),
                ),
            ),
        ]);
        let attach = |key: &OmpRouteKey| ServerEvent::OmpPaneAttach {
            client_id: 11,
            pane_id: key.pane_id.clone(),
            omp_session_id: key.omp_session_id.clone(),
            route_generation: key.route_generation,
            target_app_client_id: None,
            renderer_capabilities: crate::protocol::OmpRendererCapabilities {
                client_local_native: true,
            },
            renderer_request: crate::protocol::OmpRendererRequest::Independent,
        };
        service.handle_event(attach(&first), true, &clients);
        let messages = service.handle_event(attach(&second), true, &clients);
        assert!(matches!(
            messages.as_slice(),
            [(11, ServerMessage::OmpError { code, .. })] if code == "route_busy"
        ));
        assert_eq!(service.route_bindings.get(&11), Some(&first));
    }
}
