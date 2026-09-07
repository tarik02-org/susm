use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use susm_protocol::{
    host::{
        GetControllerStatusRequest, GetControllerStatusResponse, HostStatus, RegisterUserRequest,
        RegisterUserResponse, RestartControllerRequest, RestartControllerResponse,
        UnregisterUserRequest, UnregisterUserResponse,
        host_control_service_server::HostControlService,
    },
    pipe::{CallerIdentity, UserSid},
    session::EndingEvent,
};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{
    launcher::{ControllerRunner, ProcessIdentity, ProcessObserver},
    registration::{self, RuntimeSession},
};

pub struct HostRpc {
    controllers: Mutex<BTreeMap<String, Registration>>,
    ending_windows_sessions: Mutex<BTreeSet<(u32, u64)>>,
}

struct Registration {
    manager_session_id: String,
    windows_session_id: Option<u32>,
    authentication_id: Option<u64>,
    ending_event: Option<EndingEvent>,
    runner: Option<ControllerRunner>,
    recorded_process: Option<ProcessIdentity>,
    session_conflict: bool,
}

impl HostRpc {
    pub fn new() -> windows::core::Result<Self> {
        let mut runtime = registration::runtime_sessions()?;
        let controllers = registration::list()?
            .into_iter()
            .map(|sid| {
                let saved = runtime.remove(&sid);
                (
                    sid,
                    Registration {
                        manager_session_id: saved
                            .as_ref()
                            .map_or_else(String::new, |session| session.manager_session_id.clone()),
                        windows_session_id: saved
                            .as_ref()
                            .map(|session| session.windows_session_id),
                        authentication_id: saved.as_ref().map(|session| session.authentication_id),
                        ending_event: None,
                        runner: None,
                        recorded_process: saved.and_then(|session| {
                            (session.controller_process_id != 0
                                && session.controller_creation_time != 0)
                                .then_some(ProcessIdentity {
                                    process_id: session.controller_process_id,
                                    creation_time: session.controller_creation_time,
                                })
                        }),
                        session_conflict: false,
                    },
                )
            })
            .collect();
        for sid in runtime.keys() {
            let _ = registration::remove_runtime(sid);
        }
        let host = Self {
            controllers: Mutex::new(controllers),
            ending_windows_sessions: Mutex::new(BTreeSet::new()),
        };
        host.reconcile();
        Ok(host)
    }

    pub fn reconcile(&self) {
        let Ok(inventory) = registration::active_sessions() else {
            return;
        };
        let mut sessions = inventory.sessions;
        let conflicts = inventory.conflicts;
        {
            let mut ending = self
                .ending_windows_sessions
                .lock()
                .expect("host ending-session lock poisoned");
            ending.retain(|logon| inventory.logons.contains(logon));
            sessions.retain(|_, session| {
                !ending.contains(&(session.session_id, session.authentication_id))
            });
        }
        let Ok(registered) = registration::list() else {
            return;
        };
        let mut controllers = self
            .controllers
            .lock()
            .expect("host controller lock poisoned");
        controllers.retain(|sid, _| registered.contains(sid));
        for sid in registered {
            controllers.entry(sid).or_insert_with(|| Registration {
                manager_session_id: String::new(),
                windows_session_id: None,
                authentication_id: None,
                ending_event: None,
                runner: None,
                recorded_process: None,
                session_conflict: false,
            });
        }
        for (sid, registration) in &mut *controllers {
            registration.session_conflict = conflicts.contains(sid);
            if registration.session_conflict {
                persist_runtime(sid, registration);
                continue;
            }
            match (registration.runner.is_some(), sessions.remove(sid)) {
                (false, Some(mut session)) => {
                    let Ok(user_sid) = UserSid::parse_windows(sid.clone()) else {
                        continue;
                    };
                    let same_logon =
                        registration.authentication_id == Some(session.authentication_id);
                    if !same_logon {
                        signal_previous_session(registration, &user_sid);
                        registration.manager_session_id.clear();
                        registration.windows_session_id = None;
                        registration.recorded_process = None;
                        let _ = registration::remove_runtime(sid);
                    }
                    let manager_session_id = if registration.manager_session_id.is_empty() {
                        Uuid::now_v7().to_string()
                    } else {
                        registration.manager_session_id.clone()
                    };
                    let Ok(ending_event) = EndingEvent::create(&manager_session_id, &user_sid)
                    else {
                        continue;
                    };
                    let runner = if let Some(process) = registration.recorded_process.take() {
                        ControllerRunner::adopt_from_session_token(
                            session.take_token(),
                            session.session_id,
                            manager_session_id.clone(),
                            process,
                            runtime_observer(
                                sid.clone(),
                                manager_session_id.clone(),
                                session.session_id,
                                session.authentication_id,
                            ),
                        )
                    } else {
                        ControllerRunner::from_session_token(
                            session.take_token(),
                            session.session_id,
                            manager_session_id.clone(),
                            runtime_observer(
                                sid.clone(),
                                manager_session_id.clone(),
                                session.session_id,
                                session.authentication_id,
                            ),
                        )
                    };
                    let Ok(runner) = runner else {
                        continue;
                    };
                    registration.manager_session_id = manager_session_id;
                    registration.windows_session_id = Some(session.session_id);
                    registration.authentication_id = Some(session.authentication_id);
                    registration.ending_event = Some(ending_event);
                    registration.runner = Some(runner);
                    persist_runtime(sid, registration);
                    tracing::info!(
                        name = "manager_session_started",
                        manager_session_id = %registration.manager_session_id,
                        windows_session_id = session.session_id,
                        authentication_id = session.authentication_id,
                    );
                }
                (true, None) => {
                    end_registration(sid, registration);
                }
                (true, Some(session)) => {
                    if registration.windows_session_id == Some(session.session_id)
                        && registration.authentication_id == Some(session.authentication_id)
                    {
                        persist_runtime(sid, registration);
                    } else {
                        end_registration(sid, registration);
                    }
                }
                (false, None) => {}
            }
        }
    }

