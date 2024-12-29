//! Consensus p2p network.
//!
//! This network is used by workers and primaries to reliably send consensus messages.

use crate::{
    codec::{TNCodec, TNMessage},
    error::NetworkError,
    types::{NetworkCommand, NetworkEvent, NetworkHandle, NetworkResult, SwarmCommand},
};
use futures::StreamExt as _;
use libp2p::{
    gossipsub::{self, IdentTopic, MessageAcceptance},
    multiaddr::Protocol,
    request_response::{self, Codec, OutboundRequestId, ProtocolSupport},
    swarm::{NetworkBehaviour, SwarmEvent},
    PeerId, StreamProtocol, Swarm, SwarmBuilder,
};
use std::{
    collections::{hash_map, HashMap, HashSet},
    time::Duration,
};
use tn_config::ConsensusConfig;
use tokio::{
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
    task::JoinHandle,
};
use tracing::{error, info, trace};

/// Custom network libp2p behaviour type for Telcoin Network.
///
/// The behavior includes gossipsub, request-response, and identify.
/// TODO: possibly KAD?
#[derive(NetworkBehaviour)]
pub struct TNBehavior<C>
where
    C: Codec + Send + Clone + 'static,
{
    /// The gossipsub network behavior.
    pub(crate) gossipsub: gossipsub::Behaviour,
    /// The request-response network behavior.
    pub(crate) req_res: request_response::Behaviour<C>,
}

impl<C> TNBehavior<C>
where
    C: Codec + Send + Clone + 'static,
{
    /// Create a new instance of Self.
    pub fn new(gossipsub: gossipsub::Behaviour, req_res: request_response::Behaviour<C>) -> Self {
        Self { gossipsub, req_res }
    }
}

/// The network type for consensus messages.
///
/// The primary and workers use separate instances of this network to reliably send messages to
/// other peers within the committee. The isolation of these networks is intended to:
/// - prevent a surge in one network message type from overwhelming all network traffic
/// - provide more granular control over resource allocation
/// - allow specific network configurations based on worker/primary needs
///
/// TODO: Primaries gossip signatures of final execution state at epoch boundaries and workers
/// gossip transactions? Publishers usually broadcast to several peers, so this may not be efficient
/// (multiple txs submitted).
pub struct ConsensusNetwork<Req, Res>
where
    Req: TNMessage,
    Res: TNMessage,
{
    /// The gossip network for flood publishing sealed worker blocks.
    swarm: Swarm<TNBehavior<TNCodec<Req, Res>>>,
    /// The subscribed gossip network topics.
    topics: Vec<IdentTopic>,
    /// The stream for forwarding network events.
    event_stream: Sender<NetworkEvent<Req, Res>>,
    /// The sender for network handles.
    handle: Sender<NetworkCommand<Req, Res>>,
    /// The receiver for processing network handle requests.
    commands: Receiver<NetworkCommand<Req, Res>>,
    /// The collection of staked validators.
    ///
    /// This set must be updated at the start of each epoch. It is used to verify message sources
    /// are from validators.
    authorized_publishers: HashSet<PeerId>,
    /// The collection of pending dials.
    pending_dials: HashMap<PeerId, oneshot::Sender<NetworkResult<()>>>,
    /// The collection of pending requests.
    ///
    /// Callers include a oneshot channel for the network to return response. The caller is responsible for decoding message bytes and reporting peers who return bad data. Peers that send messages that fail to decode must receive an application score penalty.
    pending_requests: HashMap<OutboundRequestId, oneshot::Sender<Res>>,
}

