//! Host-wide OMP admission maintenance lease.
//!
//! The JSON API owns this state. OMP hosts only consult it at the existing
//! admission boundary, and live handoff only carries the active lease proof.

use std::collections::HashSet;
#[cfg(unix)]
use std::ffi::{CStr, CString, OsString};
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::api::schema::{
    ServerOmpMaintenancePermit, ServerOmpMaintenanceRoute, ServerOmpMaintenanceStatus,
};
use crate::server::omp_route::OmpRouteKey;

const STATE_VERSION: u32 = 1;
const STATUS_SCHEMA: &str = "herdr.omp_maintenance.v1";
const OPERATION_ID_BYTES: usize = 32;
const ENCODED_TOKEN_BYTES: usize = 43;
const MAX_COMPLETED_OPERATIONS: usize = 64;
const MAX_STATE_FILE_BYTES: usize = 64 * 1024;
const INSTANCE_DIRECTORY: &str = "omp-maintenance-v1.instances";
const INSTANCE_CREATE_ATTEMPTS: usize = 8;

// Keep the app-directory name aligned with config without inheriting its path selection.

#[cfg(any(unix, all(windows, not(test))))]
fn maintenance_app_dir_name() -> &'static str {
    if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    }
}

#[derive(Debug, Clone)]
pub(crate) enum OmpMaintenanceError {
    InvalidRequest(String),
    Conflict(String),
    NotOwner(String),
    RoutesLive(usize),
    StateInvalid(String),
    StateIo(String),
}

impl OmpMaintenanceError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "omp_maintenance_invalid_request",
            Self::Conflict(_) => "omp_maintenance_conflict",
            Self::NotOwner(_) => "omp_maintenance_not_owner",
            Self::RoutesLive(_) => "omp_maintenance_routes_live",
            Self::StateInvalid(_) => "omp_maintenance_state_invalid",
            Self::StateIo(_) => "omp_maintenance_state_unavailable",
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::InvalidRequest(message)
            | Self::Conflict(message)
            | Self::NotOwner(message)
            | Self::StateInvalid(message)
            | Self::StateIo(message) => message.clone(),
            Self::RoutesLive(count) => {
                format!("OMP maintenance requires zero live routes; found {count}")
            }
        }
    }
}

#[derive(Debug)]
pub(crate) enum OmpMaintenanceAdmissionError<E> {
    Active,
    State(OmpMaintenanceError),
    Route(E),
}

#[cfg(any(unix, test))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct OmpMaintenanceHandoffState {
    pub(crate) owner_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permit: Option<ServerOmpMaintenancePermit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedState {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lease: Option<PersistedLease>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    completed_operations: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    routes: Vec<PersistedRoute>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            lease: None,
            completed_operations: Vec::new(),
            routes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedLease {
    owner_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permit: Option<ServerOmpMaintenancePermit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedRoute {
    server_instance_id: String,
    session: String,
    pane_id: String,
    omp_session_id: String,
    route_generation: u64,
    proof: bool,
}

impl PersistedRoute {
    fn new(instance_id: &str, session: &str, key: &OmpRouteKey, proof: bool) -> Self {
        Self {
            server_instance_id: instance_id.to_owned(),
            session: session.to_owned(),
            pane_id: key.pane_id.clone(),
            omp_session_id: key.omp_session_id.clone(),
            route_generation: key.route_generation,
            proof,
        }
    }

    fn matches(&self, instance_id: &str, session: &str, key: &OmpRouteKey) -> bool {
        self.server_instance_id == instance_id
            && self.session == session
            && self.pane_id == key.pane_id
            && self.omp_session_id == key.omp_session_id
            && self.route_generation == key.route_generation
    }

    fn public(&self) -> ServerOmpMaintenanceRoute {
        ServerOmpMaintenanceRoute {
            session: self.session.clone(),
            pane_id: self.pane_id.clone(),
            omp_session_id: self.omp_session_id.clone(),
            route_generation: self.route_generation,
            proof: self.proof,
        }
    }
}

enum Backend {
    File {
        state_root: PathBuf,
        state_path: PathBuf,
        lock_path: PathBuf,
        instance_dir: PathBuf,
    },
    #[cfg(test)]
    Memory(TestBackend),
}

struct ServerInstance {
    id: String,
    lock_path: Option<PathBuf>,
    lock: Option<File>,
}

impl ServerInstance {
    fn file(state_root: &Path, state_path: &Path) -> Result<Self, OmpMaintenanceError> {
        validate_pinned_state_root(state_root)
            .map_err(|error| state_io(state_root, "validate state directory", error))?;
        let instance_dir = state_root.join(INSTANCE_DIRECTORY);
        ensure_private_directory(&instance_dir)
            .map_err(|error| state_io(&instance_dir, "prepare server-instance directory", error))?;
        sync_parent_directory(state_root)
            .map_err(|error| state_io(state_root, "sync state directory", error))?;

        validate_private_entry_if_present(state_path)
            .map_err(|error| state_io(state_path, "validate state entry", error))?;
        let state_lock_path = state_path.with_extension("lock");
        validate_private_entry_if_present(&state_lock_path)
            .map_err(|error| state_io(&state_lock_path, "validate state lock entry", error))?;
        for _ in 0..INSTANCE_CREATE_ATTEMPTS {
            let id = random_token()?;
            let lock_path = instance_lock_path(&instance_dir, &id);
            let mut lock = match open_private_new(&lock_path) {
                Ok(lock) => lock,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(state_io(&lock_path, "create server-instance lock", error));
                }
            };
            if let Err(error) = lock.lock() {
                let _ = fs::remove_file(&lock_path);
                return Err(state_io(&lock_path, "lock server-instance identity", error));
            }
            if let Err(error) = lock
                .write_all(id.as_bytes())
                .and_then(|()| lock.write_all(b"\n"))
                .and_then(|()| lock.sync_all())
                .and_then(|()| sync_parent_directory(&instance_dir))
            {
                drop(lock);
                let _ = fs::remove_file(&lock_path);
                return Err(state_io(
                    &lock_path,
                    "persist server-instance identity",
                    error,
                ));
            }
            return Ok(Self {
                id,
                lock_path: Some(lock_path),
                lock: Some(lock),
            });
        }

        Err(OmpMaintenanceError::StateIo(
            "failed to allocate a unique OMP maintenance server-instance identity".into(),
        ))
    }

    #[cfg(test)]
    fn memory() -> Self {
        Self {
            id: random_token().expect("test server-instance identity"),
            lock_path: None,
            lock: None,
        }
    }

    fn retire(&mut self) {
        let Some(lock_path) = self.lock_path.take() else {
            return;
        };
        drop(self.lock.take());
        if let Err(error) = fs::remove_file(&lock_path) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %lock_path.display(),
                    %error,
                    "failed to retire OMP maintenance server-instance identity"
                );
            }
        }
    }
}
#[cfg(test)]
struct TestBackendState {
    state: PersistedState,
    unregister_failures: usize,
    state_failures: usize,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct TestOmpMaintenanceStore(Arc<Mutex<TestBackendState>>);

#[cfg(test)]
impl TestOmpMaintenanceStore {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(TestBackendState {
            state: PersistedState::default(),
            unregister_failures: 0,
            state_failures: 0,
        })))
    }

    pub(crate) fn fail_next_unregisters(&self, count: usize) {
        self.0
            .lock()
            .expect("test maintenance state")
            .unregister_failures = count;
    }

    pub(crate) fn fail_next_state_accesses(&self, count: usize) {
        self.0
            .lock()
            .expect("test maintenance state")
            .state_failures = count;
    }
}

#[cfg(test)]
struct TestBackend {
    store: Arc<Mutex<TestBackendState>>,
}

pub(crate) struct OmpMaintenance {
    session: String,
    backend: Backend,
    instance: ServerInstance,
}

impl OmpMaintenance {
    #[cfg(not(test))]
    pub(crate) fn host(session: String) -> Result<Self, OmpMaintenanceError> {
        Self::file(session, host_state_root()?.join("omp-maintenance-v1.json"))
    }

    fn file(session: String, state_path: PathBuf) -> Result<Self, OmpMaintenanceError> {
        let file_name = state_path.file_name().ok_or_else(|| {
            OmpMaintenanceError::StateIo("OMP maintenance state path has no file name".into())
        })?;
        let state_root = state_path.parent().ok_or_else(|| {
            OmpMaintenanceError::StateIo("OMP maintenance state path has no parent".into())
        })?;
        let state_root = prepare_trusted_state_root(state_root)
            .map_err(|error| state_io(state_root, "prepare state directory", error))?;
        let state_path = state_root.join(file_name);
        let lock_path = state_path.with_extension("lock");
        let instance_dir = state_root.join(INSTANCE_DIRECTORY);
        let instance = ServerInstance::file(&state_root, &state_path)?;
        Ok(Self {
            session,
            backend: Backend::File {
                state_root,
                state_path,
                lock_path,
                instance_dir,
            },
            instance,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(session: &str, store: TestOmpMaintenanceStore) -> Self {
        Self {
            session: session.to_owned(),
            backend: Backend::Memory(TestBackend { store: store.0 }),
            instance: ServerInstance::memory(),
        }
    }

    #[cfg(test)]
    fn file_for_test(session: &str, state_path: PathBuf) -> Result<Self, OmpMaintenanceError> {
        Self::file(session.to_owned(), state_path)
    }

    pub(crate) fn status(&self) -> Result<ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        self.with_state(|state, _dirty| state.status())
    }
    #[cfg(not(test))]
    pub(crate) fn inspect_host() -> Result<ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        let state_root = host_state_root()?;
        inspect_file_state(
            &state_root,
            &state_root.join("omp-maintenance-v1.json"),
            &state_root.join("omp-maintenance-v1.lock"),
        )
    }

    #[cfg(test)]
    pub(crate) fn inspect_host() -> Result<ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        Err(OmpMaintenanceError::StateIo(
            "host-wide OMP maintenance inspection is unavailable in this test backend".into(),
        ))
    }