    pub fn detach_all(&self) {
        let controllers = std::mem::take(
            &mut *self
                .controllers
                .lock()
                .expect("host controller lock poisoned"),
        );
        for (sid, registration) in controllers {
            persist_runtime(&sid, &registration);
            if let Some(runner) = registration.runner {
                runner.detach();
            }
        }
    }

    pub fn end_all(&self) {
        let mut controllers = self
            .controllers
            .lock()
            .expect("host controller lock poisoned");
        for (sid, registration) in &mut *controllers {
            end_registration(sid, registration);
        }
    }

    pub fn end_windows_session(&self, session_id: u32) {
        let mut controllers = self
            .controllers
            .lock()
            .expect("host controller lock poisoned");
        let mut ended_logons = Vec::new();
        for (sid, registration) in &mut *controllers {
            if registration.windows_session_id == Some(session_id) {
                if let Some(authentication_id) = registration.authentication_id {
                    ended_logons.push((session_id, authentication_id));
                }
                end_registration(sid, registration);
            }
        }
        drop(controllers);
        self.ending_windows_sessions
            .lock()
            .expect("host ending-session lock poisoned")
            .extend(ended_logons);
    }

    fn status(&self, sid: &str) -> HostStatus {
        let controllers = self
            .controllers
            .lock()
            .expect("host controller lock poisoned");
        if let Some(registration) = controllers.get(sid) {
            let process_id = registration
                .runner
                .as_ref()
                .map_or(0, ControllerRunner::process_id);
            HostStatus {
                registered: true,
                controller_running: process_id != 0,
                manager_session_id: registration.manager_session_id.clone(),
                controller_process_id: process_id,
                message: if registration.session_conflict {
                    "multiple eligible Windows sessions are active for this user".to_owned()
                } else {
                    String::new()
                },
            }
        } else {
            HostStatus {
                registered: false,
                controller_running: false,
                manager_session_id: String::new(),
                controller_process_id: 0,
                message: String::new(),
            }
        }
    }
}

#[tonic::async_trait]
impl HostControlService for HostRpc {
    async fn register_user(
        &self,
        request: Request<RegisterUserRequest>,
    ) -> Result<Response<RegisterUserResponse>, Status> {
        let caller = caller(&request)?;
        let sid = caller.sid.to_string();
        registration::add(&sid).map_err(|error| Status::internal(error.to_string()))?;
        let mut controllers = self
            .controllers
            .lock()
            .expect("host controller lock poisoned");
        let registration = controllers
            .entry(sid.clone())
            .or_insert_with(|| Registration {
                manager_session_id: String::new(),
                windows_session_id: None,
                authentication_id: None,
                ending_event: None,
                runner: None,
                recorded_process: None,
                session_conflict: false,
            });
        if registration.runner.is_none() {
            let mut inventory = registration::active_sessions()
                .map_err(|error| Status::internal(error.to_string()))?;
            registration.session_conflict = inventory.conflicts.contains(&sid);
            let session = if registration.session_conflict {
                None
            } else {
                inventory.sessions.remove(&sid)
            };
            if let Some(mut session) = session {
                let manager_session_id = Uuid::now_v7().to_string();
                let ending_event = EndingEvent::create(&manager_session_id, &caller.sid)
                    .map_err(|error| Status::internal(error.to_string()))?;
                let runner = ControllerRunner::from_session_token(
                    session.take_token(),
                    session.session_id,
                    manager_session_id.clone(),
                    runtime_observer(
                        sid.clone(),
                        manager_session_id.clone(),
                        session.session_id,
                        session.authentication_id,
                    ),
                )
                .map_err(|error| Status::failed_precondition(error.to_string()))?;
                registration.manager_session_id = manager_session_id;
                registration.windows_session_id = Some(session.session_id);
                registration.authentication_id = Some(session.authentication_id);
                registration.ending_event = Some(ending_event);
                registration.runner = Some(runner);
                persist_runtime(&sid, registration);
                tracing::info!(
                    name = "manager_session_started",
                    manager_session_id = %registration.manager_session_id,
                    windows_session_id = session.session_id,
                    authentication_id = session.authentication_id,
                );
            }
        }
        drop(controllers);
        Ok(Response::new(RegisterUserResponse {
            status: Some(self.status(&sid)),
        }))
    }

