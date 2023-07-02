use std::net::{SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::time::Duration;

use crate::proto::{Ctl, CtlRequest, CtlResponse, Session};
use crate::{
    api,
    api::{cmd, query},
    conf, err,
    lan::LanManager,
    plat, secret,
    state::State,
};

use p2p::pairing::PairingAuthenticator;
use p2p::peer::{PeerCandidate, PeerId, PeerMetadata};
use p2p::{
    discovery,
    event::P2pEvent,
    manager::{P2pConfig, P2pManager},
};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, Id};

pub struct Node {
    /// the node configuration
    conf: conf::NodeConfig,

    /// the node configuration storage
    store: conf::NodeConfigStore,

    /// the p2p manager
    p2p: std::sync::Arc<P2pManager>,

    /// the local network manager
    // lan: LanManager,

    /// in-memory state of the node
    state: State,

    /// a channel for the ui to send queries w/ returnable values
    query: (
        mpsc::UnboundedSender<api::QueryMsg>,
        mpsc::UnboundedReceiver<api::QueryMsg>,
    ),

    /// a channel for the ui to send commands w/ returnable values
    cmd: (
        mpsc::UnboundedSender<api::CmdMsg>,
        mpsc::UnboundedReceiver<api::CmdMsg>,
    ),

    /// a channel for child threads to send events back to the core
    internal: (
        mpsc::UnboundedSender<InternalEvent>,
        mpsc::UnboundedReceiver<InternalEvent>,
    ),

    /// a channel sender for core to send events to the ui
    events: mpsc::Sender<CoreEvent>,

    /// a channel receiver for core to receive p2p events
    p2p_events: mpsc::UnboundedReceiver<P2pEvent>,
}

impl Node {
    pub async fn init(dir: PathBuf) -> Result<(Self, mpsc::Receiver<CoreEvent>), err::CoreError> {
        // build node config from disk or create
        let store: conf::NodeConfigStore = dir.into();
        let conf = store.get()?;

        // build lan
        let mut lan = LanManager::new()?;
        let local = lan.next_ipv4_up().await;

        // build p2p
        let p2p_conf = P2pConfig {
            id: conf.id.clone(),
            device: plat::DEVICE_TYPE,
            name: conf.name.clone(),
            multicast: SocketAddr::V4(SocketAddrV4::new(discovery::DISCOVERY_MULTICAST, 50692)), // TODO 0 port??
            p2p_addr: SocketAddr::V4(SocketAddrV4::new(
                // *lan.lan
                //     .iter()
                //     .next()
                //     .ok_or(err::CoreError::NoNetworkAccess)?,
                local, 0,
            )),
        };
        let (p2p, p2p_events) = P2pManager::new(p2p_conf).await?;

        // append known peers
        for p in secret::to_known(&conf.known_peers) {
            p2p.add_known_peer(p);
        }

        let (events, events_rx) = mpsc::channel(64);

        let node = Self {
            conf,
            store,
            p2p,
            // lan,
            state: State::default(),
            query: mpsc::unbounded_channel(),
            cmd: mpsc::unbounded_channel(),
            internal: mpsc::unbounded_channel(),
            events,
            p2p_events,
        };

        Ok((node, events_rx))
    }

    // called by
    pub async fn start(&mut self) {
        // TODO: start p2p event loop here?
        loop {
            tokio::select! {
                Some(q) = self.query.1.recv() => {
                    let res = self.handle_query(q.req).await;
                    if let Err(e) = &res {
                        tracing::error!("Could not handle query: {}", e);
                    }
                    q.res.send(res).unwrap_or(());
                }
                Some(c) = self.cmd.1.recv() => {
                    let res = self.handle_command(c.req).await;
                    if let Err(e) = &res {
                        tracing::error!("Could not handle command: {}", e);
                    }
                    c.res.send(res).unwrap_or(());
                }
                Some(e) = self.internal.1.recv() => {
                    let res = self.handle_event(e).await;
                    if let Err(e) = &res {
                        tracing::error!("Could not handle internal event: {}", e);
                    }
                }
                Some(e) = self.p2p_events.recv() => {
                    let res = self.handle_p2p(e).await;
                    if let Err(e) = &res {
                        tracing::error!("Could not handle p2p event: {}", e);
                    }
                }
                // Ok(n) = self.lan.next() => {
                //     debug!("LAN event: {:?}", n);
                // }

            }
        }

        tracing::debug!("Shutting down node")

        // get state from p2p and persist
    }