    pub(crate) fn inspect(&self) -> Result<ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        match &self.backend {
            Backend::File {
                state_root,
                state_path,
                lock_path,
                ..
            } => inspect_file_state(state_root, state_path, lock_path),
            #[cfg(test)]
            Backend::Memory(backend) => {
                let backend = backend.store.lock().map_err(|_| {
                    OmpMaintenanceError::StateIo("OMP maintenance state lock is poisoned".into())
                })?;
                backend.state.validate()?;
                Ok(backend.state.status())
            }
        }
    }

    pub(crate) fn acquire(
        &self,
        operation_id: &str,
    ) -> Result<ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        validate_operation_id(operation_id)?;
        let owner_hash = operation_owner_hash(operation_id);
        self.with_state(|state, dirty| {
            if state
                .completed_operations
                .iter()
                .any(|completed| owner_matches(completed, &owner_hash))
            {
                return Ok(state.status());
            }
            if let Some(lease) = &state.lease {
                if !owner_matches(&lease.owner_hash, &owner_hash) {
                    return Err(OmpMaintenanceError::Conflict(
                        "OMP maintenance is already held".into(),
                    ));
                }
                return Ok(state.status());
            }
            state.lease = Some(PersistedLease {
                owner_hash,
                permit: None,
            });
            *dirty = true;
            Ok(state.status())
        })?
    }

    pub(crate) fn grant_permit(
        &self,
        operation_id: &str,
        session: &str,
        pane_id: &str,
    ) -> Result<ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        validate_operation_id(operation_id)?;
        validate_session(session)?;
        validate_pane_id(pane_id)?;
        let owner_hash = operation_owner_hash(operation_id);
        self.with_state(|state, dirty| {
            let Some(lease) = state.lease.as_mut() else {
                return Err(OmpMaintenanceError::NotOwner(
                    "OMP maintenance ownership proof is invalid".into(),
                ));
            };
            if !owner_matches(&lease.owner_hash, &owner_hash) {
                return Err(OmpMaintenanceError::NotOwner(
                    "OMP maintenance ownership proof is invalid".into(),
                ));
            }
            let permit = ServerOmpMaintenancePermit {
                session: session.to_owned(),
                pane_id: pane_id.to_owned(),
            };
            if !state.routes.is_empty() {
                if state.routes.len() == 1 {
                    let route = &state.routes[0];
                    if route.proof
                        && route.session == permit.session
                        && route.pane_id == permit.pane_id
                    {
                        return Ok(state.status());
                    }
                }
                return Err(OmpMaintenanceError::RoutesLive(state.routes.len()));
            }
            if let Some(current) = &lease.permit {
                if current == &permit {
                    return Ok(state.status());
                }
                return Err(OmpMaintenanceError::Conflict(format!(
                    "OMP maintenance already has a permit for session {} pane {}",
                    current.session, current.pane_id
                )));
            }
            lease.permit = Some(permit);
            *dirty = true;
            Ok(state.status())
        })?
    }

    pub(crate) fn release(
        &self,
        operation_id: &str,
    ) -> Result<ServerOmpMaintenanceStatus, OmpMaintenanceError> {
        validate_operation_id(operation_id)?;
        let owner_hash = operation_owner_hash(operation_id);
        self.with_state(|state, dirty| {
            if state
                .completed_operations
                .iter()
                .any(|completed| owner_matches(completed, &owner_hash))
            {
                return Ok(state.status());
            }
            let Some(lease) = &state.lease else {
                return Err(OmpMaintenanceError::NotOwner(
                    "OMP maintenance ownership proof is invalid".into(),
                ));
            };
            if !owner_matches(&lease.owner_hash, &owner_hash) {
                return Err(OmpMaintenanceError::NotOwner(
                    "OMP maintenance ownership proof is invalid".into(),
                ));
            }
            if !state.routes.is_empty() {
                return Err(OmpMaintenanceError::RoutesLive(state.routes.len()));
            }
            let Some(released) = state.lease.take() else {
                return Err(OmpMaintenanceError::StateInvalid(
                    "OMP maintenance lease disappeared during release".into(),
                ));
            };
            if state.completed_operations.len() == MAX_COMPLETED_OPERATIONS {
                state.completed_operations.remove(0);
            }
            state.completed_operations.push(released.owner_hash);
            *dirty = true;
            Ok(state.status())
        })?
    }

    pub(crate) fn admit<T, E>(
        &self,
        key: &OmpRouteKey,
        admit_route: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, OmpMaintenanceAdmissionError<E>> {
        self.with_state(|state, dirty| {
            let proof = match state.lease.as_mut() {
                None => false,
                Some(lease) => {
                    let Some(permit) = lease.permit.as_ref() else {
                        return Err(OmpMaintenanceAdmissionError::Active);
                    };
                    if permit.session != self.session || permit.pane_id != key.pane_id {
                        return Err(OmpMaintenanceAdmissionError::Active);
                    }
                    true
                }
            };

            let admitted = admit_route().map_err(OmpMaintenanceAdmissionError::Route)?;
            if proof {
                if let Some(lease) = state.lease.as_mut() {
                    lease.permit = None;
                }
            }
            state.routes.push(PersistedRoute::new(
                &self.instance.id,
                &self.session,
                key,
                proof,
            ));
            *dirty = true;
            Ok(admitted)
        })
        .map_err(OmpMaintenanceAdmissionError::State)?
    }
    pub(crate) fn unregister_route(&self, key: &OmpRouteKey) -> Result<(), OmpMaintenanceError> {
        #[cfg(test)]
        if let Backend::Memory(backend) = &self.backend {
            let mut backend = backend.store.lock().map_err(|_| {
                OmpMaintenanceError::StateIo("OMP maintenance state lock is poisoned".into())
            })?;
            if backend.unregister_failures != 0 {
                backend.unregister_failures -= 1;
                return Err(OmpMaintenanceError::StateIo(
                    "injected OMP maintenance unregister failure".into(),
                ));
            }
        }

        self.with_state(|state, dirty| {
            let before = state.routes.len();
            state
                .routes
                .retain(|route| !route.matches(&self.instance.id, &self.session, key));
            *dirty |= state.routes.len() != before;
        })
    }

    pub(crate) fn routes_to_drain(
        &self,
        local_routes: &[OmpRouteKey],
    ) -> Result<Vec<OmpRouteKey>, OmpMaintenanceError> {
        self.with_state(|state, _dirty| {
            if state.lease.is_none() {
                return Vec::new();
            }
            local_routes
                .iter()
                .filter(|key| {
                    !state
                        .routes
                        .iter()
                        .find(|route| route.matches(&self.instance.id, &self.session, key))
                        .is_some_and(|route| route.proof)
                })
                .cloned()
                .collect()
        })
    }

    #[cfg(any(unix, test))]
    pub(crate) fn handoff_state(
        &self,
    ) -> Result<Option<OmpMaintenanceHandoffState>, OmpMaintenanceError> {
        self.with_state(|state, _dirty| {
            if state.lease.is_some() && !state.routes.is_empty() {
                return Err(OmpMaintenanceError::RoutesLive(state.routes.len()));
            }
            Ok(state
                .lease
                .as_ref()
                .map(|lease| OmpMaintenanceHandoffState {
                    owner_hash: lease.owner_hash.clone(),
                    permit: lease.permit.clone(),
                }))
        })?
    }

    #[cfg(any(unix, test))]
    pub(crate) fn validate_handoff_state(
        &self,
        expected: Option<&OmpMaintenanceHandoffState>,
    ) -> Result<(), OmpMaintenanceError> {
        self.with_state(|state, _dirty| {
            if expected.is_some() && !state.routes.is_empty() {
                return Err(OmpMaintenanceError::RoutesLive(state.routes.len()));
            }
            let actual = state
                .lease
                .as_ref()
                .map(|lease| OmpMaintenanceHandoffState {
                    owner_hash: lease.owner_hash.clone(),
                    permit: lease.permit.clone(),
                });
            if actual.as_ref() == expected {
                Ok(())
            } else {
                Err(OmpMaintenanceError::StateInvalid(
                    "OMP maintenance changed during live handoff".into(),
                ))
            }
        })?
    }

    pub(crate) fn retire_instance(&mut self) {
        self.instance.retire();
    }

    fn with_state<T>(
        &self,
        apply: impl FnOnce(&mut PersistedState, &mut bool) -> T,
    ) -> Result<T, OmpMaintenanceError> {
        match &self.backend {
            Backend::File {
                state_root,
                state_path,
                lock_path,
                instance_dir,
            } => with_file_state(
                state_root,
                state_path,
                lock_path,
                instance_dir,
                &self.instance.id,
                apply,
            ),
            #[cfg(test)]
            Backend::Memory(backend) => {
                let mut backend = backend.store.lock().map_err(|_| {
                    OmpMaintenanceError::StateIo("OMP maintenance state lock is poisoned".into())
                })?;
                if backend.state_failures != 0 {
                    backend.state_failures -= 1;
                    return Err(OmpMaintenanceError::StateIo(
                        "injected OMP maintenance state failure".into(),
                    ));
                }
                backend.state.validate()?;
                let mut dirty = false;
                let result = apply(&mut backend.state, &mut dirty);
                if dirty {
                    backend.state.validate()?;
                }
                Ok(result)
            }
        }
    }
}

