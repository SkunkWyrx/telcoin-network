//! Constants and trait implementations for network compatibility.

use libp2p::{
    gossipsub::{IdentTopic, MessageId, PublishError, SubscriptionError, TopicHash},
    swarm::{dial_opts::DialOpts, DialError},
    Multiaddr, PeerId,
};
use std::collections::{HashMap, HashSet};
use tokio::sync::{mpsc, oneshot};

/// The topic for NVVs to subscribe to for published worker blocks.
pub const WORKER_BLOCK_TOPIC: &str = "tn_worker_blocks";
/// The topic for NVVs to subscribe to for published primary certificates.
pub const PRIMARY_CERT_TOPIC: &str = "tn_certificates";
/// The topic for NVVs to subscribe to for published consensus chain.
pub const CONSENSUS_HEADER_TOPIC: &str = "tn_consensus_headers";

/// Commands for the swarm.
#[derive(Debug)]
//TODO: add <M> generic here so devs can only publish correct messages?
pub enum NetworkCommand {
    /// Update the list of authorized publishers.
    ///
    /// This list is used to verify messages came from an authorized source.
    UpdateAuthorizedPublishers {
        /// The unique set of authorized peers.
        authorities: HashSet<PeerId>,
        /// The acknowledgement that the set was updated.
        reply: oneshot::Sender<eyre::Result<()>>,
    },
    /// Listeners
    GetListener { reply: oneshot::Sender<Vec<Multiaddr>> },
    /// Add explicit peer to add.
    ///
    /// This adds to the swarm's peers and the gossipsub's peers.
    AddExplicitPeer {
        /// The peer's id.
        peer_id: PeerId,
        /// The peer's address.
        addr: Multiaddr,
    },
    /// Dial a peer to establish a connection.
    Dial {
        /// The peer's address and peer id both impl Into<DialOpts>.
        ///
        /// However, it seems best to use the peer's [Multiaddr].
        dial_opts: DialOpts,
        /// Oneshot for reply
        reply: oneshot::Sender<std::result::Result<(), DialError>>,
    },
    /// Return an owned copy of this node's [PeerId].
    LocalPeerId { reply: oneshot::Sender<PeerId> },
    /// Subscribe to a topic.
    Subscribe {
        topic: IdentTopic,
        reply: oneshot::Sender<std::result::Result<bool, SubscriptionError>>,
    },
    /// Publish a message to topic subscribers.
    Publish {
        topic: IdentTopic,
        msg: Vec<u8>,
        reply: oneshot::Sender<std::result::Result<MessageId, PublishError>>,
    },
    /// Map of all known peers and their associated subscribed topics.
    AllPeers { reply: oneshot::Sender<HashMap<PeerId, Vec<TopicHash>>> },
    /// Collection of this node's connected peers.
    ConnectedPeers { reply: oneshot::Sender<Vec<PeerId>> },
    /// Collection of all mesh peers.
    AllMeshPeers { reply: oneshot::Sender<Vec<PeerId>> },
    /// Collection of all mesh peers by a certain topic hash.
    MeshPeers { topic: TopicHash, reply: oneshot::Sender<Vec<PeerId>> },
    /// The peer's score, if it exists.
    PeerScore { peer_id: PeerId, reply: oneshot::Sender<Option<f64>> },
    /// Set peer's application score.
    ///
    /// Peer's application score is P₅ of the peer scoring system.
    SetApplicationScore { peer_id: PeerId, new_score: f64, reply: oneshot::Sender<bool> },
}

/// Network handle.
///
/// The type that sends commands to the running network (swarm) task.
#[derive(Clone)]
pub struct GossipNetworkHandle {
    /// Sending channel to the network to process commands.
    sender: mpsc::Sender<NetworkCommand>,
}

impl GossipNetworkHandle {
    /// Create a new instance of Self.
    pub fn new(sender: mpsc::Sender<NetworkCommand>) -> Self {
        Self { sender }
    }

    /// Update the list of authorized publishers.
    pub async fn update_authorized_publishers(
        &self,
        authorities: HashSet<PeerId>,
    ) -> eyre::Result<()> {
        let (reply, ack) = oneshot::channel();
        self.sender.send(NetworkCommand::UpdateAuthorizedPublishers { authorities, reply }).await?;
        ack.await?
    }