    // pub async fn get_conf(&self) -> conf::NodeConfig {
    //     self.conf.clone()
    // }

    // pub async fn set_config(&self, new: conf::NodeConfig) {
    //     _ = self.get_cmd_api().send(cmd::Request::SetConfig(new)).await;
    // }

    // pub async fn start_discovery(&self) {
    //     _ = self.get_cmd_api().send(cmd::Request::StartDiscovery).await;
    // }

    // pub async fn stop_discovery(&self) {
    //     _ = self.get_cmd_api().send(cmd::Request::StopDiscovery).await;
    // }

    // pub async fn send_peer(&self, id: PeerId, req: PeerRequest) -> cmd::Response {
    //     self.get_cmd_api()
    //         .send(cmd::Request::SendPeer(id, req))
    //         .await
    //         .unwrap_or(cmd::Response::Err)
    // }

    // pub fn get_controller(&self) -> CoreController {
    //     CoreController {
    //         query_tx: self.query.0.clone(),
    //         command_tx: self.cmd.0.clone(),
    //     }
    // }
    pub fn get_query_api(&self) -> api::QueryApi {
        api::Api {
            tx: self.query.0.clone(),
        }
    }

    pub fn get_cmd_api(&self) -> api::CmdApi {
        api::Api {
            tx: self.cmd.0.clone(),
        }
    }

    // handle queries
    async fn handle_query(&self, query: query::Request) -> Result<query::Response, err::CoreError> {
        Ok(match query {
            query::Request::GetConf => query::Response::Conf(self.conf.clone()),
            query::Request::GetDiscoveredPeers => {
                query::Response::DiscoveredPeers(self.p2p.get_discovered_peers())
            }
            query::Request::GetSharableQrCode(shared_secret) => {
                // is the optional shared secret is set, that means this is the second stage of pairing 2 devices
                let secret = match shared_secret {
                    None => rand::random::<u64>().to_string(),
                    Some(s) => s,
                };
                let payload = serde_json::to_vec(&QrPayload {
                    secret,
                    peer: self.p2p.get_metadata().clone(),
                })?;
                // let code = qrcode::QrCode::new(payload)?;
                // let image = code.render::<image::Luma<u8>>().build();
                query::Response::SharableQrCode(payload)
            }
        })
    }

