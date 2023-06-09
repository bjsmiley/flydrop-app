use futures::{SinkExt, StreamExt};
use std::net::{Ipv4Addr, SocketAddr};
use tokio::{net::UdpSocket, sync::mpsc};
use tokio_util::udp::UdpFramed;
use tracing::{debug, error};

use crate::{event::DiscoveryEvent, proto::DiscoveryCodec};

pub static DISCOVERY_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 42, 98);

pub fn multicast(
    addr: &SocketAddr,
    multi_addr: &SocketAddr,
) -> Result<(UdpSocket, SocketAddr), std::io::Error> {
    use socket2::{Domain, Protocol, Socket, Type};

    assert!(multi_addr.ip().is_multicast(), "Must be multcast address");
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&socket2::SockAddr::from(*addr))?;
    socket.set_multicast_loop_v4(true)?;
    if let (SocketAddr::V4(a), SocketAddr::V4(m)) = (addr, multi_addr) {
        socket.join_multicast_v4(m.ip(), a.ip())?
    }
    socket.set_nonblocking(true)?;
    Ok((UdpSocket::from_std(socket.into())?, *multi_addr))
}

pub fn start(
    sock: UdpSocket,
    addr: SocketAddr,
) -> (
    mpsc::Sender<DiscoveryEvent>,
    mpsc::Receiver<(DiscoveryEvent, SocketAddr)>,
) {
    let (app_tx, mut app_rx) = mpsc::channel(1024);
    let (transport_tx, transport_rx) = mpsc::channel::<(DiscoveryEvent, SocketAddr)>(1024);
    let discovery_socket = sock;

    tokio::spawn(async move {
        let local_addr = discovery_socket.local_addr().unwrap();
        let (mut writer, mut reader) = UdpFramed::new(discovery_socket, DiscoveryCodec).split();
        let mut just_send_request = false;
        loop {
            tokio::select! {
                broadcast = app_rx.recv() => {
                    if let Some(event) = broadcast {
                        match event {
                            DiscoveryEvent::PresenceRequest => {
                                debug!("Sending PresenceRequest");
                                // this is hacky
                                just_send_request = true;
                                if let Err(error) = writer.send((event, addr)).await {
                                    error!("Error sending PresenceRequest: {:?}", error);
                                }
                            },
                            DiscoveryEvent::PresenceResponse(_) => {
                                debug!("Sending PresenceResponse");
                                if let Err(error) = writer.send((event, addr)).await {
                                    error!("Error sending PresenceResponse: {:?}", error);
                                }
                            },
                        }
                    }
                    else {
                        debug!("Discovery shutting down. Application Sender closed.");
                        break;
                    }
                }
                network = reader.next() => {
                    if let Some(result) = network {
                        match result {
                            Ok(frame) => {

                                // this is hacky to avoid presence requests from self
                                if just_send_request {
                                    if let (DiscoveryEvent::PresenceRequest, addr) = frame {
                                        if local_addr == addr {
                                            just_send_request = false;
                                            continue;
                                        }
                                    }
                                }
                                debug!("Recieved Discovery event");
                                if (transport_tx.send(frame).await).is_err() {
                                    debug!("Discovery shutting down. Transport Sender closed.");
                                    break;
                                }
                            },
                            Err(error) => {
                                error!("error reading from Discovery: {:?}", error)
                            }
                        }
                    }
                }
            }
        }
    });

    (app_tx, transport_rx)
}