    async fn unregister_user(
        &self,
        request: Request<UnregisterUserRequest>,
    ) -> Result<Response<UnregisterUserResponse>, Status> {
        let sid = caller(&request)?.sid.to_string();
        registration::remove(&sid).map_err(|error| Status::internal(error.to_string()))?;
        let registration = self
            .controllers
            .lock()
            .expect("host controller lock poisoned")
            .remove(&sid);
        if let Some(registration) = registration {
            let mut registration = registration;
            end_registration(&sid, &mut registration);
        }
        Ok(Response::new(UnregisterUserResponse {
            status: Some(self.status(&sid)),
        }))
    }

    async fn get_controller_status(
        &self,
        request: Request<GetControllerStatusRequest>,
    ) -> Result<Response<GetControllerStatusResponse>, Status> {
        let sid = caller(&request)?.sid.to_string();
        Ok(Response::new(GetControllerStatusResponse {
            status: Some(self.status(&sid)),
        }))
    }

    async fn restart_controller(
        &self,
        request: Request<RestartControllerRequest>,
    ) -> Result<Response<RestartControllerResponse>, Status> {
        let sid = caller(&request)?.sid.to_string();
        let controllers = self
            .controllers
            .lock()
            .expect("host controller lock poisoned");
        let registration = controllers
            .get(&sid)
            .ok_or_else(|| Status::not_found("user is not registered"))?;
        registration
            .runner
            .as_ref()
            .ok_or_else(|| Status::unavailable("no eligible interactive session is active"))?
            .restart();
        drop(controllers);
        Ok(Response::new(RestartControllerResponse {
            status: Some(self.status(&sid)),
        }))
    }
}

fn end_registration(sid: &str, registration: &mut Registration) {
    if !registration.manager_session_id.is_empty() {
        tracing::info!(
            name = "manager_session_ending",
            manager_session_id = %registration.manager_session_id,
            windows_session_id = ?registration.windows_session_id,
        );
    }
    if let Some(event) = &registration.ending_event {
        let _ = event.signal();
    }
    if let Some(runner) = registration.runner.take() {
        runner.end_session();
    }
    registration.manager_session_id.clear();
    registration.windows_session_id = None;
    registration.authentication_id = None;
    registration.ending_event = None;
    registration.recorded_process = None;
    let _ = registration::remove_runtime(sid);
}

fn persist_runtime(sid: &str, registration: &Registration) {
    let Some(windows_session_id) = registration.windows_session_id else {
        return;
    };
    let Some(authentication_id) = registration.authentication_id else {
        return;
    };
    let identity = registration
        .runner
        .as_ref()
        .and_then(ControllerRunner::process_identity)
        .or(registration.recorded_process);
    let _ = registration::save_runtime(
        sid,
        &RuntimeSession {
            manager_session_id: registration.manager_session_id.clone(),
            windows_session_id,
            authentication_id,
            controller_process_id: identity.map_or(0, |identity| identity.process_id),
            controller_creation_time: identity.map_or(0, |identity| identity.creation_time),
        },
    );
}

fn runtime_observer(
    sid: String,
    manager_session_id: String,
    windows_session_id: u32,
    authentication_id: u64,
) -> ProcessObserver {
    Arc::new(move |identity| {
        tracing::info!(
            name = "controller_process_changed",
            manager_session_id = %manager_session_id,
            windows_session_id,
            process_id = identity.map_or(0, |identity| identity.process_id),
        );
        let _ = registration::save_runtime(
            &sid,
            &RuntimeSession {
                manager_session_id: manager_session_id.clone(),
                windows_session_id,
                authentication_id,
                controller_process_id: identity.map_or(0, |identity| identity.process_id),
                controller_creation_time: identity.map_or(0, |identity| identity.creation_time),
            },
        );
    })
}

fn signal_previous_session(registration: &Registration, sid: &UserSid) {
    if registration.manager_session_id.is_empty() {
        return;
    }
    if let Ok(event) = EndingEvent::create(&registration.manager_session_id, sid) {
        let _ = event.signal();
    }
}

fn caller<T>(request: &Request<T>) -> Result<CallerIdentity, Status> {
    request
        .extensions()
        .get::<CallerIdentity>()
        .cloned()
        .ok_or_else(|| Status::unauthenticated("named-pipe caller identity is missing"))
}