    // handle commands
    async fn handle_command(&mut self, cmd: cmd::Request) -> Result<cmd::Response, err::CoreError> {
        match cmd {
            cmd::Request::StartDiscovery => {
                if self.state.discovery_ct.is_none() {
                    let token = CancellationToken::new();
                    self.state.discovery_ct = Option::Some(token.clone());
                    info!("channel close? {}", self.p2p.is_discovery_channel_closed());
                    let p2p = self.p2p.clone();
                    info!("channel close? {}", p2p.is_discovery_channel_closed());

                    tokio::spawn(async move {
                        debug!("request presence loop started");
                        info!("channel close? {}", p2p.is_discovery_channel_closed());
                        p2p.request_presence().await;
                        loop {
                            info!("channel close.? {}", p2p.is_discovery_channel_closed());
                            sleep(Duration::from_secs(2)).await;
                            tokio::select! {
                                _ = token.cancelled() => break,
                                _ = p2p.request_presence() => continue
                            };
                        }
                        debug!("request presence loop stopped");
                    });
                }
            }
            cmd::Request::StopDiscovery => {
                if let Some(token) = &self.state.discovery_ct {
                    token.cancel();
                }
                self.state.discovery_ct = None;
            }
            cmd::Request::SendPeer(id, request) => {
                // Current state: "similar" to HTTP 1 - 1 request & N responses per connection
                // TODO: save and reuse connections (HTTP 1.1)
                // TODO: support more complex flows, timeouts, etc.

                let peer = self.p2p.connect_to_peer(&id).await?;
                let tx = self.internal.0.clone();
                self.state.session_id += 1;
                let session = Session {
                    id: self.state.session_id,
                    ctl: Ctl::Request(request.into()),
                };
                tokio::spawn(crate::peer::client_handler(peer, session, tx));
            }
            cmd::Request::SetConfig(mut new) => {
                new.id = self.conf.id.clone();
                self.store.set(&new)?;
                self.conf = new;
            }
            cmd::Request::Pair(json) => {
                let payload: QrPayload = serde_json::from_slice(json.as_slice())?;
                let auth = PairingAuthenticator::new(payload.secret.into_bytes())?;
                let known = PeerCandidate::new(&payload.peer, auth);
                self.conf.known_peers.insert(payload.peer);
                self.store.set(&self.conf)?;
                self.p2p.add_known_peer(known);
            }
            cmd::Request::Ack(_, session, ack) => {
                if let Some(s) = self.state.sessions.remove(&session) {
                    _ = s
                        .send(Session {
                            id: session,
                            ctl: Ctl::Response(ack.into()),
                        })
                        .await;
                }
            }
        }
        Ok(cmd::Response::Ok)
    }

    // handle events
    async fn handle_event(&mut self, event: InternalEvent) -> Result<(), err::CoreError> {
        match event {
            InternalEvent::InboundSession { meta, body, tx } => {
                self.state.sessions.insert(body.id, tx.clone());
                match body.ctl {
                    Ctl::Request(CtlRequest::LaunchUri(uri)) => {
                        let msg = match self.conf.auto_accept {
                            true => (
                                CoreEvent::LaunchUri(meta.id, body.id, uri),
                                CtlResponse::Success,
                            ),
                            false => (
                                CoreEvent::AskLaunchUri(meta.id, body.id, uri),
                                CtlResponse::Waiting,
                            ),
                        };
                        let res = match self.events.send(msg.0).await {
                            Err(_) => CtlResponse::Error(crate::proto::CTL_UNKNOWN_ERR),
                            Ok(()) => msg.1,
                        };
                        _ = tx
                            .send(Session {
                                id: body.id,
                                ctl: Ctl::Response(res),
                            })
                            .await;
                    }
                    x => error!("unhandled app ctl request: {:?}", x),
                }
            }
            InternalEvent::SessionResult { id, body } => match body.ctl {
                Ctl::Response(status) => {
                    let msg = match status {
                        CtlResponse::Error(code) => {
                            error!("Failed to perform app control: {}", code);
                            self.state.sessions.get(&body.id); // drop
                            CoreEvent::PeerCtlFailed(id)
                        }
                        CtlResponse::Success => {
                            self.state.sessions.get(&body.id); // drop
                            CoreEvent::PeerCtlSuccess(id)
                        }
                        CtlResponse::Cancel => {
                            self.state.sessions.get(&body.id); // drop
                            CoreEvent::PeerCtlCancel(id)
                        }
                        CtlResponse::Waiting => CoreEvent::PeerCtlWaiting(id),
                    };
                    _ = self.events.send(msg).await;
                }
                x => error!("Unhandled app ctl response {:?}", x),
            },
        }

        Ok(())
    }

    // handle p2p events
    async fn handle_p2p(&mut self, event: P2pEvent) -> Result<(), err::CoreError> {
        match event {
            P2pEvent::PeerDiscovered(peer) => {
                _ = self.events.send(CoreEvent::Discovered(peer)).await
            }
            P2pEvent::PeerDisconnected(_) => {}
            P2pEvent::PeerConnected(peer) => {
                // not sending to UI
                let tx = self.internal.0.clone();
                tokio::spawn(crate::peer::server_handler(peer, tx));
            }
        }

        Ok(())
    }
}