impl PersistedState {
    fn status(&self) -> ServerOmpMaintenanceStatus {
        let mut routes = self
            .routes
            .iter()
            .map(PersistedRoute::public)
            .collect::<Vec<_>>();
        routes.sort_by(|left, right| {
            (
                &left.session,
                &left.pane_id,
                &left.omp_session_id,
                left.route_generation,
            )
                .cmp(&(
                    &right.session,
                    &right.pane_id,
                    &right.omp_session_id,
                    right.route_generation,
                ))
        });
        ServerOmpMaintenanceStatus {
            schema: STATUS_SCHEMA.into(),
            held: self.lease.is_some(),
            permit: self.lease.as_ref().and_then(|lease| lease.permit.clone()),
            route_count: routes.len(),
            routes,
        }
    }

    fn validate(&self) -> Result<(), OmpMaintenanceError> {
        if self.version != STATE_VERSION {
            return Err(OmpMaintenanceError::StateInvalid(format!(
                "unsupported OMP maintenance state version {}",
                self.version
            )));
        }
        if let Some(lease) = &self.lease {
            validate_owner_hash(&lease.owner_hash)?;
            if self
                .completed_operations
                .iter()
                .any(|completed| owner_matches(completed, &lease.owner_hash))
            {
                return Err(OmpMaintenanceError::StateInvalid(
                    "active OMP maintenance operation is already completed".into(),
                ));
            }
            if let Some(permit) = &lease.permit {
                validate_session(&permit.session)
                    .map_err(|error| OmpMaintenanceError::StateInvalid(error.message()))?;
                validate_pane_id(&permit.pane_id)
                    .map_err(|error| OmpMaintenanceError::StateInvalid(error.message()))?;
                if !self.routes.is_empty() {
                    return Err(OmpMaintenanceError::StateInvalid(
                        "OMP maintenance permit cannot coexist with live routes".into(),
                    ));
                }
            }
        }

        if self.completed_operations.len() > MAX_COMPLETED_OPERATIONS {
            return Err(OmpMaintenanceError::StateInvalid(format!(
                "OMP maintenance contains more than {MAX_COMPLETED_OPERATIONS} completed operations"
            )));
        }
        let mut completed = HashSet::new();
        for owner_hash in &self.completed_operations {
            validate_owner_hash(owner_hash)?;
            if !completed.insert(owner_hash.as_str()) {
                return Err(OmpMaintenanceError::StateInvalid(
                    "OMP maintenance contains a duplicate completed operation".into(),
                ));
            }
        }

        let mut keys = HashSet::new();
        let mut proof_routes = 0usize;
        for route in &self.routes {
            validate_server_instance_id(&route.server_instance_id)?;
            validate_session(&route.session)
                .map_err(|error| OmpMaintenanceError::StateInvalid(error.message()))?;
            validate_pane_id(&route.pane_id)
                .map_err(|error| OmpMaintenanceError::StateInvalid(error.message()))?;
            validate_pane_id(&route.omp_session_id)
                .map_err(|error| OmpMaintenanceError::StateInvalid(error.message()))?;
            if !keys.insert((
                route.session.as_str(),
                route.pane_id.as_str(),
                route.omp_session_id.as_str(),
                route.route_generation,
            )) {
                return Err(OmpMaintenanceError::StateInvalid(
                    "OMP maintenance contains a duplicate route".into(),
                ));
            }
            proof_routes += usize::from(route.proof);
        }
        if self.lease.is_none() && proof_routes != 0 {
            return Err(OmpMaintenanceError::StateInvalid(
                "OMP proof route exists without a maintenance lease".into(),
            ));
        }
        if proof_routes > 1 || (proof_routes == 1 && self.routes.len() != 1) {
            return Err(OmpMaintenanceError::StateInvalid(
                "OMP maintenance allows only one proof route".into(),
            ));
        }
        Ok(())
    }
}

fn validate_operation_id(operation_id: &str) -> Result<(), OmpMaintenanceError> {
    if valid_encoded_token(operation_id) {
        Ok(())
    } else {
        Err(OmpMaintenanceError::InvalidRequest(format!(
            "operation_id must be canonical unpadded base64url encoding of {OPERATION_ID_BYTES} random bytes"
        )))
    }
}

fn validate_owner_hash(owner_hash: &str) -> Result<(), OmpMaintenanceError> {
    if valid_encoded_token(owner_hash) {
        Ok(())
    } else {
        Err(OmpMaintenanceError::StateInvalid(
            "OMP maintenance owner hash is invalid".into(),
        ))
    }
}

fn validate_server_instance_id(instance_id: &str) -> Result<(), OmpMaintenanceError> {
    if valid_encoded_token(instance_id) {
        Ok(())
    } else {
        Err(OmpMaintenanceError::StateInvalid(
            "OMP maintenance server-instance identity is invalid".into(),
        ))
    }
}

fn valid_encoded_token(value: &str) -> bool {
    if value.len() != ENCODED_TOKEN_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return false;
    }
    let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value) else {
        return false;
    };
    decoded.len() == OPERATION_ID_BYTES
        && base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(decoded) == value
}

fn random_token() -> Result<String, OmpMaintenanceError> {
    let mut bytes = [0u8; OPERATION_ID_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| {
        OmpMaintenanceError::StateIo(format!(
            "failed to generate an OMP maintenance random token: {error}"
        ))
    })?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn operation_owner_hash(operation_id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(operation_id.as_bytes()))
}

fn owner_matches(expected: &str, candidate: &str) -> bool {
    expected.len() == candidate.len()
        && expected
            .bytes()
            .zip(candidate.bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

fn validate_session(session: &str) -> Result<(), OmpMaintenanceError> {
    crate::session::validate_name(session).map_err(OmpMaintenanceError::InvalidRequest)
}

fn validate_pane_id(pane_id: &str) -> Result<(), OmpMaintenanceError> {
    if pane_id.is_empty()
        || pane_id.len() > crate::protocol::MAX_OMP_ROUTE_ID_BYTES
        || pane_id.chars().any(char::is_control)
    {
        return Err(OmpMaintenanceError::InvalidRequest(
            "OMP maintenance pane identifier is invalid".into(),
        ));
    }
    Ok(())
}

fn inspect_file_state(
    state_root: &Path,
    state_path: &Path,
    lock_path: &Path,
) -> Result<ServerOmpMaintenanceStatus, OmpMaintenanceError> {
    if state_path.parent() != Some(state_root) || lock_path.parent() != Some(state_root) {
        return Err(OmpMaintenanceError::StateIo(
            "OMP maintenance inspection path escaped its trusted root".into(),
        ));
    }
    let pinned_root = match fs::symlink_metadata(state_root) {
        Ok(_) => pin_trusted_state_root(state_root)
            .map_err(|error| state_io(state_root, "validate state directory", error))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            validate_existing_state_root_ancestry(state_root)
                .map_err(|error| state_io(state_root, "validate state ancestry", error))?;
            return Ok(PersistedState::default().status());
        }
        Err(error) => {
            return Err(state_io(state_root, "inspect state directory", error));
        }
    };
    let state_name = state_path.file_name().ok_or_else(|| {
        OmpMaintenanceError::StateIo("OMP maintenance state path has no file name".into())
    })?;
    let lock_name = lock_path.file_name().ok_or_else(|| {
        OmpMaintenanceError::StateIo("OMP maintenance lock path has no file name".into())
    })?;
    let state_path = pinned_root.join(state_name);
    let lock_path = pinned_root.join(lock_name);
    validate_private_entry_if_present(&state_path)
        .map_err(|error| state_io(&state_path, "validate state entry", error))?;
    validate_private_entry_if_present(&lock_path)
        .map_err(|error| state_io(&lock_path, "validate state lock entry", error))?;
    let state_exists = match fs::symlink_metadata(&state_path) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(state_io(&state_path, "inspect state entry", error)),
    };
    if !state_exists {
        return Ok(PersistedState::default().status());
    }
    let lock = open_private(&lock_path, false, false)
        .map_err(|error| state_io(&lock_path, "open state lock", error))?;
    lock.lock()
        .map_err(|error| state_io(&lock_path, "lock state", error))?;
    let state = load_state(&state_path)?;
    state.validate()?;
    Ok(state.status())
}

