use std::{sync::Arc, time::Duration};

use futures::{SinkExt, StreamExt};
use tokio::{net::TcpStream, time::timeout};
use tokio_util::codec::Framed;
use tracing::{debug, error};

use crate::{
    err, hmac,
    manager::P2pManager,
    peer::{Peer, PeerCandidate},
    proto::{Connection, ConnectionCodec},
};

const TIMEOUT_ERR: u32 = 2001;
const NOT_FOUND_ERR: u32 = 2002;
const AUTH_ERR: u32 = 2003;

/// handshake as the client to attempt to connect as a connected peer
pub(crate) async fn connect(
    manager: &Arc<P2pManager>,
    conn: TcpStream,
    peer: &PeerCandidate,
) -> Result<Peer, err::ConnError> {
    // get auth code
    let code = peer.auth.generate().unwrap();
    let key = code.as_bytes();
    let tag = hmac::sign(key, manager.id.as_bytes());

    // send a connect request
    let mut frame = Framed::new(conn, ConnectionCodec);
    frame
        .send(Connection::Request {
            id: manager.id.clone(),
            tag: tag.as_ref().to_vec(),
        })
        .await?;

    // wait for a connect response
    let Ok(response) = timeout(Duration::from_secs(1), frame.next()).await else {
        error!("peer timed out waiting for ConnectResponse");
        _ = frame.send(crate::proto::Connection::Failure(TIMEOUT_ERR)).await;
        return Err(err::ConnError::Timeout);
    };
    match response {
        None => {
            error!("peer closed the connection");
            Err(err::ConnError::Disconnect)
        }
        Some(res) => {
            match res? {
                Connection::Response(tag) => {
                    debug!("validating peer's totp code");
                    if let Err(e) = hmac::verify(key, peer.id.as_bytes(), &tag) {
                        error!("Error verifying totp hmac: {:?}", e);
                        _ = frame
                            .send(crate::proto::Connection::Failure(AUTH_ERR))
                            .await;
                        return Err(err::ConnError::Auth);
                    }
                    // send a complete request & wait for a complete response
                    frame.send(Connection::CompleteRequest).await?;
                    let Ok(complete) = timeout(Duration::from_secs(1), frame.next()).await else {
                        error!("peer timed out waiting for ConnectionCompleteResponse");
                        _ = frame.send(crate::proto::Connection::Failure(TIMEOUT_ERR)).await;
                        return Err(err::ConnError::Timeout);
                    };
                    match complete {
                        Some(res) => match res? {
                            Connection::CompleteResponse => {
                                let connected = Peer::new(
                                    manager,
                                    crate::peer::ConnectionType::Client,
                                    frame.into_inner(),
                                    peer.metadata.clone(),
                                )
                                .unwrap();
                                debug!("Peer is connected!");
                                Ok(connected)
                            }
                            _ => {
                                error!("peer recieved the wrong message instead of ConnectionCompleteResponse");
                                Err(err::ConnError::Msg)
                            }
                        },
                        None => {
                            error!("peer closed the connection");
                            Err(err::ConnError::Disconnect)
                        }
                    }
                }
                Connection::Failure(code) => {
                    error!("received error {} instead of ConnectionResponse", code);
                    Err(err::ConnError::Failure(code))
                }
                _ => {
                    error!("peer recieved the wrong message instead of ConnectionResponse");
                    Err(err::ConnError::Msg)
                }
            }
        }
    }
}

/// handshake as the host to accept an incoming tcp connection as a connected peer
pub(crate) async fn accept(
    manager: &Arc<P2pManager>,
    conn: TcpStream,
) -> Result<Peer, err::ConnError> {
    let mut frame = Framed::new(conn, ConnectionCodec);

    // timeout in 1 sec to ensure no bad intent
    // wait for a connect request
    let Ok(request) = timeout(Duration::from_secs(1), frame.next()).await else {
        error!("peer timed out waiting for ConnectionRequest");
        _ = frame.send(crate::proto::Connection::Failure(TIMEOUT_ERR)).await;
        return Err(err::ConnError::Timeout);
    };
    match request {
        None => {
            error!("peer closed the connection");
            Err(err::ConnError::Disconnect)
        }
        Some(req) => {
            match req? {
                Connection::Request { id, tag } => {
                    let Some(peer) = manager.get_peer_candidate(&id) else {
                        _ = frame.send(crate::proto::Connection::Failure(NOT_FOUND_ERR)).await;
                        error!("peer is not known nor discovered");
                        return Err(err::ConnError::NotFound);
                    };
                    debug!("validating peer's totp code");
                    let code = peer.auth.generate().unwrap();
                    let key = code.as_bytes();
                    if let Err(e) = hmac::verify(key, peer.id.as_bytes(), &tag) {
                        error!("Error verifying totp hmac: {:?}", e);
                        _ = frame
                            .send(crate::proto::Connection::Failure(AUTH_ERR))
                            .await;
                        return Err(err::ConnError::Auth);
                    }
                    let tag = hmac::sign(key, manager.id.as_bytes());
                    // send a connect response & wait for a complete request
                    frame
                        .send(crate::proto::Connection::Response(tag.as_ref().to_vec()))
                        .await?;
                    let Ok(complete) = timeout(Duration::from_secs(1), frame.next()).await else {
                        error!("peer timed out waiting for ConnectionCompleteRequest");
                        _ = frame.send(crate::proto::Connection::Failure(TIMEOUT_ERR)).await;
                        return Err(err::ConnError::Timeout);
                    };
                    match complete {
                        Some(res) => {
                            match res? {
                                Connection::CompleteRequest => {
                                    // send a complete response
                                    frame.send(Connection::CompleteResponse).await?;
                                    let connected = Peer::new(
                                        manager,
                                        crate::peer::ConnectionType::Server,
                                        frame.into_inner(),
                                        peer.metadata,
                                    )
                                    .unwrap();
                                    debug!("Peer is connected!");
                                    Ok(connected)
                                }
                                _ => {
                                    error!("peer recieved the wrong message instead of ConnectionCompleteRequest");
                                    Err(err::ConnError::Msg)
                                }
                            }
                        }
                        None => {
                            error!("peer closed the connection");
                            Err(err::ConnError::Disconnect)
                        }
                    }
                }
                Connection::Failure(code) => {
                    error!("received error {} instead of ConnectionRequest", code);
                    Err(err::ConnError::Failure(code))
                }
                _ => {
                    error!("peer recieved the wrong message instead of ConnectionRequest");
                    Err(err::ConnError::Msg)
                }
            }
        }
    }
}
