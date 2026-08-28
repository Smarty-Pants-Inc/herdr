//! Herdr-owned opaque OMP pane routing. Payload JSON is validated at ingress, never rewritten.

use std::collections::{BTreeMap, HashMap};

use crate::protocol::{validate_omp_frame, OmpControlAction, OmpFrameDirection, OmpPaneState};

const INITIAL_ROUTE_GENERATION: u64 = 1;

fn validate_route_frame(frame: &[u8], direction: OmpFrameDirection) -> Result<(), OmpRouteError> {
    let payload = validate_omp_frame(frame, direction)
        .map_err(|err| OmpRouteError::InvalidFrame(err.to_string()))?;
    serde_json::from_slice::<serde::de::IgnoredAny>(payload)
        .map_err(|err| OmpRouteError::InvalidFrame(format!("invalid OMP JSON payload: {err}")))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OmpRouteKey {
    pub(crate) pane_id: String,
    pub(crate) omp_session_id: String,
    pub(crate) route_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OmpRouteError {
    UnknownRoute,
    StaleGeneration,
    StaleAttachment,
    HostUnavailable,
    RouteBusy,
    ControllerRequired,
    InvalidFrame(String),
}

impl OmpRouteError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::UnknownRoute => "unknown_route",
            Self::StaleGeneration => "stale_generation",
            Self::StaleAttachment => "stale_attachment",
            Self::HostUnavailable => "host_unavailable",
            Self::RouteBusy => "route_busy",
            Self::ControllerRequired => "controller_required",
            Self::InvalidFrame(_) => "invalid_frame",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OmpRouteDelivery {
    Guest {
        client_id: u64,
        attachment_epoch: u64,
        frame: Vec<u8>,
    },
    HostFrame {
        client_id: u64,
        frame: Vec<u8>,
    },
    HostControl {
        client_id: u64,
        action: OmpControlAction,
    },
    Pane {
        client_id: u64,
        attachment_epoch: u64,
        controller: bool,
        state: OmpPaneState,
    },
}
#[derive(Debug)]
struct OmpRoute {
    key: OmpRouteKey,
    live: bool,
    failed: bool,
    next_attachment_epoch: u64,
    attachments: BTreeMap<u64, u64>,
    controller: Option<(u64, u64)>,
}

impl OmpRoute {
    fn state(&self) -> OmpPaneState {
        let controller_client_id = self.controller.map(|(client_id, _)| client_id);
        if self.failed {
            OmpPaneState::Failed {
                controller_client_id,
            }
        } else if self.live {
            OmpPaneState::Live {
                controller_client_id,
            }
        } else {
            OmpPaneState::Starting {
                controller_client_id,
            }
        }
    }

    fn attachment(&self, client_id: u64, epoch: u64) -> Result<(), OmpRouteError> {
        match self.attachments.get(&client_id) {
            Some(current) if *current == epoch => Ok(()),
            Some(_) => Err(OmpRouteError::StaleAttachment),
            None => Err(OmpRouteError::StaleAttachment),
        }
    }

    fn require_live(&self) -> Result<(), OmpRouteError> {
        if self.live {
            Ok(())
        } else {
            Err(OmpRouteError::HostUnavailable)
        }
    }

    fn pane_deliveries(&self) -> Vec<OmpRouteDelivery> {
        self.attachments
            .iter()
            .map(|(&client_id, &attachment_epoch)| OmpRouteDelivery::Pane {
                client_id,
                attachment_epoch,
                controller: self.controller == Some((client_id, attachment_epoch)),
                state: self.state(),
            })
            .collect()
    }
}

/// All logical OMP routing state, deliberately independent of render state and client geometry.
#[derive(Debug, Default)]
pub(crate) struct OmpRouteRegistry {
    routes: HashMap<(String, String), OmpRoute>,
    /// Last server-assigned generation per durable pane slot, retained after pruning.
    route_generations: HashMap<String, u64>,
}
impl OmpRouteRegistry {
    /// Validates an expected-current host claim and returns the server-assigned key.
    /// The announcement never chooses the next generation.
    pub(crate) fn prepare_host_start(
        &self,
        announced: &OmpRouteKey,
    ) -> Result<OmpRouteKey, OmpRouteError> {
        let mut key = announced.clone();
        if key.route_generation == 0 {
            return Err(OmpRouteError::StaleGeneration);
        }
        key.route_generation = match self.route_generations.get(&key.pane_id).copied() {
            None if announced.route_generation == INITIAL_ROUTE_GENERATION => {
                INITIAL_ROUTE_GENERATION
            }
            None => return Err(OmpRouteError::StaleGeneration),
            Some(current) if announced.route_generation == current => current
                .checked_add(1)
                .ok_or(OmpRouteError::StaleGeneration)?,
            Some(_) => return Err(OmpRouteError::StaleGeneration),
        };
        if self.routes.iter().any(|((pane_id, _), route)| {
            pane_id == &key.pane_id && (route.live || !route.attachments.is_empty())
        }) {
            return Err(OmpRouteError::RouteBusy);
        }
        Ok(key)
    }

    pub(crate) fn commit_host_start(&mut self, key: OmpRouteKey) -> Vec<OmpRouteDelivery> {
        let pane_id = key.pane_id.clone();
        let map_key = (pane_id.clone(), key.omp_session_id.clone());
        debug_assert!(self.routes.iter().all(|((current_pane_id, _), route)| {
            current_pane_id != &pane_id || route.attachments.is_empty()
        }));
        self.routes
            .retain(|(current_pane_id, _), _| current_pane_id != &pane_id);
        self.routes.insert(
            map_key,
            OmpRoute {
                key: key.clone(),
                live: true,
                failed: false,
                next_attachment_epoch: 1,
                attachments: BTreeMap::new(),
                controller: None,
            },
        );
        self.route_generations.insert(pane_id, key.route_generation);
        Vec::new()
    }

    pub(crate) fn host_started(
        &mut self,
        announced: OmpRouteKey,
    ) -> Result<Vec<OmpRouteDelivery>, OmpRouteError> {
        let key = self.prepare_host_start(&announced)?;
        Ok(self.commit_host_start(key))
    }

    pub(crate) fn host_stopped(
        &mut self,
        key: &OmpRouteKey,
    ) -> Result<Vec<OmpRouteDelivery>, OmpRouteError> {
        let route = self.route_mut(&key.pane_id, &key.omp_session_id, key.route_generation)?;
        route.live = false;
        route.failed = true;
        Ok(route.pane_deliveries())
    }

    pub(crate) fn remove_if_inactive_and_empty(&mut self, key: &OmpRouteKey) {
        let map_key = (key.pane_id.clone(), key.omp_session_id.clone());
        let removable = self.routes.get(&map_key).is_some_and(|route| {
            route.key.route_generation == key.route_generation
                && !route.live
                && route.attachments.is_empty()
        });
        if removable {
            self.routes.remove(&map_key);
        }
    }

    fn route_mut(
        &mut self,
        pane_id: &str,
        session_id: &str,
        generation: u64,
    ) -> Result<&mut OmpRoute, OmpRouteError> {
        let route = self
            .routes
            .get_mut(&(pane_id.to_owned(), session_id.to_owned()))
            .ok_or(OmpRouteError::UnknownRoute)?;
        if route.key.route_generation != generation {
            return Err(OmpRouteError::StaleGeneration);
        }
        Ok(route)
    }

    pub(crate) fn attach(
        &mut self,
        client_id: u64,
        key: &OmpRouteKey,
    ) -> Result<Vec<OmpRouteDelivery>, OmpRouteError> {
        self.attach_with_controller(client_id, key, true)
    }

    /// Attaches a bridge guest without granting controller authority.
    pub(crate) fn attach_observer(
        &mut self,
        client_id: u64,
        key: &OmpRouteKey,
    ) -> Result<Vec<OmpRouteDelivery>, OmpRouteError> {
        self.route_mut(&key.pane_id, &key.omp_session_id, key.route_generation)?
            .require_live()?;
        self.attach_with_controller(client_id, key, false)
    }

    fn attach_with_controller(
        &mut self,
        client_id: u64,
        key: &OmpRouteKey,
        assign_controller: bool,
    ) -> Result<Vec<OmpRouteDelivery>, OmpRouteError> {
        let route = self.route_mut(&key.pane_id, &key.omp_session_id, key.route_generation)?;
        let epoch = match route.attachments.get(&client_id) {
            Some(&epoch) => epoch,
            None => {
                let epoch = route.next_attachment_epoch;
                route.next_attachment_epoch = route.next_attachment_epoch.saturating_add(1);
                route.attachments.insert(client_id, epoch);
                epoch
            }
        };
        if assign_controller && route.controller.is_none() {
            route.controller = Some((client_id, epoch));
        }
        Ok(route.pane_deliveries())
    }

    pub(crate) fn detach(
        &mut self,
        client_id: u64,
        key: &OmpRouteKey,
        epoch: u64,
    ) -> Result<Vec<OmpRouteDelivery>, OmpRouteError> {
        let route = self.route_mut(&key.pane_id, &key.omp_session_id, key.route_generation)?;
        route.attachment(client_id, epoch)?;
        route.attachments.remove(&client_id);
        if route.controller == Some((client_id, epoch)) {
            route.controller = None;
        }
        Ok(route.pane_deliveries())
    }

    pub(crate) fn disconnect(
        &mut self,
        client_id: u64,
    ) -> Vec<(OmpRouteKey, Vec<OmpRouteDelivery>)> {
        let mut routes = Vec::new();
        for route in self.routes.values_mut() {
            let Some(epoch) = route.attachments.remove(&client_id) else {
                continue;
            };
            if route.controller == Some((client_id, epoch)) {
                route.controller = None;
            }
            routes.push((route.key.clone(), route.pane_deliveries()));
        }
        routes
    }
    pub(crate) fn disconnect_from_route(
        &mut self,
        client_id: u64,
        key: &OmpRouteKey,
    ) -> Option<Vec<OmpRouteDelivery>> {
        let route = self
            .route_mut(&key.pane_id, &key.omp_session_id, key.route_generation)
            .ok()?;
        let epoch = route.attachments.remove(&client_id)?;
        if route.controller == Some((client_id, epoch)) {
            route.controller = None;
        }
        Some(route.pane_deliveries())
    }

    pub(crate) fn guest_frame(
        &mut self,
        client_id: u64,
        key: &OmpRouteKey,
        epoch: u64,
        frame: Vec<u8>,
    ) -> Result<OmpRouteDelivery, OmpRouteError> {
        let route = self.route_mut(&key.pane_id, &key.omp_session_id, key.route_generation)?;
        route.attachment(client_id, epoch)?;
        route.require_live()?;
        validate_route_frame(&frame, OmpFrameDirection::GuestToHost)?;
        Ok(OmpRouteDelivery::HostFrame { client_id, frame })
    }

    pub(crate) fn control(
        &mut self,
        client_id: u64,
        key: &OmpRouteKey,
        epoch: u64,
        action: OmpControlAction,
    ) -> Result<Vec<OmpRouteDelivery>, OmpRouteError> {
        let route = self.route_mut(&key.pane_id, &key.omp_session_id, key.route_generation)?;
        route.attachment(client_id, epoch)?;
        match action {
            OmpControlAction::RequestController => {
                if route.controller.is_none() {
                    route.controller = Some((client_id, epoch));
                }
                Ok(route.pane_deliveries())
            }
            OmpControlAction::ReleaseController => {
                if route.controller == Some((client_id, epoch)) {
                    route.controller = None;
                }
                Ok(route.pane_deliveries())
            }
            OmpControlAction::Mutation { ref frame } => {
                route.require_live()?;
                if route.controller != Some((client_id, epoch)) {
                    return Err(OmpRouteError::ControllerRequired);
                }
                validate_route_frame(frame, OmpFrameDirection::GuestToHost)?;
                Ok(vec![OmpRouteDelivery::HostControl { client_id, action }])
            }
        }
    }

    pub(crate) fn host_frame(
        &mut self,
        key: &OmpRouteKey,
        target_client_id: Option<u64>,
        frame: Vec<u8>,
    ) -> Result<Vec<OmpRouteDelivery>, OmpRouteError> {
        let route = self.route_mut(&key.pane_id, &key.omp_session_id, key.route_generation)?;
        route.require_live()?;
        validate_route_frame(&frame, OmpFrameDirection::HostToGuest)?;
        let deliveries = route
            .attachments
            .iter()
            .filter(|(client_id, _)| target_client_id.is_none_or(|target| **client_id == target))
            .map(|(&client_id, &attachment_epoch)| OmpRouteDelivery::Guest {
                client_id,
                attachment_epoch,
                frame: frame.clone(),
            })
            .collect();
        Ok(deliveries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::encode_omp_frame;

    fn key() -> OmpRouteKey {
        OmpRouteKey {
            pane_id: "p".into(),
            omp_session_id: "s".into(),
            route_generation: 1,
        }
    }
    fn guest(payload: &[u8]) -> Vec<u8> {
        let payload = serde_json::to_vec(std::str::from_utf8(payload).unwrap()).unwrap();
        encode_omp_frame(OmpFrameDirection::GuestToHost, &payload).unwrap()
    }
    fn host(payload: &[u8]) -> Vec<u8> {
        let payload = serde_json::to_vec(std::str::from_utf8(payload).unwrap()).unwrap();
        encode_omp_frame(OmpFrameDirection::HostToGuest, &payload).unwrap()
    }

    fn raw(direction: OmpFrameDirection, payload: &[u8]) -> Vec<u8> {
        encode_omp_frame(direction, payload).unwrap()
    }
    fn epoch(items: &[OmpRouteDelivery], client_id: u64) -> u64 {
        items
            .iter()
            .find_map(|item| match item {
                OmpRouteDelivery::Pane {
                    client_id: id,
                    attachment_epoch,
                    ..
                } if *id == client_id => Some(*attachment_epoch),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn route_rejects_non_json_and_non_utf8_payloads_at_ingress() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let epoch = epoch(&routes.attach(1, &key()).unwrap(), 1);

        for payload in [b"not-json".as_slice(), &[0xff, 0xfe]] {
            assert!(matches!(
                routes.guest_frame(
                    1,
                    &key(),
                    epoch,
                    raw(OmpFrameDirection::GuestToHost, payload),
                ),
                Err(OmpRouteError::InvalidFrame(_))
            ));
            assert!(matches!(
                routes.host_frame(&key(), None, raw(OmpFrameDirection::HostToGuest, payload),),
                Err(OmpRouteError::InvalidFrame(_))
            ));
        }

        assert!(matches!(
            routes.control(
                1,
                &key(),
                epoch,
                OmpControlAction::Mutation {
                    frame: raw(OmpFrameDirection::GuestToHost, b"not-json"),
                },
            ),
            Err(OmpRouteError::InvalidFrame(_))
        ));
    }

    #[test]
    fn observer_attach_keeps_controller_unassigned_until_requested() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let attached = routes.attach_observer(10, &key()).unwrap();
        let attachment_epoch = epoch(&attached, 10);
        assert!(attached.iter().all(|delivery| matches!(
            delivery,
            OmpRouteDelivery::Pane {
                controller: false,
                ..
            }
        )));
        let requested = routes
            .control(
                10,
                &key(),
                attachment_epoch,
                OmpControlAction::RequestController,
            )
            .unwrap();
        assert!(requested.iter().all(|delivery| matches!(
            delivery,
            OmpRouteDelivery::Pane {
                controller: true,
                ..
            }
        )));
    }

    #[test]
    fn observer_attach_rejects_retained_non_live_route() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let existing = routes.attach_observer(10, &key()).unwrap();
        assert!(routes.host_stopped(&key()).is_ok());
        assert!(matches!(
            routes.attach_observer(20, &key()),
            Err(OmpRouteError::HostUnavailable)
        ));
        assert_eq!(epoch(&existing, 10), 1);
    }

    #[test]
    fn two_clients_controller_and_host_survives_detach() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let first = routes.attach(10, &key()).unwrap();
        let first_epoch = epoch(&first, 10);
        let second = routes.attach(20, &key()).unwrap();
        let second_epoch = epoch(&second, 20);
        assert!(matches!(
            routes
                .guest_frame(20, &key(), second_epoch, guest(b"hello"))
                .unwrap(),
            OmpRouteDelivery::HostFrame { client_id: 20, .. }
        ));
        assert!(matches!(
            routes.control(
                20,
                &key(),
                second_epoch,
                OmpControlAction::Mutation {
                    frame: guest(b"no")
                },
            ),
            Err(OmpRouteError::ControllerRequired)
        ));
        assert!(matches!(
            routes
                .guest_frame(10, &key(), first_epoch, guest(b"yes"))
                .unwrap(),
            OmpRouteDelivery::HostFrame { .. }
        ));
        let released = routes
            .control(10, &key(), first_epoch, OmpControlAction::ReleaseController)
            .unwrap();
        assert!(released.iter().all(|delivery| matches!(
            delivery,
            OmpRouteDelivery::Pane {
                controller: false,
                ..
            }
        )));
        assert!(matches!(
            routes.control(
                10,
                &key(),
                first_epoch,
                OmpControlAction::Mutation {
                    frame: guest(b"stale")
                },
            ),
            Err(OmpRouteError::ControllerRequired)
        ));
        let promoted = routes
            .control(
                20,
                &key(),
                second_epoch,
                OmpControlAction::RequestController,
            )
            .unwrap();
        assert_eq!(epoch(&promoted, 20), second_epoch);
        assert!(matches!(
            promoted.as_slice(),
            [
                OmpRouteDelivery::Pane {
                    client_id: 10,
                    controller: false,
                    ..
                },
                OmpRouteDelivery::Pane {
                    client_id: 20,
                    controller: true,
                    ..
                }
            ]
        ));
        assert!(matches!(
            routes.control(
                20,
                &key(),
                second_epoch,
                OmpControlAction::Mutation { frame: guest(b"yes") },
            ),
            Ok(deliveries) if matches!(deliveries.as_slice(), [OmpRouteDelivery::HostControl { client_id: 20, .. }])
        ));
    }

    #[test]
    fn stale_epoch_generation_and_ordered_targeted_broadcast_are_rejected_or_preserved() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let a = epoch(&routes.attach(1, &key()).unwrap(), 1);
        let b = epoch(&routes.attach(2, &key()).unwrap(), 2);
        let stale = OmpRouteKey {
            route_generation: 2,
            ..key()
        };
        assert!(matches!(
            routes.attach(3, &stale),
            Err(OmpRouteError::StaleGeneration)
        ));
        assert!(matches!(
            routes.guest_frame(1, &key(), a + 1, guest(b"bad")),
            Err(OmpRouteError::StaleAttachment)
        ));
        let broadcast = routes.host_frame(&key(), None, host(b"first")).unwrap();
        assert_eq!(
            broadcast
                .iter()
                .map(|delivery| match delivery {
                    OmpRouteDelivery::Guest {
                        client_id,
                        attachment_epoch,
                        ..
                    } => (*client_id, *attachment_epoch),
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>(),
            vec![(1, a), (2, b)]
        );
        let targeted = routes.host_frame(&key(), Some(2), host(b"second")).unwrap();
        assert!(
            matches!(targeted.as_slice(), [OmpRouteDelivery::Guest { client_id: 2, attachment_epoch, .. }] if *attachment_epoch == b)
        );
    }

    #[test]
    fn attachment_epoch_is_stable_across_controller_leases_and_changes_only_on_reattach() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let first_epoch = epoch(&routes.attach(1, &key()).unwrap(), 1);
        let second_epoch = epoch(&routes.attach(2, &key()).unwrap(), 2);

        let released = routes
            .control(1, &key(), first_epoch, OmpControlAction::ReleaseController)
            .unwrap();
        assert_eq!(epoch(&released, 1), first_epoch);
        assert_eq!(epoch(&released, 2), second_epoch);

        let handed_off = routes
            .control(2, &key(), second_epoch, OmpControlAction::RequestController)
            .unwrap();
        assert_eq!(epoch(&handed_off, 1), first_epoch);
        assert_eq!(epoch(&handed_off, 2), second_epoch);
        assert!(matches!(
            handed_off.as_slice(),
            [
                OmpRouteDelivery::Pane {
                    client_id: 1,
                    controller: false,
                    ..
                },
                OmpRouteDelivery::Pane {
                    client_id: 2,
                    controller: true,
                    ..
                }
            ]
        ));

        let occupied = routes
            .control(1, &key(), first_epoch, OmpControlAction::RequestController)
            .unwrap();
        assert_eq!(epoch(&occupied, 1), first_epoch);
        assert_eq!(epoch(&occupied, 2), second_epoch);
        assert!(matches!(
            occupied.as_slice(),
            [
                OmpRouteDelivery::Pane {
                    client_id: 1,
                    controller: false,
                    ..
                },
                OmpRouteDelivery::Pane {
                    client_id: 2,
                    controller: true,
                    ..
                }
            ]
        ));
        assert!(matches!(
            routes.control(
                2,
                &key(),
                second_epoch + 1,
                OmpControlAction::ReleaseController
            ),
            Err(OmpRouteError::StaleAttachment)
        ));

        let disconnected = routes.disconnect(1);
        assert!(matches!(
            disconnected.as_slice(),
            [(route, deliveries)] if route == &key()
                && matches!(deliveries.as_slice(), [OmpRouteDelivery::Pane {
                    client_id: 2,
                    attachment_epoch,
                    controller: true,
                    ..
                }] if *attachment_epoch == second_epoch)
        ));
        assert!(matches!(
            routes.guest_frame(1, &key(), first_epoch, guest(b"detached")),
            Err(OmpRouteError::StaleAttachment)
        ));

        routes.disconnect(2);
        let reattached_epoch = epoch(&routes.attach(1, &key()).unwrap(), 1);
        assert!(reattached_epoch > second_epoch);
    }

    #[test]
    fn simultaneous_routes_keep_membership_and_controller_leases_isolated() {
        let mut routes = OmpRouteRegistry::default();
        let route_a = key();
        let route_b = OmpRouteKey {
            pane_id: "p2".into(),
            omp_session_id: "s2".into(),
            ..key()
        };
        routes.host_started(route_a.clone()).unwrap();
        routes.host_started(route_b.clone()).unwrap();
        let a1 = epoch(&routes.attach(1, &route_a).unwrap(), 1);
        let a2 = epoch(&routes.attach(2, &route_a).unwrap(), 2);
        let a3 = epoch(&routes.attach(3, &route_a).unwrap(), 3);
        let b1 = epoch(&routes.attach(4, &route_b).unwrap(), 4);
        let _b2 = epoch(&routes.attach(5, &route_b).unwrap(), 5);

        routes
            .control(1, &route_a, a1, OmpControlAction::ReleaseController)
            .unwrap();
        let promoted = routes
            .control(2, &route_a, a2, OmpControlAction::RequestController)
            .unwrap();
        assert_eq!(epoch(&promoted, 2), a2);
        let losing_request = routes
            .control(3, &route_a, a3, OmpControlAction::RequestController)
            .unwrap();
        assert!(losing_request.iter().any(|delivery| matches!(
            delivery,
            OmpRouteDelivery::Pane {
                client_id: 2,
                controller: true,
                ..
            }
        )));
        assert!(matches!(
            routes.control(
                3,
                &route_a,
                a3,
                OmpControlAction::Mutation {
                    frame: guest(b"no")
                },
            ),
            Err(OmpRouteError::ControllerRequired)
        ));
        assert!(matches!(
            routes.control(
                2,
                &route_a,
                a2,
                OmpControlAction::Mutation { frame: guest(b"a") },
            ),
            Ok(deliveries) if matches!(deliveries.as_slice(), [OmpRouteDelivery::HostControl { client_id: 2, .. }])
        ));
        assert!(matches!(
            routes.control(
                4,
                &route_b,
                b1,
                OmpControlAction::Mutation { frame: guest(b"b") },
            ),
            Ok(deliveries) if matches!(deliveries.as_slice(), [OmpRouteDelivery::HostControl { client_id: 4, .. }])
        ));

        let a_broadcast = routes.host_frame(&route_a, None, host(b"a")).unwrap();
        let b_broadcast = routes.host_frame(&route_b, None, host(b"b")).unwrap();
        assert_eq!(
            a_broadcast
                .iter()
                .filter_map(|delivery| match delivery {
                    OmpRouteDelivery::Guest { client_id, .. } => Some(*client_id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            b_broadcast
                .iter()
                .filter_map(|delivery| match delivery {
                    OmpRouteDelivery::Guest { client_id, .. } => Some(*client_id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![4, 5]
        );
    }

    #[test]
    fn stopped_host_rejects_frames_and_restarts_cleanly() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let attached = routes.attach(1, &key()).unwrap();
        let first_epoch = epoch(&attached, 1);
        let failed = routes.host_stopped(&key()).unwrap();

        assert!(failed.iter().all(|delivery| matches!(
            delivery,
            OmpRouteDelivery::Pane {
                state: OmpPaneState::Failed { .. },
                ..
            }
        )));
        assert!(matches!(
            routes.guest_frame(1, &key(), first_epoch, guest(b"offline")),
            Err(OmpRouteError::HostUnavailable)
        ));
        assert!(matches!(
            routes.host_frame(&key(), None, host(b"offline")),
            Err(OmpRouteError::HostUnavailable)
        ));

        assert_eq!(routes.host_started(key()), Err(OmpRouteError::RouteBusy));
        assert!(matches!(
            routes.guest_frame(1, &key(), first_epoch, guest(b"still-offline")),
            Err(OmpRouteError::HostUnavailable)
        ));
    }

    #[test]
    fn host_admission_rejects_busy_replacement_and_stale_or_forged_claims() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let attached = routes.attach(1, &key()).unwrap();
        let epoch = epoch(&attached, 1);

        assert_eq!(routes.host_started(key()), Err(OmpRouteError::RouteBusy));
        let forged = OmpRouteKey {
            route_generation: key().route_generation + 1,
            ..key()
        };
        assert_eq!(
            routes.host_started(forged),
            Err(OmpRouteError::StaleGeneration)
        );
        let stale = OmpRouteKey {
            route_generation: 0,
            ..key()
        };
        assert_eq!(
            routes.host_started(stale),
            Err(OmpRouteError::StaleGeneration)
        );
        assert!(routes
            .guest_frame(1, &key(), epoch, guest(b"still-live"))
            .is_ok());
    }

    #[test]
    fn stopped_host_restarts_at_the_next_generation_after_all_guests_detach() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let attached = routes.attach(1, &key()).unwrap();
        let epoch = epoch(&attached, 1);
        routes.host_stopped(&key()).unwrap();

        assert_eq!(routes.host_started(key()), Err(OmpRouteError::RouteBusy));
        routes.detach(1, &key(), epoch).unwrap();
        assert_eq!(routes.host_started(key()), Ok(Vec::new()));
        let replacement = OmpRouteKey {
            route_generation: 2,
            ..key()
        };
        assert!(matches!(
            routes.attach(2, &key()),
            Err(OmpRouteError::StaleGeneration)
        ));
        assert!(routes.attach(2, &replacement).is_ok());
    }

    #[test]
    fn inactive_empty_route_retains_generation_history_after_pruning() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        let attached = routes.attach(1, &key()).unwrap();
        let epoch = epoch(&attached, 1);
        routes.host_stopped(&key()).unwrap();
        routes.detach(1, &key(), epoch).unwrap();
        routes.remove_if_inactive_and_empty(&key());

        assert!(matches!(
            routes.attach(1, &key()),
            Err(OmpRouteError::UnknownRoute)
        ));
        let caller_chosen_next = OmpRouteKey {
            route_generation: 2,
            ..key()
        };
        assert_eq!(
            routes.host_started(caller_chosen_next),
            Err(OmpRouteError::StaleGeneration)
        );
        assert_eq!(routes.host_started(key()), Ok(Vec::new()));
        let replacement = OmpRouteKey {
            route_generation: 2,
            ..key()
        };
        assert!(matches!(
            routes.attach(1, &key()),
            Err(OmpRouteError::StaleGeneration)
        ));
        assert!(routes.attach(1, &replacement).is_ok());
    }

    #[test]
    fn changed_session_in_the_same_pane_advances_the_generation() {
        let mut routes = OmpRouteRegistry::default();
        routes.host_started(key()).unwrap();
        routes.host_stopped(&key()).unwrap();
        routes.remove_if_inactive_and_empty(&key());

        let announced = OmpRouteKey {
            omp_session_id: "replacement-session".into(),
            ..key()
        };
        assert_eq!(routes.host_started(announced.clone()), Ok(Vec::new()));
        let replacement = OmpRouteKey {
            route_generation: 2,
            ..announced
        };
        assert!(matches!(
            routes.attach(1, &key()),
            Err(OmpRouteError::UnknownRoute)
        ));
        assert!(routes.attach(1, &replacement).is_ok());
    }
}