fn with_file_state<T>(
    state_root: &Path,
    state_path: &Path,
    lock_path: &Path,
    instance_dir: &Path,
    current_instance_id: &str,
    apply: impl FnOnce(&mut PersistedState, &mut bool) -> T,
) -> Result<T, OmpMaintenanceError> {
    if state_path.parent() != Some(state_root)
        || lock_path.parent() != Some(state_root)
        || instance_dir.parent() != Some(state_root)
    {
        return Err(OmpMaintenanceError::StateIo(
            "OMP maintenance state path escaped its trusted root".into(),
        ));
    }
    validate_pinned_state_root(state_root)
        .map_err(|error| state_io(state_root, "validate state directory", error))?;
    validate_private_directory(instance_dir)
        .map_err(|error| state_io(instance_dir, "validate server-instance directory", error))?;
    validate_private_entry_if_present(state_path)
        .map_err(|error| state_io(state_path, "validate state entry", error))?;
    validate_private_entry_if_present(lock_path)
        .map_err(|error| state_io(lock_path, "validate state lock entry", error))?;
    let lock = open_private(lock_path, true, true)
        .map_err(|error| state_io(lock_path, "open state lock", error))?;
    lock.lock()
        .map_err(|error| state_io(lock_path, "lock state", error))?;

    let mut state = load_state(state_path)?;
    state.validate()?;
    let mut dirty = reconcile_stale_routes(&mut state, instance_dir, current_instance_id)?;
    let result = apply(&mut state, &mut dirty);
    if dirty {
        state.validate()?;
        save_state(state_path, &state)?;
    }
    Ok(result)
}

fn reconcile_stale_routes(
    state: &mut PersistedState,
    instance_dir: &Path,
    current_instance_id: &str,
) -> Result<bool, OmpMaintenanceError> {
    let instance_ids = state
        .routes
        .iter()
        .filter(|route| route.server_instance_id != current_instance_id)
        .map(|route| route.server_instance_id.clone())
        .collect::<HashSet<_>>();
    let mut stale = HashSet::new();
    for instance_id in instance_ids {
        let path = instance_lock_path(instance_dir, &instance_id);
        let mut file = match open_private(&path, false, true) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(OmpMaintenanceError::StateInvalid(format!(
                    "OMP maintenance route has no server-instance identity {}",
                    path.display()
                )));
            }
            Err(error) => return Err(state_io(&path, "open server-instance identity", error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| state_io(&path, "inspect server-instance identity", error))?;
        if metadata.len() > (ENCODED_TOKEN_BYTES + 1) as u64 {
            return Err(OmpMaintenanceError::StateInvalid(format!(
                "OMP maintenance server-instance identity {} is invalid",
                path.display()
            )));
        }
        let mut identity = String::new();
        file.read_to_string(&mut identity)
            .map_err(|error| state_io(&path, "read server-instance identity", error))?;
        if identity.trim_end() != instance_id {
            return Err(OmpMaintenanceError::StateInvalid(format!(
                "OMP maintenance server-instance identity {} does not match persisted state",
                path.display()
            )));
        }
        match file.try_lock() {
            Ok(()) => {
                stale.insert(instance_id);
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(state_io(&path, "probe server-instance liveness", error));
            }
        }
    }
    if stale.is_empty() {
        return Ok(false);
    }
    state
        .routes
        .retain(|route| !stale.contains(&route.server_instance_id));
    Ok(true)
}

fn load_state(path: &Path) -> Result<PersistedState, OmpMaintenanceError> {
    let file = match open_private(path, false, false) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(PersistedState::default());
        }
        Err(error) => return Err(state_io(path, "open state", error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| state_io(path, "inspect state", error))?;
    if metadata.len() > MAX_STATE_FILE_BYTES as u64 {
        return Err(OmpMaintenanceError::StateInvalid(format!(
            "OMP maintenance state {} exceeds {MAX_STATE_FILE_BYTES} bytes",
            path.display()
        )));
    }
    let mut content = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_STATE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut content)
        .map_err(|error| state_io(path, "read state", error))?;
    if content.len() > MAX_STATE_FILE_BYTES {
        return Err(OmpMaintenanceError::StateInvalid(format!(
            "OMP maintenance state {} exceeds {MAX_STATE_FILE_BYTES} bytes",
            path.display()
        )));
    }
    serde_json::from_slice(&content).map_err(|error| {
        OmpMaintenanceError::StateInvalid(format!(
            "failed to parse OMP maintenance state {}: {error}",
            path.display()
        ))
    })
}

fn save_state(path: &Path, state: &PersistedState) -> Result<(), OmpMaintenanceError> {
    save_state_with_hook(path, state, |_| Ok(()))
}

