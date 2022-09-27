// SPDX-License-Identifier: (MIT OR Apache-2.0)
// Copyright Authors of bpfd
use std::sync::{Arc, Mutex};

use bpfd_api::v1::{
    list_response::ListResult, load_request::AttachType, loader_server::Loader, ListRequest,
    ListResponse, LoadRequest, LoadResponse, UnloadRequest, UnloadResponse,
};
use log::warn;
use tokio::sync::{mpsc, mpsc::Sender, oneshot};
use tonic::{Request, Response, Status};
use uuid::Uuid;
use x509_certificate::X509Certificate;

use crate::server::{bpf::InterfaceInfo, errors::BpfdError, pull_bytecode::pull_bytecode};

#[derive(Debug, Default)]
struct User {
    username: String,
}

static DEFAULT_USER: User = User {
    username: String::new(),
};

/// This function will get called on each inbound request.
/// It extracts the username from the client certificate and adds it to the request
pub(crate) fn intercept(mut req: Request<()>) -> Result<Request<()>, Status> {
    let certs = req
        .peer_certs()
        .ok_or_else(|| Status::unauthenticated("no certificate provided"))?;

    if certs.len() != 1 {
        return Err(Status::unauthenticated(
            "expected only one client certificate",
        ));
    }

    let cert = X509Certificate::from_der(certs[0].get_ref()).unwrap();
    let username = cert
        .subject_common_name()
        .ok_or_else(|| Status::unauthenticated("CN is empty"))?;

    req.extensions_mut().insert(User { username });
    Ok(req)
}

#[derive(Debug)]
pub struct BpfdLoader {
    tx: Arc<Mutex<Sender<Command>>>,
}

/// Provided by the requester and used by the manager task to send
/// the command response back to the requester.
type Responder<T> = oneshot::Sender<T>;

impl BpfdLoader {
    pub(crate) fn new(tx: mpsc::Sender<Command>) -> BpfdLoader {
        let tx = Arc::new(Mutex::new(tx));
        BpfdLoader { tx }
    }
}

#[tonic::async_trait]
impl Loader for BpfdLoader {
    async fn load(&self, request: Request<LoadRequest>) -> Result<Response<LoadResponse>, Status> {
        let mut reply = LoadResponse { id: String::new() };
        let username = request
            .extensions()
            .get::<User>()
            .unwrap_or(&DEFAULT_USER)
            .username
            .to_string();
        let mut request = request.into_inner();

        if request.from_image {
            // Pull image from Repo if not locally here, dump bytecode
            let internal_program_overrides = pull_bytecode(&request.path).await;
            match internal_program_overrides {
                Ok(internal_program_overrides) => {
                    request.path = internal_program_overrides.path;
                    request.section_name = internal_program_overrides.image_meta.section_name;
                }
                Err(e) => return Err(Status::aborted(format!("{e}"))),
            };
        }

        let (resp_tx, resp_rx) = oneshot::channel();

        if request.attach_type.is_none() {
            return Err(Status::aborted("message missing attach_type"));
        }
        let cmd = match request.attach_type.unwrap() {
            AttachType::NetworkMultiAttach(attach) => Command::Load {
                iface: attach.iface,
                responder: resp_tx,
                path: request.path,
                program_type: request.program_type,
                priority: attach.priority,
                section_name: request.section_name,
                proceed_on: attach.proceed_on,
                username,
            },
            _ => unimplemented!("attach type not yet implemented"),
        };

        let tx = self.tx.lock().unwrap().clone();
        // Send the GET request
        tx.send(cmd).await.unwrap();

        // Await the response
        match resp_rx.await {
            Ok(res) => match res {
                Ok(id) => {
                    reply.id = id.to_string();
                    Ok(Response::new(reply))
                }
                Err(e) => {
                    warn!("BPFD load error: {}", e);
                    Err(Status::aborted(format!("{e}")))
                }
            },

            Err(e) => {
                warn!("RPC load error: {}", e);
                Err(Status::aborted(format!("{e}")))
            }
        }
    }

    async fn unload(
        &self,
        request: Request<UnloadRequest>,
    ) -> Result<Response<UnloadResponse>, Status> {
        let reply = UnloadResponse {};
        let username = request
            .extensions()
            .get::<User>()
            .unwrap_or(&DEFAULT_USER)
            .username
            .to_string();
        let request = request.into_inner();
        let id = request
            .id
            .parse()
            .map_err(|_| Status::invalid_argument("invalid id"))?;

        let (resp_tx, resp_rx) = oneshot::channel();
        let cmd = Command::Unload {
            id,
            iface: request.iface,
            username,
            responder: resp_tx,
        };

        let tx = self.tx.lock().unwrap().clone();
        // Send the GET request
        tx.send(cmd).await.unwrap();

        // Await the response
        match resp_rx.await {
            Ok(res) => match res {
                Ok(_) => Ok(Response::new(reply)),
                Err(e) => {
                    warn!("BPFD unload error: {}", e);
                    Err(Status::aborted(format!("{e}")))
                }
            },
            Err(e) => {
                warn!("RPC unload error: {}", e);
                Err(Status::aborted(format!("{e}")))
            }
        }
    }

    async fn list(&self, request: Request<ListRequest>) -> Result<Response<ListResponse>, Status> {
        let mut reply = ListResponse {
            xdp_mode: String::new(),
            results: vec![],
        };
        let request = request.into_inner();

        let (resp_tx, resp_rx) = oneshot::channel();
        let cmd = Command::List {
            iface: request.iface,
            responder: resp_tx,
        };

        let tx = self.tx.lock().unwrap().clone();
        // Send the GET request
        tx.send(cmd).await.unwrap();

        // Await the response
        match resp_rx.await {
            Ok(res) => match res {
                Ok(results) => {
                    reply.xdp_mode = results.xdp_mode;
                    for r in results.programs {
                        reply.results.push(ListResult {
                            id: r.id,
                            name: r.name,
                            path: r.path,
                            position: r.position as u32,
                            priority: r.priority,
                            proceed_on: r.proceed_on,
                        })
                    }
                    Ok(Response::new(reply))
                }
                Err(e) => match e {
                    BpfdError::NoProgramsLoaded => Ok(Response::new(reply)),
                    _ => {
                        warn!("BPFD list error: {}", e);
                        Err(Status::aborted(format!("{e}")))
                    }
                },
            },
            Err(e) => {
                warn!("RPC list error: {}", e);
                Err(Status::aborted(format!("{e}")))
            }
        }
    }
}

/// Multiple different commands are multiplexed over a single channel.
#[derive(Debug)]
pub(crate) enum Command {
    Load {
        program_type: i32,
        iface: String,
        path: String,
        priority: i32,
        section_name: String,
        proceed_on: Vec<i32>,
        username: String,
        responder: Responder<Result<Uuid, BpfdError>>,
    },
    Unload {
        id: Uuid,
        iface: String,
        username: String,
        responder: Responder<Result<(), BpfdError>>,
    },
    List {
        iface: String,
        responder: Responder<Result<InterfaceInfo, BpfdError>>,
    },
}