impl<Req, Res> ConsensusNetwork<Req, Res>
where
    Req: TNMessage,
    Res: TNMessage,
{
    /// Create a new instance of Self.
    ///
    /// TODO: add NetworkResult errors before merge - using `expect` for quicker refactors
    /// !!!~~~~~~~k
    pub fn new<DB>(
        config: &ConsensusConfig<DB>,
        event_stream: mpsc::Sender<NetworkEvent<Req, Res>>,
        authorized_publishers: HashSet<PeerId>,
        gossipsub_config: gossipsub::Config,
        topics: Vec<IdentTopic>,
    ) -> NetworkResult<Self>
    where
        // TODO: need to import tn-storage just for this trait?
        DB: tn_storage::traits::Database,
    {
        //
        //
        // TODO: pass keypair as arg so this function stays agnostic to primary/worker
        // - don't put helper method on key config bc that is TN-specific, and this is required by
        //   libp2p
        // - need to separate worker/primary network signatures
        let mut key_bytes = config.key_config().primary_network_keypair().as_ref().to_vec();
        let keypair = libp2p::identity::Keypair::ed25519_from_bytes(&mut key_bytes).expect("TODO");

        let gossipsub = gossipsub::Behaviour::new(
            gossipsub::MessageAuthenticity::Signed(keypair.clone()),
            gossipsub_config,
        )
        .expect("TODO");

        // ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
        //
        // revisit keypair approach

        // TODO: use const
        let tn_codec = TNCodec::<Req, Res>::new(1024 * 1024);
        // TODO: is StreamProtocol sufficient?
        // - ProtocolSupport::Full?
        let protocols = [(StreamProtocol::new("/tn-consensus"), ProtocolSupport::Full)];
        let req_res = request_response::Behaviour::with_codec(
            tn_codec,
            protocols,
            request_response::Config::default(),
        );
        let behavior = TNBehavior::new(gossipsub, req_res);

        // create swarm
        let swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_quic()
            .with_behaviour(|_| behavior)
            .expect("TODO")
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
            .build();

        let (handle, commands) = tokio::sync::mpsc::channel(100);
        Ok(Self {
            swarm,
            topics,
            handle,
            commands,
            event_stream,
            authorized_publishers,
            pending_dials: Default::default(),
            pending_requests: Default::default(),
        })
    }

    /// Return a [NetworkHandle] to send commands to this network.
    ///
    /// TODO: this should just be `NetworkHandle`
    pub fn network_handle(&self) -> NetworkHandle<Req, Res> {
        NetworkHandle::new(self.handle.clone())
    }

    /// Run the network loop to process incoming gossip.
    pub fn run(mut self) -> JoinHandle<NetworkResult<()>> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    event = self.swarm.select_next_some() => self.process_event(event).await?,
                    command = self.commands.recv() => match command {
                        Some(c) => self.process_command(c),
                        None => {
                            info!(target: "consensus-network", topics=?self.topics, "subscriber shutting down...");
                            return Ok(())
                        }
                    }
                }
            }
        })
    }

    /// Process commands for the swarm.
    fn process_command(&mut self, command: NetworkCommand<Req, Res>) {
        match command {
            NetworkCommand::UpdateAuthorizedPublishers { authorities, reply } => {
                self.authorized_publishers = authorities;
                let _ = reply.send(Ok(()));
            }
            NetworkCommand::Swarm(c) => self.process_swarm_command(c),
        }
    }

    /// Process commands for the swarm.
    fn process_swarm_command(&mut self, command: SwarmCommand<Req, Res>) {
        match command {
            SwarmCommand::StartListening { multiaddr, reply } => {
                let res = self.swarm.listen_on(multiaddr);
                if let Err(e) = reply.send(res) {
                    error!(target: "swarm-command", ?e, "StartListening failed to send result");
                }
            }
            SwarmCommand::GetListener { reply } => {
                let addrs = self.swarm.listeners().cloned().collect();
                if let Err(e) = reply.send(addrs) {
                    error!(target: "gossip-network", ?e, "GetListeners command failed");
                }
            }
            SwarmCommand::AddExplicitPeer { peer_id, addr } => {
                self.swarm.add_peer_address(peer_id, addr);
                self.swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
            }
            SwarmCommand::Dial { peer_id, peer_addr, reply } => {
                if let hash_map::Entry::Vacant(entry) = self.pending_dials.entry(peer_id) {
                    // TODO: support kademlia?
                    //
                    // self.swarm
                    //     .behaviour_mut()
                    //     .kademlia
                    //     .add_address(&peer_id, peer_addr.clone());
                    match self.swarm.dial(peer_addr.with(Protocol::P2p(peer_id))) {
                        Ok(()) => {
                            entry.insert(reply);
                        }
                        Err(e) => {
                            if let Err(e) = reply.send(Err(e.into())) {
                                error!(target: "network", ?e, "AddExplicitPeer oneshot dropped");
                            }
                        }
                    }
                } else {
                    todo!("Already dialed peer.");
                }
            }
            SwarmCommand::LocalPeerId { reply } => {
                let peer_id = *self.swarm.local_peer_id();
                if let Err(e) = reply.send(peer_id) {
                    error!(target: "gossip-network", ?e, "LocalPeerId command failed");
                }
            }
            SwarmCommand::Publish { topic, msg, reply } => {
                let res = self.swarm.behaviour_mut().gossipsub.publish(topic, msg);
                if let Err(e) = reply.send(res) {
                    error!(target: "gossip-network", ?e, "Publish command failed");
                }
            }
            SwarmCommand::Subscribe { topic, reply } => {
                let res = self.swarm.behaviour_mut().gossipsub.subscribe(&topic);
                if let Err(e) = reply.send(res) {
                    error!(target: "gossip-network", ?e, "Subscribe command failed");
                }
            }
            SwarmCommand::ConnectedPeers { reply } => {
                let res = self.swarm.connected_peers().cloned().collect();
                if let Err(e) = reply.send(res) {
                    error!(target: "gossip-network", ?e, "ConnectedPeers command failed");
                }
            }
            SwarmCommand::PeerScore { peer_id, reply } => {
                let opt_score = self.swarm.behaviour_mut().gossipsub.peer_score(&peer_id);
                if let Err(e) = reply.send(opt_score) {
                    error!(target: "gossip-network", ?e, "PeerScore command failed");
                }
            }
            SwarmCommand::SetApplicationScore { peer_id, new_score, reply } => {
                let bool =
                    self.swarm.behaviour_mut().gossipsub.set_application_score(&peer_id, new_score);
                if let Err(e) = reply.send(bool) {
                    error!(target: "gossip-network", ?e, "SetApplicationScore command failed");
                }
            }
            SwarmCommand::AllPeers { reply } => {
                let collection = self
                    .swarm
                    .behaviour_mut()
                    .gossipsub
                    .all_peers()
                    .map(|(peer_id, vec)| (*peer_id, vec.into_iter().cloned().collect()))
                    .collect();

                if let Err(e) = reply.send(collection) {
                    error!(target: "gossip-network", ?e, "AllPeers command failed");
                }
            }
            SwarmCommand::AllMeshPeers { reply } => {
                let collection =
                    self.swarm.behaviour_mut().gossipsub.all_mesh_peers().cloned().collect();
                if let Err(e) = reply.send(collection) {
                    error!(target: "gossip-network", ?e, "AllMeshPeers command failed");
                }
            }
            SwarmCommand::MeshPeers { topic, reply } => {
                let collection =
                    self.swarm.behaviour_mut().gossipsub.mesh_peers(&topic).cloned().collect();
                if let Err(e) = reply.send(collection) {
                    error!(target: "gossip-network", ?e, "MeshPeers command failed");
                }
            }
            SwarmCommand::SendRequest { peer, request, reply } => {
                tracing::debug!("inside SwarmCommand send request");
                let request_id = self.swarm.behaviour_mut().req_res.send_request(&peer, request);
                self.pending_requests.insert(request_id, reply);
            }
            SwarmCommand::SendResponse { response, channel, reply } => {
                let res = self.swarm.behaviour_mut().req_res.send_response(channel, response);
                if let Err(e) = reply.send(res) {
                    error!(target: "network", ?e, "MeshPeers command failed");
                }
            }
        }
    }

    /// Process events from the swarm.
    async fn process_event(
        &mut self,
        event: SwarmEvent<TNBehaviorEvent<TNCodec<Req, Res>>>,
    ) -> NetworkResult<()> {
        match event {
            SwarmEvent::Behaviour(behavior) => match behavior {
                TNBehaviorEvent::Gossipsub(gossip) => match gossip {
                    gossipsub::Event::Message { propagation_source, message_id, message } => {
                        trace!(target: "consensus-network", topic=?self.topics, ?propagation_source, ?message_id, ?message, "message received from publisher");
                        // verify message was published by authorized node
                        let msg_acceptance = if message
                            .source
                            .is_some_and(|id| self.authorized_publishers.contains(&id))
                        {
                            // forward message to handler
                            if let Err(e) =
                                self.event_stream.try_send(NetworkEvent::Gossip(message.data))
                            {
                                error!(target: "consensus-network", topics=?self.topics, ?propagation_source, ?message_id, ?e, "failed to forward gossip!");
                                // fatal - unable to process gossip messages
                                return Err(e.into());
                            }

                            MessageAcceptance::Accept
                        } else {
                            MessageAcceptance::Reject
                        };

                        // report message validation results
                        if let Err(e) =
                            self.swarm.behaviour_mut().gossipsub.report_message_validation_result(
                                &message_id,
                                &propagation_source,
                                msg_acceptance,
                            )
                        {
                            error!(target: "consensus-network", topics=?self.topics, ?propagation_source, ?message_id, ?e, "error reporting message validation result");
                        }
                    }
                    gossipsub::Event::Subscribed { peer_id, topic } => {
                        trace!(target: "consensus-network", topics=?self.topics, ?peer_id, ?topic, "gossipsub event - subscribed")
                    }
                    gossipsub::Event::Unsubscribed { peer_id, topic } => {
                        trace!(target: "consensus-network", topics=?self.topics, ?peer_id, ?topic, "gossipsub event - unsubscribed")
                    }
                    gossipsub::Event::GossipsubNotSupported { peer_id } => {
                        // TODO: remove peer at self point?
                        trace!(target: "consensus-network", topics=?self.topics, ?peer_id, "gossipsub event - not supported")
                    }
                },
                TNBehaviorEvent::ReqRes(rpc) => match rpc {
                    request_response::Event::Message { peer, message } => {
                        info!(target: "consensus-network",  ?peer, ?message, "req/res MESSAGE event");

                        match message {
                            request_response::Message::Request { request_id, request, channel } => {
                                // forward request to handler without blocking other events
                                if let Err(e) = self
                                    .event_stream
                                    .try_send(NetworkEvent::Request { request, channel })
                                {
                                    error!(target: "consensus-network", topics=?self.topics, ?request_id, ?e, "failed to forward request!");
                                    // fatal - unable to process requests
                                    return Err(e.into());
                                }
                            }
                            request_response::Message::Response { request_id, response } => {
                                // forward response to original caller
                                if let Err(e) = self
                                    .pending_requests
                                    .remove(&request_id)
                                    .ok_or(NetworkError::RequestChannelLost)?
                                    .send(response)
                                {
                                    error!(target: "consensus-network", topics=?self.topics, ?request_id, ?e, "failed to forward request!");
                                    // fatal - unable to process requests
                                    return Err(NetworkError::RequestChannelLost);
                                }
                            }
                        }
                    }
                    request_response::Event::OutboundFailure { peer, request_id, error } => {
                        println!(
                            "outbound failure?? - {:?} - {:?} - {:?}",
                            peer, request_id, error
                        );
                    }
                    request_response::Event::InboundFailure { peer, request_id, error } => {
                        println!("inbound failure?? - {:?} - {:?} - {:?}", peer, request_id, error);
                    }
                    request_response::Event::ResponseSent { peer, request_id } => {
                        info!(target: "consensus-network",  ?peer, ?request_id, "req/res RESPONSE_SENT event")
                    }
                },
            },
            SwarmEvent::ConnectionEstablished {
                peer_id,
                connection_id,
                endpoint,
                num_established,
                concurrent_dial_errors,
                established_in,
            } => {
                trace!(target: "consensus-network", topics=?self.topics, ?peer_id, ?connection_id, ?endpoint, ?num_established, ?concurrent_dial_errors, ?established_in, "connection established");
                if endpoint.is_dialer() {
                    if let Some(sender) = self.pending_dials.remove(&peer_id) {
                        if let Err(e) = sender.send(Ok(())) {
                            error!(target: "network", ?e, "failed to report dial success - oneshot dropped");
                        }
                    }
                }
            }
            SwarmEvent::ConnectionClosed {
                peer_id,
                connection_id,
                endpoint,
                num_established,
                cause,
            } => trace!(
                target: "consensus-network",
                topics=?self.topics,
                ?peer_id,
                ?connection_id,
                ?endpoint,
                ?num_established,
                ?cause,
                "connection closed"
            ),
            SwarmEvent::IncomingConnection { connection_id, local_addr, send_back_addr } => {
                trace!(target: "consensus-network", topics=?self.topics, ?connection_id, ?local_addr, ?send_back_addr, "incoming connection")
            }
            SwarmEvent::IncomingConnectionError {
                connection_id,
                local_addr,
                send_back_addr,
                error,
            } => trace!(
                target: "consensus-network",
                topics=?self.topics,
                ?connection_id,
                ?local_addr,
                ?send_back_addr,
                ?error,
                "incoming connection error"
            ),
            SwarmEvent::OutgoingConnectionError { connection_id, peer_id, error } => {
                trace!(target: "consensus-network", topics=?self.topics, ?connection_id, ?peer_id, ?error, "outgoing connection error");
                if let Some(peer_id) = peer_id {
                    if let Some(sender) = self.pending_dials.remove(&peer_id) {
                        if let Err(e) = sender.send(Err(error.into())) {
                            error!(target: "network", ?e, "failed to report dial failure - oneshot dropped");
                        }
                    }
                }
            }
            SwarmEvent::NewListenAddr { listener_id, address } => {
                trace!(target: "consensus-network", topics=?self.topics, ?listener_id, ?address, "new listener addr")
            }
            SwarmEvent::ExpiredListenAddr { listener_id, address } => {
                trace!(target: "consensus-network", topics=?self.topics, ?listener_id, ?address, "expired listen addr")
            }
            SwarmEvent::ListenerClosed { listener_id, addresses, reason } => {
                trace!(target: "consensus-network", topics=?self.topics, ?listener_id, ?addresses, ?reason, "listener closed")
            }
            SwarmEvent::ListenerError { listener_id, error } => {
                trace!(target: "consensus-network", topics=?self.topics, ?listener_id, ?error, "listener error")
            }
            SwarmEvent::Dialing { peer_id, connection_id } => {
                trace!(target: "consensus-network", topics=?self.topics, ? peer_id, ?connection_id, "dialing")
            }
            SwarmEvent::NewExternalAddrCandidate { address } => {
                trace!(target: "consensus-network", topics=?self.topics, ?address, "new external addr candidate")
            }
            SwarmEvent::ExternalAddrConfirmed { address } => {
                trace!(target: "consensus-network", topics=?self.topics, ?address, "external addr confirmed")
            }
            SwarmEvent::ExternalAddrExpired { address } => {
                trace!(target: "consensus-network", topics=?self.topics, ?address, "external addr expired")
            }
            SwarmEvent::NewExternalAddrOfPeer { peer_id, address } => {
                trace!(target: "consensus-network", topics=?self.topics, ?peer_id, ?address, "new external addr of peer")
            }
            _e => {
                trace!(target: "consensus-network", topics=?self.topics, ?_e, "non-exhaustive event match")
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::_primary_gossip_config;
    use libp2p::Multiaddr;
    use tn_storage::mem_db::MemDatabase;
    use tn_test_utils::{fixture_batch_with_transactions, CommitteeFixture};
    use tn_types::WorkerBlock;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_worker_network() -> eyre::Result<()> {
        tn_test_utils::init_test_tracing();
        // TODO: reload known peers from database,
        // - use this file on startup for "discoverability" at genesis

        let all_nodes = CommitteeFixture::builder(MemDatabase::default).build();

        //
        //=== peer 1
        //

        let mut authorities = all_nodes.authorities();
        let authority_1 = authorities.next().expect("first authority");
        let authority_2 = authorities.next().expect("second authority");
        let config_1 = authority_1.consensus_config();
        let config_2 = authority_2.consensus_config();
        let (tx1, mut network_messages1) = mpsc::channel(1);
        let (tx2, mut network_messages2) = mpsc::channel(1);
        let authorized_publishers: HashSet<PeerId> = all_nodes
            .authorities()
            .map(|a| {
                let mut key_bytes = a.primary_network_keypair().as_ref().to_vec();
                let keypair = libp2p::identity::Keypair::ed25519_from_bytes(&mut key_bytes)
                    .expect("primary ed25519 key from bytes");
                let public_key = keypair.public();

                PeerId::from_public_key(&public_key)
            })
            .collect();

        println!("authorized publishers: {:?}", authorized_publishers);
        let gossipsub_config = _primary_gossip_config()?;
        let topics = vec![IdentTopic::new("test-topic")];
        // peer1
        let peer1_network = ConsensusNetwork::<WorkerBlock, WorkerBlock>::new(
            &config_1,
            tx1,
            authorized_publishers.clone(),
            gossipsub_config.clone(),
            topics.clone(),
        )?;

        // peer2
        let peer2_network = ConsensusNetwork::<WorkerBlock, WorkerBlock>::new(
            &config_2,
            tx2,
            authorized_publishers.clone(),
            gossipsub_config.clone(),
            topics.clone(),
        )?;

        // spawn tasks
        let peer1 = peer1_network.network_handle();
        peer1_network.run();

        let peer2 = peer2_network.network_handle();
        peer2_network.run();

        // start swarm listening on default any address
        let listen_on: Multiaddr = "/ip4/127.0.0.1/udp/0/quic-v1".parse()?;
        peer1.start_listening(listen_on.clone()).await?;
        peer2.start_listening(listen_on).await?;
        let peer1_id = peer1.local_peer_id().await?;
        let peer2_id = peer2.local_peer_id().await?;
        let peer2_addr = peer2.listeners().await?.first().expect("peer2 listen addr").clone();

        let worker_block_req = fixture_batch_with_transactions(3);
        let worker_block_res = fixture_batch_with_transactions(3);

        // sanity check
        assert_ne!(worker_block_req, worker_block_res);

        // let mut other_peers =
        //     authorized_publishers.iter().filter(|id| *id != &peer1_id && *id != &peer2_id);
        // let peer2 = other_peers.next().ok_or_eyre("committee must have more than 1 peer")?;

        // dial peer2
        peer1.dial(peer2_id, peer2_addr).await?;
        // peer2.dial(peer1_addr.into()).await?;

        // allow enough time for peer info to exchange from dial
        //
        // sleep seems to be the only thing that works here

        println!("\n\n\nmade it here!!!!");
        let network_res = peer1.send_request(worker_block_req.clone(), peer2_id).await?;

        let dur = Duration::from_secs(5);
        let event =
            timeout(dur, network_messages2.recv()).await?.expect("first network event received");

        if let NetworkEvent::Request { request, channel } = event {
            assert_eq!(request, worker_block_req);
            // send response
            peer1.send_response(worker_block_res.clone(), channel).await?;
        } else {
            panic!("wrong network event received");
        }

        let response = timeout(dur, network_res).await?.expect("outbound id recv");

        assert_eq!(response, worker_block_res);

        Ok(())
    }
}