fn save_state_with_hook(
    path: &Path,
    state: &PersistedState,
    before_replace: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<(), OmpMaintenanceError> {
    let content = serde_json::to_vec(state).map_err(|error| {
        OmpMaintenanceError::StateInvalid(format!(
            "failed to encode OMP maintenance state: {error}"
        ))
    })?;
    if content.len() >= MAX_STATE_FILE_BYTES {
        return Err(OmpMaintenanceError::StateInvalid(format!(
            "OMP maintenance state exceeds {MAX_STATE_FILE_BYTES} bytes"
        )));
    }
    let parent = path.parent().ok_or_else(|| {
        OmpMaintenanceError::StateIo("OMP maintenance state path has no parent".into())
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("omp-maintenance-state");
    let mut temporary = None;
    for _ in 0..INSTANCE_CREATE_ATTEMPTS {
        let candidate = parent.join(format!(".{file_name}.{}.tmp", random_token()?));
        match open_private_new(&candidate) {
            Ok(file) => {
                temporary = Some((TemporaryStateFile::new(candidate), file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(state_io(&candidate, "create state temp file", error)),
        }
    }
    let Some((mut temporary, mut file)) = temporary else {
        return Err(OmpMaintenanceError::StateIo(
            "failed to allocate a unique OMP maintenance state temp file".into(),
        ));
    };
    file.write_all(&content)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|error| state_io(temporary.path(), "write state temp file", error))?;
    drop(file);
    before_replace(temporary.path())
        .map_err(|error| state_io(temporary.path(), "prepare state replacement", error))?;
    replace_state_file(temporary.path(), path)
        .map_err(|error| state_io(path, "replace state", error))?;
    temporary.persist();
    sync_parent_directory(parent).map_err(|error| state_io(parent, "sync state directory", error))
}

struct TemporaryStateFile {
    path: PathBuf,
    persisted: bool,
}

impl TemporaryStateFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            persisted: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn persist(&mut self) {
        self.persisted = true;
    }
}

impl Drop for TemporaryStateFile {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn instance_lock_path(instance_dir: &Path, instance_id: &str) -> PathBuf {
    instance_dir.join(format!("server-{instance_id}.lock"))
}

#[cfg(all(not(test), unix))]
fn host_state_root() -> Result<PathBuf, OmpMaintenanceError> {
    unix_host_state_root()
}

#[cfg(all(not(test), windows))]
fn host_state_root() -> Result<PathBuf, OmpMaintenanceError> {
    windows_host_state_root()
}

#[cfg(all(not(test), not(any(unix, windows))))]
fn host_state_root() -> Result<PathBuf, OmpMaintenanceError> {
    Err(OmpMaintenanceError::StateIo(
        "OMP maintenance requires a trusted per-user state directory".into(),
    ))
}

#[cfg(unix)]
fn unix_host_state_root() -> Result<PathBuf, OmpMaintenanceError> {
    #[cfg(debug_assertions)]
    if let Some(root) = std::env::var_os("HERDR_TEST_OMP_MAINTENANCE_STATE_ROOT") {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err(OmpMaintenanceError::StateIo(
                "test OMP maintenance state root must be absolute".into(),
            ));
        }
        return Ok(root);
    }
    Ok(unix_account_home()?
        .join(".local")
        .join("state")
        .join(maintenance_app_dir_name()))
}

#[cfg(unix)]
fn unix_account_home() -> Result<PathBuf, OmpMaintenanceError> {
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let size = if suggested > 0 {
        usize::try_from(suggested).unwrap_or(16 * 1024)
    } else {
        16 * 1024
    }
    .clamp(1024, 1024 * 1024);
    let mut buffer = vec![0u8; size];
    let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            effective_uid(),
            record.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() {
        return Err(OmpMaintenanceError::StateIo(format!(
            "failed to resolve the current OS account home ({status})"
        )));
    }
    let record = unsafe { record.assume_init() };
    if record.pw_dir.is_null() {
        return Err(OmpMaintenanceError::StateIo(
            "the current OS account has no home directory".into(),
        ));
    }
    let bytes = unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes();
    let home = PathBuf::from(OsString::from_vec(bytes.to_vec()));
    if !home.is_absolute() {
        return Err(OmpMaintenanceError::StateIo(
            "the current OS account home is not absolute".into(),
        ));
    }
    Ok(home)
}

#[cfg(all(windows, not(test)))]
fn windows_host_state_root() -> Result<PathBuf, OmpMaintenanceError> {
    let local_app_data = windows_local_app_data()?;
    let local_app_data = prepare_trusted_state_root(&local_app_data)
        .map_err(|error| state_io(&local_app_data, "validate LocalAppData", error))?;
    let state_root = prepare_trusted_state_root(&local_app_data.join(maintenance_app_dir_name()))
        .map_err(|error| {
        state_io(
            &local_app_data,
            "prepare maintenance state directory",
            error,
        )
    })?;
    if state_root.parent() != Some(local_app_data.as_path()) {
        return Err(OmpMaintenanceError::StateIo(
            "OMP maintenance state escaped the current user's LocalAppData".into(),
        ));
    }
    Ok(state_root)
}

#[cfg(windows)]
fn windows_local_app_data() -> Result<PathBuf, OmpMaintenanceError> {
    // This is the shell's current-user Known Folder, not an environment override.
    use std::os::windows::ffi::OsStringExt as _;
    use windows_sys::Win32::UI::Shell::{SHGetFolderPathW, CSIDL_LOCAL_APPDATA};

    const MAX_PATH: usize = 260;
    let mut buffer = [0u16; MAX_PATH];
    let result = unsafe {
        SHGetFolderPathW(
            std::ptr::null_mut(),
            CSIDL_LOCAL_APPDATA as i32,
            std::ptr::null_mut(),
            0,
            buffer.as_mut_ptr(),
        )
    };
    if result < 0 {
        return Err(OmpMaintenanceError::StateIo(format!(
            "failed to resolve the current user's LocalAppData ({result:#x})"
        )));
    }
    let Some(length) = buffer.iter().position(|character| *character == 0) else {
        return Err(OmpMaintenanceError::StateIo(
            "the current user's LocalAppData path is too long".into(),
        ));
    };
    let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(OmpMaintenanceError::StateIo(
            "the current user's LocalAppData path is not absolute".into(),
        ))
    }
}

fn validate_pinned_state_root(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(private_path_error(
            "OMP maintenance state root must be absolute",
        ));
    }
    let canonical = pin_trusted_state_root(path)?;
    if canonical != path {
        return Err(private_path_error(
            "OMP maintenance state root changed while it was pinned",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn pin_trusted_state_root(path: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;

    let directory = open_private_directory(path)?;
    validate_opened_private_directory(&directory, path)?;
    let opened = directory.metadata()?;
    let canonical = fs::canonicalize(path)?;
    let canonical_metadata = fs::symlink_metadata(&canonical)?;
    if canonical_metadata.file_type().is_symlink()
        || canonical_metadata.dev() != opened.dev()
        || canonical_metadata.ino() != opened.ino()
    {
        return Err(private_path_error(
            "OMP maintenance state root changed while it was pinned",
        ));
    }
    validate_trusted_state_root_ancestry(&canonical)?;
    Ok(canonical)
}

#[cfg(unix)]
fn validate_trusted_state_root_ancestry(root: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let mut current = PathBuf::new();
    for component in root.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(private_path_error(
                "OMP maintenance state root has a non-directory ancestor",
            ));
        }
        let mode = metadata.permissions().mode();
        if current == root {
            if metadata.uid() != effective_uid() || mode & 0o077 != 0 {
                return Err(private_path_error(
                    "OMP maintenance state root is not private to the current user",
                ));
            }
        } else {
            if metadata.uid() != 0 && metadata.uid() != effective_uid() {
                return Err(private_path_error(
                    "OMP maintenance state root has an untrusted ancestor owner",
                ));
            }
            if mode & 0o022 != 0 && mode & 0o1000 == 0 {
                return Err(private_path_error(
                    "OMP maintenance state root has an unsafe writable ancestor",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn pin_trusted_state_root(path: &Path) -> io::Result<PathBuf> {
    validate_windows_directory_ancestry(path)?;
    validate_acl_free(path)?;
    let canonical = fs::canonicalize(path)?;
    validate_windows_directory_ancestry(&canonical)?;
    validate_acl_free(&canonical)?;
    Ok(canonical)
}

#[cfg(windows)]
fn validate_windows_directory_ancestry(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    if !path.is_absolute() {
        return Err(private_path_error(
            "OMP maintenance state root must be absolute",
        ));
    }
    for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        let metadata = fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(private_path_error(
                "OMP maintenance state root contains a reparse point or non-directory",
            ));
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn pin_trusted_state_root(path: &Path) -> io::Result<PathBuf> {
    Err(private_path_error(&format!(
        "OMP maintenance has no trusted state-root implementation for {}",
        path.display()
    )))
}
#[cfg(unix)]
fn validate_existing_state_root_ancestry(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let mut current = PathBuf::new();
    let mut missing = false;
    for component in path.components() {
        current.push(component.as_os_str());
        if missing {
            continue;
        }
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing = true;
                continue;
            }
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() {
            if current == path || !is_trusted_system_state_root_alias(&current, &metadata) {
                return Err(private_path_error(
                    "OMP maintenance state root has an untrusted symlink ancestor",
                ));
            }
            continue;
        }
        if !metadata.is_dir() {
            return Err(private_path_error(
                "OMP maintenance state root has a non-directory ancestor",
            ));
        }
        if current == path {
            continue;
        }
        if metadata.uid() != 0 && metadata.uid() != effective_uid() {
            return Err(private_path_error(
                "OMP maintenance state root has an untrusted ancestor owner",
            ));
        }
        if metadata.permissions().mode() & 0o022 != 0 && metadata.permissions().mode() & 0o1000 == 0
        {
            return Err(private_path_error(
                "OMP maintenance state root has an unsafe writable ancestor",
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_trusted_system_state_root_alias(path: &Path, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    // Accept only root-owned aliases directly beneath / (for example macOS /tmp).
    path.parent() == Some(Path::new("/"))
        && metadata.uid() == 0
        && fs::symlink_metadata("/").is_ok_and(|root| {
            root.is_dir() && root.uid() == 0 && root.permissions().mode() & 0o022 == 0
        })
}

#[cfg(windows)]
fn validate_existing_state_root_ancestry(path: &Path) -> io::Result<()> {
    let mut existing = path;
    while let Err(error) = fs::symlink_metadata(existing) {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
        existing = existing.parent().ok_or_else(|| {
            private_path_error("OMP maintenance state root has no existing ancestor")
        })?;
    }
    validate_windows_directory_ancestry(existing)
}

#[cfg(not(any(unix, windows)))]
fn validate_existing_state_root_ancestry(_: &Path) -> io::Result<()> {
    Ok(())
}

fn prepare_trusted_state_root(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(private_path_error(
            "OMP maintenance state root must be absolute",
        ));
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(private_path_error(
            "OMP maintenance state root must not contain relative components",
        ));
    }
    validate_existing_state_root_ancestry(path)?;
    ensure_private_directory(path)?;
    pin_trusted_state_root(path)
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;

    validate_private_directory(path)
}

#[cfg(unix)]
fn open_private_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(unix)]
fn prepare_opened_private_directory(directory: &File, path: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::MetadataExt as _;

    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() || metadata.mode() & 0o022 != 0 {
        return Err(private_path_error(
            "OMP maintenance directory is not a private current-user directory",
        ));
    }
    if metadata.mode() & 0o077 != 0 && unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    validate_opened_private_directory(directory, path)
}

#[cfg(unix)]
fn validate_opened_private_directory(directory: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != effective_uid() || metadata.mode() & 0o077 != 0 {
        return Err(private_path_error(
            "OMP maintenance directory is not a private current-user directory",
        ));
    }
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink()
        || path_metadata.dev() != metadata.dev()
        || path_metadata.ino() != metadata.ino()
    {
        return Err(private_path_error(
            "OMP maintenance directory changed while it was validated",
        ));
    }
    validate_acl_free(path)
}

fn validate_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        let directory = open_private_directory(path)?;
        prepare_opened_private_directory(&directory, path)
    }
    #[cfg(windows)]
    {
        validate_windows_directory_ancestry(path)?;
        validate_acl_free(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        if fs::symlink_metadata(path)?.file_type().is_dir() {
            Ok(())
        } else {
            Err(private_path_error(
                "OMP maintenance path is not a directory",
            ))
        }
    }
}

fn validate_private_entry_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            private_path_error("OMP maintenance entry is not a regular file"),
        ),
        #[cfg(unix)]
        Ok(metadata) => {
            use std::os::unix::fs::MetadataExt as _;

            if metadata.uid() != effective_uid()
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
            {
                Err(private_path_error(
                    "OMP maintenance entry is not a private current-user file",
                ))
            } else {
                validate_acl_free(path)
            }
        }
        #[cfg(windows)]
        Ok(metadata) => {
            use std::os::windows::fs::MetadataExt as _;

            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                Err(private_path_error(
                    "OMP maintenance entry is a reparse point",
                ))
            } else {
                validate_acl_free(path)
            }
        }
        #[cfg(not(any(unix, windows)))]
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "linux")]
fn path_xattr_names(path: &Path) -> io::Result<Vec<u8>> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| private_path_error("OMP maintenance path contains a NUL byte"))?;
    let size = unsafe { libc::llistxattr(path.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut names = vec![0u8; size as usize];
    let read = unsafe { libc::llistxattr(path.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    names.truncate(read as usize);
    Ok(names)
}

#[cfg(target_os = "linux")]
fn validate_acl_free(path: &Path) -> io::Result<()> {
    let names = path_xattr_names(path)?;
    if names.split(|byte| *byte == 0).any(|name| {
        name.windows(3)
            .any(|window| window.eq_ignore_ascii_case(b"acl"))
    }) {
        Err(private_path_error(
            "OMP maintenance path has an access control list",
        ))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn validate_acl_free(path: &Path) -> io::Result<()> {
    type Acl = *mut libc::c_void;
    extern "C" {
        fn acl_get_link_np(path: *const libc::c_char, acl_type: libc::c_int) -> Acl;
        fn acl_get_entry(acl: Acl, entry_id: libc::c_int, entry: *mut Acl) -> libc::c_int;
        fn acl_free(object: *mut libc::c_void) -> libc::c_int;
    }

    const ACL_TYPE_EXTENDED: libc::c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: libc::c_int = 0;
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| private_path_error("OMP maintenance path contains a NUL byte"))?;
    let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let mut entry = std::ptr::null_mut();
    let result = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) };
    let free_result = unsafe { acl_free(acl) };
    if free_result != 0 {
        return Err(io::Error::last_os_error());
    }
    if result == 0 {
        Err(private_path_error(
            "OMP maintenance path has an access control list",
        ))
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn validate_acl_free(_: &Path) -> io::Result<()> {
    Err(private_path_error(
        "OMP maintenance ACL validation is unsupported on this platform",
    ))
}

#[cfg(windows)]
fn validate_acl_free(_: &Path) -> io::Result<()> {
    Err(private_path_error(
        "OMP maintenance owner-only DACL validation is unavailable",
    ))
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn private_path_error(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn open_private(path: &Path, create: bool, write: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .create(create)
        .truncate(false);
    configure_private_open(&mut options);
    let file = options.open(path)?;
    validate_private_file(&file, path)?;
    Ok(file)
}

fn open_private_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    configure_private_open(&mut options);
    let file = options.open(path)?;
    validate_private_file(&file, path)?;
    Ok(file)
}

fn configure_private_open(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;

        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

#[cfg(unix)]
fn validate_private_file(file: &File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != effective_uid()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "OMP maintenance state is not a private current-user file",
        ));
    }
    validate_acl_free(path)
}

#[cfg(windows)]
fn validate_private_file(file: &File, path: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt as _;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let metadata = file.metadata()?;
    if metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        validate_acl_free(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "OMP maintenance state is not a regular non-reparse file",
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn validate_private_file(file: &File, _: &Path) -> io::Result<()> {
    if file.metadata()?.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "OMP maintenance state is not a regular file",
        ))
    }
}

#[cfg(not(windows))]
fn replace_state_file(temporary: &Path, path: &Path) -> io::Result<()> {
    fs::rename(temporary, path)
}

#[cfg(windows)]
fn replace_state_file(temporary: &Path, path: &Path) -> io::Result<()> {
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
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(directory: &Path) -> io::Result<()> {
    match File::open(directory).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::Unsupported | io::ErrorKind::InvalidInput
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn sync_parent_directory(_: &Path) -> io::Result<()> {
    Ok(())
}

fn state_io(path: &Path, action: &str, error: io::Error) -> OmpMaintenanceError {
    OmpMaintenanceError::StateIo(format!("failed to {action} {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(pane_id: &str) -> OmpRouteKey {
        OmpRouteKey {
            pane_id: pane_id.into(),
            omp_session_id: "omp-session".into(),
            route_generation: 1,
        }
    }

    fn operation_id(seed: u8) -> String {
        let bytes =
            std::array::from_fn::<_, OPERATION_ID_BYTES, _>(|index| seed.wrapping_add(index as u8));
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn test_dir(name: &str) -> PathBuf {
        fs::canonicalize(std::env::temp_dir())
            .expect("canonical test temporary directory")
            .join(format!(
                "herdr-omp-maintenance-{name}-{}-{}",
                std::process::id(),
                operation_id(name.len() as u8)
            ))
    }

    #[cfg(unix)]
    #[test]
    fn ambient_environment_cannot_partition_the_account_maintenance_root() {
        const CHILD_ENV: &str = "HERDR_TEST_OMP_MAINTENANCE_ACCOUNT_ROOT_CHILD";
        const TEST_NAME: &str = "server::omp_maintenance::tests::ambient_environment_cannot_partition_the_account_maintenance_root";

        if let Some(mode) = std::env::var_os(CHILD_ENV) {
            std::env::remove_var("XDG_STATE_HOME");
            std::env::remove_var("HOME");
            match mode.to_string_lossy().as_ref() {
                "xdg-relative" => std::env::set_var("XDG_STATE_HOME", "relative-state"),
                "xdg-absolute" => std::env::set_var(
                    "XDG_STATE_HOME",
                    std::env::current_dir().unwrap().join("absolute-state"),
                ),
                "xdg-empty" => std::env::set_var("XDG_STATE_HOME", ""),
                "home-relative" => std::env::set_var("HOME", "relative-home"),
                "home-absolute" => std::env::set_var(
                    "HOME",
                    std::env::current_dir().unwrap().join("absolute-home"),
                ),
                "home-empty" => std::env::set_var("HOME", ""),
                unexpected => panic!("unexpected child mode {unexpected}"),
            }
            let expected = unix_account_home()
                .unwrap()
                .join(".local")
                .join("state")
                .join(maintenance_app_dir_name());
            assert_eq!(unix_host_state_root().unwrap(), expected);
            return;
        }

        let base = test_dir("ambient-environment-roots");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let executable = std::env::current_exe().expect("test executable");
        for cwd_name in ["first-cwd", "second-cwd"] {
            let cwd = base.join(cwd_name);
            fs::create_dir(&cwd).unwrap();
            for mode in [
                "xdg-relative",
                "xdg-absolute",
                "xdg-empty",
                "home-relative",
                "home-absolute",
                "home-empty",
            ] {
                let status = std::process::Command::new(&executable)
                    .arg("--exact")
                    .arg(TEST_NAME)
                    .arg("--nocapture")
                    .env(CHILD_ENV, mode)
                    .env_remove("XDG_STATE_HOME")
                    .env_remove("HOME")
                    .current_dir(&cwd)
                    .status()
                    .expect("run account-root child test");
                assert!(
                    status.success(),
                    "account-root child failed for {mode} in {cwd_name}"
                );
            }
        }
        let _ = fs::remove_dir_all(base);
    }

    #[cfg(windows)]
    #[test]
    fn windows_maintenance_requires_an_absolute_known_folder_root() {
        assert!(windows_local_app_data()
            .expect("resolve LocalAppData")
            .is_absolute());
        assert!(
            prepare_trusted_state_root(std::path::Path::new("relative-maintenance-root")).is_err()
        );
    }

    #[cfg(unix)]
    fn make_private(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[cfg(not(unix))]
    fn make_private(_: &Path) {}

    #[cfg(unix)]
    fn assert_backend_creation_rejected(state_path: PathBuf) {
        assert!(matches!(
            OmpMaintenance::file_for_test("default", state_path),
            Err(OmpMaintenanceError::StateIo(_))
        ));
    }

    #[cfg(unix)]
    fn assert_state_access_rejected(state_path: PathBuf) {
        assert_backend_creation_rejected(state_path);
    }
    #[test]
    fn file_state_survives_reopening_with_armed_permit() {
        let dir = test_dir("reopen");
        let _ = fs::remove_dir_all(&dir);
        let state_path = dir.join("state.json");
        let owner = operation_id(1);
        let first = OmpMaintenance::file_for_test("default", state_path.clone()).unwrap();
        first.acquire(&owner).unwrap();
        first.grant_permit(&owner, "proof", "w1:p1").unwrap();
        drop(first);

        let reopened = OmpMaintenance::file_for_test("proof", state_path).unwrap();
        assert_eq!(
            reopened.handoff_state().unwrap(),
            Some(OmpMaintenanceHandoffState {
                owner_hash: operation_owner_hash(&owner),
                permit: Some(ServerOmpMaintenancePermit {
                    session: "proof".into(),
                    pane_id: "w1:p1".into(),
                }),
            })
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn malformed_state_denies_admission_without_calling_route_registry() {
        let dir = test_dir("malformed");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("state.json");
        fs::write(&state_path, b"not-json\n").unwrap();
        make_private(&state_path);
        let maintenance = OmpMaintenance::file_for_test("default", state_path).unwrap();
        let mut called = false;

        let result = maintenance.admit(&route("w1:p1"), || {
            called = true;
            Ok::<_, ()>(())
        });

        assert!(matches!(
            result,
            Err(OmpMaintenanceAdmissionError::State(
                OmpMaintenanceError::StateInvalid(_)
            ))
        ));
        assert!(!called);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn crash_stale_route_is_reconciled_after_its_os_lock_releases() {
        let dir = test_dir("crash-reopen");
        let _ = fs::remove_dir_all(&dir);
        let state_path = dir.join("state.json");
        let crashed = OmpMaintenance::file_for_test("crashed", state_path.clone()).unwrap();
        crashed.admit(&route("w1:p1"), || Ok::<_, ()>(())).unwrap();
        assert_eq!(crashed.status().unwrap().route_count, 1);
        drop(crashed);

        let reopened = OmpMaintenance::file_for_test("controller", state_path).unwrap();
        assert_eq!(reopened.status().unwrap().route_count, 0);
        assert!(reopened.acquire(&operation_id(2)).unwrap().held);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn inspect_is_passive_and_does_not_reconcile_or_delete_routes() {
        let dir = test_dir("passive-inspect");
        let _ = fs::remove_dir_all(&dir);
        let state_path = dir.join("state.json");
        let live = OmpMaintenance::file_for_test("live", state_path.clone()).unwrap();
        live.admit(&route("w1:p1"), || Ok::<_, ()>(())).unwrap();
        drop(live);

        let inspector = OmpMaintenance::file_for_test("inspector", state_path.clone()).unwrap();
        let before = fs::read(&state_path).unwrap();
        assert_eq!(inspector.inspect().unwrap().route_count, 1);
        assert_eq!(fs::read(&state_path).unwrap(), before);
        assert_eq!(inspector.inspect().unwrap().route_count, 1);
        assert_eq!(fs::read(&state_path).unwrap(), before);
        drop(inspector);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn passive_inspect_accepts_a_canonical_system_root_alias() {
        let dir = Path::new("/tmp").join(format!(
            "herdr-omp-maintenance-alias-{}-{}",
            std::process::id(),
            operation_id(19)
        ));
        let _ = fs::remove_dir_all(&dir);
        let state_path = dir.join("state.json");
        let lock_path = state_path.with_extension("lock");
        let maintenance = OmpMaintenance::file_for_test("default", state_path.clone()).unwrap();
        maintenance.acquire(&operation_id(20)).unwrap();

        let inspected = inspect_file_state(&dir, &state_path, &lock_path).unwrap();
        assert!(inspected.held);

        drop(maintenance);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn reconciliation_keeps_routes_owned_by_still_live_sessions() {
        let dir = test_dir("live-sessions");
        let _ = fs::remove_dir_all(&dir);
        let state_path = dir.join("state.json");
        let first = OmpMaintenance::file_for_test("first", state_path.clone()).unwrap();
        let second = OmpMaintenance::file_for_test("second", state_path.clone()).unwrap();
        let controller = OmpMaintenance::file_for_test("controller", state_path).unwrap();
        first.admit(&route("w1:p1"), || Ok::<_, ()>(())).unwrap();
        second.admit(&route("w2:p1"), || Ok::<_, ()>(())).unwrap();

        let status = controller.status().unwrap();
        assert_eq!(status.route_count, 2);
        assert_eq!(
            status
                .routes
                .iter()
                .map(|route| route.session.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );

        drop(first);
        let status = controller.status().unwrap();
        assert_eq!(status.route_count, 1);
        assert_eq!(status.routes[0].session, "second");
        drop(second);
        assert_eq!(controller.status().unwrap().route_count, 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn interrupted_temp_write_leaves_the_previous_state_intact() {
        let dir = test_dir("interrupted-write");
        let _ = fs::remove_dir_all(&dir);
        let state_path = dir.join("state.json");
        let owner = operation_id(3);
        let maintenance = OmpMaintenance::file_for_test("default", state_path.clone()).unwrap();
        maintenance.acquire(&owner).unwrap();
        let before = fs::read(&state_path).unwrap();

        let error = save_state_with_hook(&state_path, &PersistedState::default(), |temporary| {
            fs::write(temporary, b"{")?;
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "simulated interrupted replacement",
            ))
        })
        .unwrap_err();

        assert!(matches!(error, OmpMaintenanceError::StateIo(_)));
        assert_eq!(fs::read(&state_path).unwrap(), before);
        drop(maintenance);
        let reopened = OmpMaintenance::file_for_test("default", state_path).unwrap();
        assert!(reopened.status().unwrap().held);
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn acl_grants_on_maintenance_roots_and_files_are_rejected() {
        use std::os::unix::fs::PermissionsExt as _;

        fn add_write_acl(path: &Path) {
            let status = std::process::Command::new("/bin/chmod")
                .arg("+a")
                .arg("everyone allow write")
                .arg(path)
                .status()
                .expect("add test ACL");
            assert!(status.success());
        }

        let base = test_dir("acl-paths");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        let root = base.join("root-acl");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        add_write_acl(&root);
        assert_backend_creation_rejected(root.join("state.json"));

        let file_root = base.join("file-acl");
        let state_path = file_root.join("state.json");
        let maintenance = OmpMaintenance::file_for_test("default", state_path.clone()).unwrap();
        maintenance.acquire(&operation_id(21)).unwrap();
        drop(maintenance);
        add_write_acl(&state_path);
        assert_state_access_rejected(state_path.clone());

        for path in [&root, &state_path] {
            let _ = std::process::Command::new("/bin/chmod")
                .arg("-N")
                .arg(path)
                .status();
        }
        let _ = fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_state_root_and_instance_directories_are_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let base = test_dir("unsafe-directories");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();

        let real_root = base.join("real-root");
        fs::create_dir(&real_root).unwrap();
        fs::set_permissions(&real_root, fs::Permissions::from_mode(0o700)).unwrap();
        let linked_root = base.join("linked-root");
        symlink(&real_root, &linked_root).unwrap();
        assert_backend_creation_rejected(linked_root.join("state.json"));

        for (name, mode) in [("group-writable", 0o720), ("other-writable", 0o702)] {
            let root = base.join(name);
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
            assert_backend_creation_rejected(root.join("state.json"));
        }

        let not_directory = base.join("not-directory");
        fs::write(&not_directory, b"not a directory").unwrap();
        assert_backend_creation_rejected(not_directory.join("state.json"));

        let linked_instance_root = base.join("linked-instance-root");
        fs::create_dir(&linked_instance_root).unwrap();
        fs::set_permissions(&linked_instance_root, fs::Permissions::from_mode(0o700)).unwrap();
        let real_instances = base.join("real-instances");
        fs::create_dir(&real_instances).unwrap();
        fs::set_permissions(&real_instances, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(
            &real_instances,
            linked_instance_root.join(INSTANCE_DIRECTORY),
        )
        .unwrap();
        assert_backend_creation_rejected(linked_instance_root.join("state.json"));

        let writable_instance_root = base.join("writable-instance-root");
        fs::create_dir(&writable_instance_root).unwrap();
        fs::set_permissions(&writable_instance_root, fs::Permissions::from_mode(0o700)).unwrap();
        let writable_instances = writable_instance_root.join(INSTANCE_DIRECTORY);
        fs::create_dir(&writable_instances).unwrap();
        fs::set_permissions(&writable_instances, fs::Permissions::from_mode(0o770)).unwrap();
        assert_backend_creation_rejected(writable_instance_root.join("state.json"));

        let _ = fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn readable_current_user_state_directories_are_tightened_to_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let base = test_dir("tighten-directories");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();

        for (name, mode) in [("group-readable", 0o750), ("world-readable", 0o755)] {
            let root = base.join(name);
            fs::create_dir(&root).unwrap();
            fs::set_permissions(&root, fs::Permissions::from_mode(mode)).unwrap();
            let maintenance = OmpMaintenance::file_for_test("default", root.join("state.json"))
                .expect("tighten current-user state root");
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            drop(maintenance);
        }

        let root = base.join("instance-directory");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let instances = root.join(INSTANCE_DIRECTORY);
        fs::create_dir(&instances).unwrap();
        fs::set_permissions(&instances, fs::Permissions::from_mode(0o750)).unwrap();
        let maintenance = OmpMaintenance::file_for_test("default", root.join("state.json"))
            .expect("tighten current-user instance directory");
        assert_eq!(
            fs::metadata(&instances).unwrap().permissions().mode() & 0o777,
            0o700
        );
        drop(maintenance);

        let _ = fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn intermediate_state_root_symlink_retargeting_is_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let base = test_dir("retargeted-state-root");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
        let first = base.join("first-root");
        let second = base.join("second-root");
        for root in [&first, &second] {
            fs::create_dir(root).unwrap();
            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let alias = base.join("state-alias");
        for target in [&first, &second] {
            symlink(target, &alias).unwrap();
            assert_backend_creation_rejected(alias.join("maintenance").join("state.json"));
            fs::remove_file(&alias).unwrap();
        }

        let _ = fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_state_and_lock_entries_are_rejected() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let base = test_dir("unsafe-entries");
        let _ = fs::remove_dir_all(&base);

        let state_link_root = base.join("state-link");
        fs::create_dir_all(&state_link_root).unwrap();
        fs::set_permissions(&state_link_root, fs::Permissions::from_mode(0o700)).unwrap();
        let state_target = state_link_root.join("target.json");
        fs::write(
            &state_target,
            serde_json::to_vec(&PersistedState::default()).unwrap(),
        )
        .unwrap();
        make_private(&state_target);
        let linked_state = state_link_root.join("state.json");
        symlink(&state_target, &linked_state).unwrap();
        assert_state_access_rejected(linked_state);

        let lock_link_root = base.join("lock-link");
        fs::create_dir_all(&lock_link_root).unwrap();
        fs::set_permissions(&lock_link_root, fs::Permissions::from_mode(0o700)).unwrap();
        let lock_target = lock_link_root.join("target.lock");
        fs::write(&lock_target, b"").unwrap();
        make_private(&lock_target);
        let lock_state = lock_link_root.join("state.json");
        symlink(&lock_target, lock_state.with_extension("lock")).unwrap();
        assert_state_access_rejected(lock_state);

        let hard_link_root = base.join("hard-link");
        fs::create_dir_all(&hard_link_root).unwrap();
        fs::set_permissions(&hard_link_root, fs::Permissions::from_mode(0o700)).unwrap();
        let hard_link_target = hard_link_root.join("target.json");
        fs::write(
            &hard_link_target,
            serde_json::to_vec(&PersistedState::default()).unwrap(),
        )
        .unwrap();
        make_private(&hard_link_target);
        let hard_link_state = hard_link_root.join("state.json");
        fs::hard_link(&hard_link_target, &hard_link_state).unwrap();
        assert_state_access_rejected(hard_link_state);

        let state_mode_root = base.join("state-mode");
        fs::create_dir_all(&state_mode_root).unwrap();
        fs::set_permissions(&state_mode_root, fs::Permissions::from_mode(0o700)).unwrap();
        let state_mode = state_mode_root.join("state.json");
        fs::write(
            &state_mode,
            serde_json::to_vec(&PersistedState::default()).unwrap(),
        )
        .unwrap();
        fs::set_permissions(&state_mode, fs::Permissions::from_mode(0o660)).unwrap();
        assert_state_access_rejected(state_mode);

        let lock_mode_root = base.join("lock-mode");
        fs::create_dir_all(&lock_mode_root).unwrap();
        fs::set_permissions(&lock_mode_root, fs::Permissions::from_mode(0o700)).unwrap();
        let lock_mode_state = lock_mode_root.join("state.json");
        fs::write(lock_mode_state.with_extension("lock"), b"").unwrap();
        fs::set_permissions(
            lock_mode_state.with_extension("lock"),
            fs::Permissions::from_mode(0o602),
        )
        .unwrap();
        assert_state_access_rejected(lock_mode_state);

        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn ownership_capability_is_private_idempotent_and_replay_safe() {
        let dir = test_dir("ownership");
        let _ = fs::remove_dir_all(&dir);
        let state_path = dir.join("state.json");
        let owner = operation_id(4);
        let wrong = operation_id(5);
        let next = operation_id(6);
        let maintenance = OmpMaintenance::file_for_test("default", state_path.clone()).unwrap();

        let first = maintenance.acquire(&owner).unwrap();
        assert_eq!(maintenance.acquire(&owner).unwrap(), first);
        let public = serde_json::to_string(&first).unwrap();
        assert!(!public.contains("operation_id"));
        assert!(!public.contains(&owner));
        assert!(!fs::read_to_string(&state_path).unwrap().contains(&owner));

        for error in [
            maintenance.acquire(&wrong).unwrap_err(),
            maintenance
                .grant_permit(&wrong, "default", "w1:p1")
                .unwrap_err(),
            maintenance.release(&wrong).unwrap_err(),
        ] {
            assert!(matches!(
                error,
                OmpMaintenanceError::Conflict(_) | OmpMaintenanceError::NotOwner(_)
            ));
            assert!(!error.message().contains(&owner));
        }

        let released = maintenance.release(&owner).unwrap();
        assert!(!released.held);
        assert_eq!(maintenance.release(&owner).unwrap(), released);
        assert_eq!(maintenance.acquire(&owner).unwrap(), released);
        assert!(matches!(
            maintenance.release(&wrong),
            Err(OmpMaintenanceError::NotOwner(_))
        ));
        assert!(maintenance.acquire(&next).unwrap().held);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn lost_response_acquire_retry_replays_after_later_lease_operation() {
        let store = TestOmpMaintenanceStore::new();
        let maintenance = OmpMaintenance::for_test("default", store);
        let original = operation_id(8);
        let later = operation_id(9);

        maintenance.acquire(&original).unwrap();
        maintenance.release(&original).unwrap();
        let current = maintenance.acquire(&later).unwrap();

        assert_eq!(maintenance.acquire(&original).unwrap(), current);
    }

    #[test]
    fn lost_response_release_retry_replays_after_later_lease_operation() {
        let store = TestOmpMaintenanceStore::new();
        let maintenance = OmpMaintenance::for_test("default", store);
        let original = operation_id(10);
        let later = operation_id(11);

        maintenance.acquire(&original).unwrap();
        maintenance.release(&original).unwrap();
        let current = maintenance.acquire(&later).unwrap();

        assert_eq!(maintenance.release(&original).unwrap(), current);
    }

    #[test]
    fn completed_operations_are_bounded_to_recent_retries() {
        let store = TestOmpMaintenanceStore::new();
        let maintenance = OmpMaintenance::for_test("default", store.clone());

        for seed in 0..=MAX_COMPLETED_OPERATIONS as u8 {
            let operation = operation_id(seed);
            maintenance.acquire(&operation).unwrap();
            maintenance.release(&operation).unwrap();
        }

        {
            let state = store.0.lock().unwrap();
            assert_eq!(
                state.state.completed_operations.len(),
                MAX_COMPLETED_OPERATIONS
            );
            assert!(!state
                .state
                .completed_operations
                .contains(&operation_owner_hash(&operation_id(0))));
            assert!(state
                .state
                .completed_operations
                .contains(&operation_owner_hash(&operation_id(
                    MAX_COMPLETED_OPERATIONS as u8
                ))));
        }

        assert!(
            !maintenance
                .acquire(&operation_id(MAX_COMPLETED_OPERATIONS as u8))
                .unwrap()
                .held
        );
    }

    #[cfg(unix)]
    #[test]
    fn oversized_state_is_rejected_before_parsing() {
        let dir = test_dir("oversized-state");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let state_path = dir.join("state.json");
        fs::write(&state_path, vec![b'{'; MAX_STATE_FILE_BYTES + 1]).unwrap();
        make_private(&state_path);

        let error = load_state(&state_path).unwrap_err();
        assert!(matches!(
            error,
            OmpMaintenanceError::StateInvalid(message) if message.contains("exceeds")
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn handoff_validation_rejects_a_route_added_after_state_capture() {
        let store = TestOmpMaintenanceStore::new();
        let source = OmpMaintenance::for_test("default", store.clone());
        let target = OmpMaintenance::for_test("default", store);
        let owner = operation_id(24);

        source.acquire(&owner).unwrap();
        let expected = source.handoff_state().unwrap();
        source.grant_permit(&owner, "default", "w1:p1").unwrap();
        source.admit(&route("w1:p1"), || Ok::<_, ()>(())).unwrap();

        assert!(matches!(
            target.validate_handoff_state(expected.as_ref()),
            Err(OmpMaintenanceError::RoutesLive(1))
        ));
    }

    #[test]
    fn unleased_handoff_ignores_unrelated_routes() {
        let store = TestOmpMaintenanceStore::new();
        let source = OmpMaintenance::for_test("source", store.clone());
        let target = OmpMaintenance::for_test("target", store);

        source
            .admit(&route("other:pane"), || Ok::<_, ()>(()))
            .unwrap();

        assert_eq!(source.handoff_state().unwrap(), None);
        target.validate_handoff_state(None).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn inspect_rejects_existing_nonprivate_roots_without_normalizing_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        for (name, mode) in [
            ("inspect-no-chmod-750", 0o750),
            ("inspect-no-chmod-755", 0o755),
        ] {
            let dir = test_dir(name);
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();

            let result = inspect_file_state(&dir, &dir.join("state.json"), &dir.join("state.lock"));

            assert!(matches!(result, Err(OmpMaintenanceError::StateIo(_))));
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                mode
            );
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn operation_capability_format_is_exact_and_canonical() {
        assert!(validate_operation_id(&operation_id(7)).is_ok());
        for invalid in [
            "operation-1".to_string(),
            "A".repeat(42),
            "A".repeat(44),
            format!("{}=", "A".repeat(43)),
            format!("{}B", "A".repeat(42)),
            format!("{}!", "A".repeat(42)),
        ] {
            assert!(matches!(
                validate_operation_id(&invalid),
                Err(OmpMaintenanceError::InvalidRequest(_))
            ));
        }
    }
}