// events to be subscribed to by the application ui
#[derive(Debug, Serialize, Deserialize)]
pub enum CoreEvent {
    Discovered(PeerMetadata),
    AskLaunchUri(PeerId, u64, String),
    LaunchUri(PeerId, u64, String),
    PeerCtlWaiting(PeerId),
    PeerCtlSuccess(PeerId),
    PeerCtlCancel(PeerId),
    PeerCtlFailed(PeerId),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct QrPayload {
    pub secret: String,
    pub peer: PeerMetadata,
}

// // commands and queries sent from the application layer to core
// #[repr(C)]
// pub enum AppCmd {
//     SetName(String),
//     Discover(u8),
// }

// pub enum AppQuery {
//     GetConf,
// }

// #[derive(Serialize, Deserialize, Debug)]
// #[serde(tag = "key", content = "data")]
// #[ts(export)]
// #[repr(u8)]
// pub enum CoreResponse {
//     Ok,
//     Conf(p2p::peer::PeerId), // ClientGetState(ClientState),
//                              // Sum(i32),
// }

/// Events from child threads
pub(crate) enum InternalEvent {
    /// A remote client sent a session request
    InboundSession {
        /// the peer's metadata
        meta: PeerMetadata,
        /// The session id
        body: Session,
        // the channel for session responses
        tx: mpsc::Sender<Session>,
    },
    /// A remote host sent a session response
    SessionResult { id: PeerId, body: Session },
}

// a wrapper around external input with a returning sender channel for core to respond
// #[derive(Debug)]
// pub struct ReturnableMessage<D, R = Result<CoreResponse, err::CoreError>> {
//     data: D,
//     tx_return: tokio::sync::oneshot::Sender<R>,
// }

// pub struct Ctx<T> {
//     tx: mpsc::UnboundedSender<ReturnableMessage<T>>,
// }

// impl Ctx<AppQuery> {
//     pub async fn send(&self, msg: AppQuery) -> Result<CoreResponse, err::CoreError> {
//         let (tx, rx) = tokio::sync::oneshot::channel();
//         let payload = ReturnableMessage {
//             data: msg,
//             tx_return: tx,
//         };

//         self.tx.send(payload).unwrap_or(());
//         rx.await.unwrap()
//     }
// }

// impl Ctx<AppCmd> {
//     pub async fn send(&self, msg: AppCmd) -> Result<CoreResponse, err::CoreError> {
//         let (tx, rx) = tokio::sync::oneshot::channel();
//         let payload = ReturnableMessage {
//             data: msg,
//             tx_return: tx,
//         };

//         self.tx.send(payload).unwrap_or(());
//         rx.await.unwrap()
//     }
// }

// core controller is passed to the client to communicate with the core which runs in a dedicated thread
// #[derive(Clone)]
// pub struct CoreController {
//     query_tx: mpsc::UnboundedSender<ReturnableMessage<AppQuery>>,
//     command_tx: mpsc::UnboundedSender<ReturnableMessage<AppCmd>>,
// }

// impl CoreController {
//     pub async fn query(&self, query: AppQuery) -> Result<CoreResponse, err::CoreError> {
// let (tx, rx) = tokio::sync::oneshot::channel();
// let payload = ReturnableMessage {
//     data: query,
//     tx_return: tx,
// };

// self.query_tx.send(payload).unwrap_or(());
// rx.await.unwrap()
//     }

//     pub async fn command(&self, cmd: AppCmd) -> Result<CoreResponse, err::CoreError> {
//         let (tx, rx) = tokio::sync::oneshot::channel();
//         let payload = ReturnableMessage {
//             data: cmd,
//             tx_return: tx,
//         };

//         self.command_tx.send(payload).unwrap_or(());
//         rx.await.unwrap()
//     }
// }