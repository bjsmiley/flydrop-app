use std::{
    collections::HashSet,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use dashmap::{DashMap, DashSet};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tracing::{debug, error};

use crate::{
    discovery, err,
    event::*,
    event_loop,
    peer::{DeviceType, Peer, PeerCandidate, PeerId, PeerMetadata},
};

pub struct P2pManager {
    // store internal state
    /// PeerId is the unique identifier of the current peer.
    pub(crate) id: PeerId,

    // /// identity is the TLS identity of the current peer.
    // pub(crate) identity: (Certificate, PrivateKey),
    /// The metadata of the current peer
    pub(crate) metadata: PeerMetadata,

    /// known_peers are peers who have been previously paired up with, only from these peers can the
    /// P2p Manager discover and connect with.
    known_peers: DashMap<PeerId, PeerCandidate>,

    /// discovered_peers contains a list of all peers which have been discovered by any discovery mechanism.
    discovered_peers: DashMap<PeerId, PeerCandidate>,

    /// connected_peers
    connected_peers: DashSet<PeerId>,

    /// channel to send Discovery events
    discovery_channel: mpsc::UnboundedSender<DiscoveryEvent>,

    /// internal_channel is a channel which is used to communicate with the main internal event loop.
    internal_channel: mpsc::UnboundedSender<InternalEvent>,

    /// app_channel is a channel which is used to communicate with the application
    app_channel: mpsc::UnboundedSender<P2pEvent>,

    /// an id for deduplicating presense requests
    pub(crate) dedup: u32,
}

pub struct P2pConfig {
    pub id: PeerId,
    pub device: DeviceType,
    pub name: String,
    pub multicast: SocketAddr,
    pub p2p_addr: SocketAddr,
}

impl P2pManager {
    pub async fn new(
        config: P2pConfig,
    ) -> std::io::Result<(Arc<Self>, mpsc::UnboundedReceiver<P2pEvent>)> {
        // setup discovery
        let local = SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::LOCALHOST,
            config.multicast.port(),
        ));
        let discovery = discovery::multicast(&local, &config.multicast)?;

        // setup tcp listener
        let listener = TcpListener::bind(config.p2p_addr).await?;
        debug!(
            "Peer {} listening on {}",
            config.id.clone(),
            listener.local_addr()?
        );

        // setup metadata
        let metadata = PeerMetadata {
            id: config.id.clone(),
            typ: config.device,
            name: config.name,
            addr: listener.local_addr()?,
        };

        let internal_channel = mpsc::unbounded_channel();
        let app_channel = mpsc::unbounded_channel();
        let discovery_channel = mpsc::unbounded_channel();

        let this = Arc::new(Self {
            id: config.id,
            metadata,
            known_peers: DashMap::new(),
            discovered_peers: DashMap::new(),
            connected_peers: DashSet::new(),
            discovery_channel: discovery_channel.0,
            internal_channel: internal_channel.0,
            app_channel: app_channel.0,
            dedup: rand::random(),
        });

        tokio::spawn(event_loop::p2p_event_loop(
            this.clone(),
            internal_channel.1,
            discovery_channel.1,
            listener,
            discovery,
        ));

        Ok((this, app_channel.1))
    }

    // debug
    pub fn is_discovery_channel_closed(self: &Arc<Self>) -> bool {
        self.discovery_channel.is_closed()
    }

    /// called by the application to populate already known peers
    pub fn add_known_peer(&self, peer: PeerCandidate) {
        self.known_peers.insert(peer.id.clone(), peer);
    }

    // called by the application to send a presenct request
    pub fn request_presence(&self) {
        if let Err(e) = self
            .discovery_channel
            .send(DiscoveryEvent::PresenceRequest(self.dedup))
        {
            tracing::error!("application is unable to request presence: {}", e);
        } else {
            debug!("peer is emitting presence request");
        }
    }

    // application calls this to get local metadata
    pub fn get_metadata(&self) -> &PeerMetadata {
        &self.metadata
    }

    pub fn get_discovered_peers(&self) -> Vec<PeerMetadata> {
        self.discovered_peers
            .iter()
            .map(|p| p.metadata.clone())
            .collect()
    }

    pub fn is_discovered(&self, id: &PeerId) -> bool {
        self.discovered_peers.contains_key(id)
    }

    pub fn is_connected(&self, id: &PeerId) -> bool {
        self.connected_peers.contains(id)
    }

    /// application calls this to connect to a peer
    pub async fn connect_to_peer(self: &Arc<Self>, id: &PeerId) -> Result<Peer, err::ConnError> {
        if self.connected_peers.contains(id) {
            return Err(err::ConnError::Dup);
        }
        let Some(candidate) = self.discovered_peers.get(id) else {
            return Err(err::ConnError::NotFound)
        };

        // let peer = candidate.clone();

        for addr in &candidate.addrs {
            match TcpStream::connect(addr).await {
                Err(e) => {
                    error!("Attempt to connect to address {:?} failed {:?}", addr, e);
                }
                Ok(conn) => {
                    debug!("Attempting to connect to {:?}", addr);
                    let peer = crate::net::connect(self, conn, &candidate).await?;
                    self.connected_peers.insert(id.clone());
                    return Ok(peer);
                }
            }
        }
        Err(err::ConnError::Addr)
    }

    // [START] Crate methods the event loop can call

    /// called by a connected peer's connection handler when closing
    pub(crate) fn peer_disconnected(self: &Arc<Self>, id: &PeerId) {
        self.connected_peers.remove(id);
        if self
            .app_channel
            .send(P2pEvent::PeerDisconnected(id.clone()))
            .is_err()
        {
            error!("failed to send PeerDisconnected event to the application");
        }
    }

    /// called by host handshake to attempt to get the PeerCandidate
    pub(crate) fn get_peer_candidate(&self, id: &PeerId) -> Option<PeerCandidate> {
        self.discovered_peers
            .get(id)
            .map(|p| p.value().clone())
            .or(self.known_peers.get(id).map(|p| p.value().clone()))
    }

    /// event loop calls this to determine if incoming connection is from a discovered peer
    // pub(crate) fn get_known_or_discovered_peer_by_addr(&self, addr: &SocketAddr) -> Option<PeerCandidate> {
    //     let Some(peer) = self.discovered_peers.iter().find(|p| p.addresses.contains(&addr)) else {
    //         return None;
    //     };
    //     Some(peer.value().clone())
    // }

    /// event loop calls this to inform manager a peer was discovered
    pub(crate) fn handle_peer_discovered(&self, peer: PeerMetadata) {
        let id = peer.id.clone();
        if !self.connected_peers.contains(&id) && !self.discovered_peers.contains_key(&id) {
            // TODO: fix not removing
            if let Some(known) = self.known_peers.remove(&id) {
                let mut candidate = PeerCandidate {
                    id: id.clone(),
                    metadata: peer.clone(),
                    addrs: HashSet::new(),
                    auth: known.1.auth,
                };
                candidate.addrs.insert(peer.addr);
                self.discovered_peers.insert(id.clone(), candidate.clone());
                self.known_peers.insert(id, candidate.clone());
                debug!("discovered peer is recorded");
                if self
                    .app_channel
                    .send(P2pEvent::PeerDiscovered(candidate.metadata))
                    .is_err()
                {
                    error!("failed to send PeerDiscovered event to the application");
                };
            }
        }
    }

    /// event loop calls this to inform manager a peer requested our precesence
    pub(crate) fn handle_presence_request(&self) {
        if let Err(e) = self
            .discovery_channel
            .send(DiscoveryEvent::PresenceResponse(self.metadata.clone()))
        {
            error!("event loop is unable to emit presence: {}", e);
        } else {
            debug!("peer is emitting presence response");
        }
    }

    /// event loop calls this to inform manager a peer is now connected
    pub(crate) fn handle_new_connection(&self, peer: Peer) {
        let id = peer.id.clone();
        self.connected_peers.insert(id);
        if self
            .app_channel
            .send(P2pEvent::PeerConnected(peer))
            .is_err()
        {
            error!("failed to send PeerConnected event to the application");
        };
    }
    // [ END ] Crate methods the event loop can call
}