    /// Request listeners from the swarm.
    pub async fn listeners(&self) -> eyre::Result<Vec<Multiaddr>> {
        let (reply, listeners) = oneshot::channel();
        self.sender.send(NetworkCommand::GetListener { reply }).await?;
        Ok(listeners.await?)
    }

    /// Add explicit peer.
    pub async fn add_explicit_peer(&self, peer_id: PeerId, addr: Multiaddr) -> eyre::Result<()> {
        self.sender.send(NetworkCommand::AddExplicitPeer { peer_id, addr }).await?;
        Ok(())
    }

    /// Dial a peer.
    pub async fn dial(&self, dial_opts: DialOpts) -> eyre::Result<()> {
        let (reply, ack) = oneshot::channel();
        self.sender.send(NetworkCommand::Dial { dial_opts, reply }).await?;
        let res = ack.await?;
        Ok(res?)
    }

    /// Get local peer id.
    pub async fn local_peer_id(&self) -> eyre::Result<PeerId> {
        let (reply, peer_id) = oneshot::channel();
        self.sender.send(NetworkCommand::LocalPeerId { reply }).await?;
        Ok(peer_id.await?)
    }

    /// Subscribe to a topic.
    pub async fn subscribe(&self, topic: IdentTopic) -> eyre::Result<bool> {
        let (reply, already_subscribed) = oneshot::channel();
        self.sender.send(NetworkCommand::Subscribe { topic, reply }).await?;
        let res = already_subscribed.await?;
        Ok(res?)
    }

    /// Publish a message on a certain topic.
    ///
    /// TODO: make this <M> generic to prevent accidental publishing of incorrect messages.
    pub async fn publish(&self, topic: IdentTopic, msg: Vec<u8>) -> eyre::Result<MessageId> {
        let (reply, published) = oneshot::channel();
        self.sender.send(NetworkCommand::Publish { topic, msg, reply }).await?;
        let res = published.await?;
        Ok(res?)
    }

    /// Retrieve a collection of connected peers.
    pub async fn connected_peers(&self) -> eyre::Result<Vec<PeerId>> {
        let (reply, peers) = oneshot::channel();
        self.sender.send(NetworkCommand::ConnectedPeers { reply }).await?;
        Ok(peers.await?)
    }

    /// Map of all known peers and their associated subscribed topics.
    pub async fn all_peers(&self) -> eyre::Result<HashMap<PeerId, Vec<TopicHash>>> {
        let (reply, all_peers) = oneshot::channel();
        self.sender.send(NetworkCommand::AllPeers { reply }).await?;
        Ok(all_peers.await?)
    }

    /// Collection of all mesh peers.
    pub async fn all_mesh_peers(&self) -> eyre::Result<Vec<PeerId>> {
        let (reply, all_mesh_peers) = oneshot::channel();
        self.sender.send(NetworkCommand::AllMeshPeers { reply }).await?;
        Ok(all_mesh_peers.await?)
    }

    /// Collection of all mesh peers by a certain topic hash.
    pub async fn mesh_peers(&self, topic: TopicHash) -> eyre::Result<Vec<PeerId>> {
        let (reply, mesh_peers) = oneshot::channel();
        self.sender.send(NetworkCommand::MeshPeers { topic, reply }).await?;
        Ok(mesh_peers.await?)
    }

    /// Retrieve a specific peer's score, if it exists.
    pub async fn peer_score(&self, peer_id: PeerId) -> eyre::Result<Option<f64>> {
        let (reply, score) = oneshot::channel();
        self.sender.send(NetworkCommand::PeerScore { peer_id, reply }).await?;
        Ok(score.await?)
    }

    /// Set the peer's application score.
    ///
    /// This is useful for reporting messages from a peer that fails decoding.
    pub async fn set_application_score(
        &self,
        peer_id: PeerId,
        new_score: f64,
    ) -> eyre::Result<bool> {
        let (reply, score) = oneshot::channel();
        self.sender.send(NetworkCommand::SetApplicationScore { peer_id, new_score, reply }).await?;
        Ok(score.await?)
    }
}
