// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::{
    bootstrapping_peer::BootstrappingPeer,
    common::{Approved, Base, GOSSIP_TIMEOUT},
    elder::{Elder, ElderDetails},
};
use crate::{
    chain::{
        Chain, EldersChange, EldersInfo, GenesisPfxInfo, NetworkParams, OnlinePayload,
        SectionKeyInfo, SendAckMessagePayload,
    },
    error::{BootstrapResponseError, RoutingError},
    event::Event,
    id::{FullId, P2pNode, PublicId},
    messages::{
        BootstrapResponse, DirectMessage, HopMessage, RelocateDetails, RoutingMessage,
        SignedRoutingMessage,
    },
    outbox::EventBox,
    parsec::{DkgResultWrapper, ParsecMap},
    peer_map::PeerMap,
    routing_message_filter::RoutingMessageFilter,
    routing_table::{Authority, Prefix},
    state_machine::{State, Transition},
    time::Duration,
    timer::Timer,
    xor_name::XorName,
    NetworkService,
};
use itertools::Itertools;
use std::{
    collections::BTreeSet,
    fmt::{self, Display, Formatter},
    mem,
};

const POKE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct AdultDetails {
    pub network_service: NetworkService,
    pub event_backlog: Vec<Event>,
    pub full_id: FullId,
    pub gen_pfx_info: GenesisPfxInfo,
    pub routing_msg_backlog: Vec<SignedRoutingMessage>,
    pub direct_msg_backlog: Vec<(P2pNode, DirectMessage)>,
    pub peer_map: PeerMap,
    pub routing_msg_filter: RoutingMessageFilter,
    pub timer: Timer,
    pub network_cfg: NetworkParams,
}

pub struct Adult {
    chain: Chain,
    network_service: NetworkService,
    event_backlog: Vec<Event>,
    full_id: FullId,
    gen_pfx_info: GenesisPfxInfo,
    /// Routing messages addressed to us that we cannot handle until we are established.
    routing_msg_backlog: Vec<SignedRoutingMessage>,
    direct_msg_backlog: Vec<(P2pNode, DirectMessage)>,
    parsec_map: ParsecMap,
    peer_map: PeerMap,
    parsec_timer_token: u64,
    routing_msg_filter: RoutingMessageFilter,
    timer: Timer,
}

impl Adult {
    pub fn from_joining_peer(
        details: AdultDetails,
        _outbox: &mut dyn EventBox,
    ) -> Result<Self, RoutingError> {
        let public_id = *details.full_id.public_id();
        let parsec_timer_token = details.timer.schedule(POKE_TIMEOUT);

        let parsec_map = ParsecMap::new(details.full_id.clone(), &details.gen_pfx_info);

        let chain = Chain::new(details.network_cfg, public_id, details.gen_pfx_info.clone());

        let node = Self {
            chain,
            network_service: details.network_service,
            event_backlog: details.event_backlog,
            full_id: details.full_id,
            gen_pfx_info: details.gen_pfx_info,
            routing_msg_backlog: details.routing_msg_backlog,
            direct_msg_backlog: details.direct_msg_backlog,
            parsec_map,
            peer_map: details.peer_map,
            routing_msg_filter: details.routing_msg_filter,
            timer: details.timer,
            parsec_timer_token,
        };

        Ok(node)
    }

    pub fn rebootstrap(self) -> Result<State, RoutingError> {
        let network_cfg = self.chain.network_cfg();

        // Try to join the same section, but using new id, otherwise the section won't accept us
        // due to duplicate votes.
        let full_id = FullId::within_range(&self.chain.our_prefix().range_inclusive());

        Ok(State::BootstrappingPeer(BootstrappingPeer::new(
            self.network_service,
            full_id,
            network_cfg,
            self.timer,
        )))
    }

    pub fn into_elder(
        self,
        elders_info: EldersInfo,
        old_pfx: Prefix<XorName>,
        outbox: &mut dyn EventBox,
    ) -> Result<State, RoutingError> {
        let details = ElderDetails {
            chain: self.chain,
            network_service: self.network_service,
            event_backlog: self.event_backlog,
            full_id: self.full_id,
            gen_pfx_info: self.gen_pfx_info,
            msg_queue: Default::default(),
            routing_msg_backlog: self.routing_msg_backlog,
            direct_msg_backlog: self.direct_msg_backlog,
            parsec_map: self.parsec_map,
            peer_map: self.peer_map,
            // we reset the message filter so that the node can correctly process some messages as
            // an Elder even if it has already seen them as an Adult
            routing_msg_filter: RoutingMessageFilter::new(),
            timer: self.timer,
        };

        Elder::from_adult(details, elders_info, old_pfx, outbox).map(State::Elder)
    }

    fn dispatch_routing_message(
        &mut self,
        msg: SignedRoutingMessage,
        outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        use crate::{messages::MessageContent::*, routing_table::Authority::*};

        let (msg, metadata) = msg.into_parts();
        // let src_name = msg.src.name();

        match msg {
            RoutingMessage {
                content:
                    ConnectionRequest {
                        conn_info,
                        pub_id,
                        msg_id,
                    },
                src: Node(_),
                dst: Node(_),
            } => {
                if self.chain.our_prefix().matches(&msg.src.name()) {
                    self.handle_connection_request(conn_info, pub_id, msg.src, msg.dst, outbox)
                } else {
                    self.add_message_to_backlog(SignedRoutingMessage::from_parts(
                        RoutingMessage {
                            content: ConnectionRequest {
                                conn_info,
                                pub_id,
                                msg_id,
                            },
                            ..msg
                        },
                        metadata,
                    ));
                    Ok(())
                }
            }
            _ => {
                self.add_message_to_backlog(SignedRoutingMessage::from_parts(msg, metadata));
                Ok(())
            }
        }
    }

    // Sends a `ParsecPoke` message to trigger a gossip request from current section members to us.
    //
    // TODO: Should restrict targets to few(counter churn-threshold)/single.
    // Currently this can result in incoming spam of gossip history from everyone.
    // Can also just be a single target once node-ageing makes Offline votes Opaque which should
    // remove invalid test failures for unaccumulated parsec::Remove blocks.
    fn send_parsec_poke(&mut self) {
        let version = *self.gen_pfx_info.first_info.version();
        let recipients = self
            .gen_pfx_info
            .latest_info
            .member_nodes()
            .cloned()
            .collect_vec();

        for recipient in recipients {
            self.send_direct_message(&recipient, DirectMessage::ParsecPoke(version));
        }
    }

    // Backlog the message to be processed once we are established.
    fn add_message_to_backlog(&mut self, msg: SignedRoutingMessage) {
        trace!(
            "{} Not elder yet. Delaying message handling: {:?}",
            self,
            msg
        );
        self.routing_msg_backlog.push(msg)
    }

    // Reject the bootstrap request, because only Elders can handle it.
    fn handle_bootstrap_request(&mut self, p2p_node: P2pNode, _destination: XorName) {
        debug!(
            "{} - Joining node {:?} rejected: We are not an established node yet.",
            self, p2p_node,
        );

        self.send_direct_message(
            &p2p_node,
            DirectMessage::BootstrapResponse(BootstrapResponse::Error(
                BootstrapResponseError::NotApproved,
            )),
        );
        self.disconnect(p2p_node.public_id());
    }
}

#[cfg(feature = "mock_base")]
impl Adult {
    pub fn chain(&self) -> &Chain {
        &self.chain
    }

    pub fn get_timed_out_tokens(&mut self) -> Vec<u64> {
        self.timer.get_timed_out_tokens()
    }

    pub fn has_unpolled_observations(&self) -> bool {
        self.parsec_map.has_unpolled_observations()
    }
}

impl Base for Adult {
    fn network_service(&self) -> &NetworkService {
        &self.network_service
    }

    fn network_service_mut(&mut self) -> &mut NetworkService {
        &mut self.network_service
    }

    fn full_id(&self) -> &FullId {
        &self.full_id
    }

    fn in_authority(&self, auth: &Authority<XorName>) -> bool {
        self.chain.in_authority(auth)
    }

    fn peer_map(&self) -> &PeerMap {
        &self.peer_map
    }

    fn peer_map_mut(&mut self) -> &mut PeerMap {
        &mut self.peer_map
    }

    fn timer(&mut self) -> &mut Timer {
        &mut self.timer
    }

    fn finish_handle_transition(&mut self, outbox: &mut dyn EventBox) -> Transition {
        debug!("{} - State changed to Adult finished.", self);

        for msg in mem::replace(&mut self.routing_msg_backlog, Default::default()) {
            if let Err(err) = self.dispatch_routing_message(msg, outbox) {
                debug!("{} - {:?}", self, err);
            }
        }

        let mut transition = Transition::Stay;
        for (pub_id, msg) in mem::replace(&mut self.direct_msg_backlog, Default::default()) {
            if let Transition::Stay = &transition {
                match self.handle_direct_message(msg, pub_id, outbox) {
                    Ok(new_transition) => transition = new_transition,
                    Err(err) => debug!("{} - {:?}", self, err),
                }
            } else {
                self.direct_msg_backlog.push((pub_id, msg));
            }
        }

        transition
    }

    fn handle_timeout(&mut self, token: u64, _: &mut dyn EventBox) -> Transition {
        if self.parsec_timer_token == token {
            if self.chain.is_peer_our_elder(self.id()) {
                self.send_parsec_gossip(None);
                self.parsec_timer_token = self.timer.schedule(GOSSIP_TIMEOUT);
            } else {
                self.send_parsec_poke();
                self.parsec_timer_token = self.timer.schedule(POKE_TIMEOUT);
            }
        }

        Transition::Stay
    }

    fn handle_peer_lost(&mut self, pub_id: PublicId, _: &mut dyn EventBox) -> Transition {
        debug!("{} - Lost peer {}", self, pub_id);
        Transition::Stay
    }

    fn handle_direct_message(
        &mut self,
        msg: DirectMessage,
        p2p_node: P2pNode,
        outbox: &mut dyn EventBox,
    ) -> Result<Transition, RoutingError> {
        use crate::messages::DirectMessage::*;
        match msg {
            ParsecRequest(version, par_request) => {
                self.handle_parsec_request(version, par_request, p2p_node, outbox)
            }
            ParsecResponse(version, par_response) => {
                self.handle_parsec_response(version, par_response, *p2p_node.public_id(), outbox)
            }
            BootstrapRequest(name) => {
                self.handle_bootstrap_request(p2p_node, name);
                Ok(Transition::Stay)
            }
            ConnectionResponse => {
                debug!("{} - Received connection response from {}", self, p2p_node);
                Ok(Transition::Stay)
            }
            msg => {
                debug!(
                    "{} Unhandled direct message, adding to backlog: {:?}",
                    self, msg
                );
                self.direct_msg_backlog.push((p2p_node, msg));
                Ok(Transition::Stay)
            }
        }
    }

    fn handle_hop_message(
        &mut self,
        msg: HopMessage,
        outbox: &mut dyn EventBox,
    ) -> Result<Transition, RoutingError> {
        let HopMessage { content: msg, .. } = msg;

        if !self
            .routing_msg_filter
            .filter_incoming(msg.routing_message())
            .is_new()
        {
            trace!(
                "{} Known message: {:?} - not handling further",
                self,
                msg.routing_message()
            );
            return Ok(Transition::Stay);
        }

        if self.in_authority(&msg.routing_message().dst) {
            self.check_signed_message_integrity(&msg)?;
            self.dispatch_routing_message(msg, outbox)?;
        } else {
            self.routing_msg_backlog.push(msg);
        }
        Ok(Transition::Stay)
    }

    fn send_routing_message(&mut self, routing_msg: RoutingMessage) -> Result<(), RoutingError> {
        if self.in_authority(&routing_msg.dst) {
            return Ok(()); // Message is for us.
        }

        let signed_msg = SignedRoutingMessage::single_source(routing_msg, self.full_id())?;

        // We should only be connected to our own Elders - send to all of them
        // Need to collect IDs first so that self is not borrowed via the iterator
        //
        // WIP: this is probably out of date? How else do we know which our section members are?
        let target_nodes = self
            .gen_pfx_info
            .latest_info
            .member_nodes()
            .cloned()
            .collect_vec();

        for p2p_node in target_nodes {
            if self
                .routing_msg_filter
                .filter_outgoing(signed_msg.routing_message(), p2p_node.public_id())
                .is_new()
            {
                let message = self.to_hop_message(signed_msg.clone())?;
                self.send_message(&p2p_node, message);
            }
        }

        Ok(())
    }
}

impl Approved for Adult {
    fn send_event(&mut self, event: Event, _: &mut dyn EventBox) {
        self.event_backlog.push(event)
    }

    fn parsec_map(&self) -> &ParsecMap {
        &self.parsec_map
    }

    fn parsec_map_mut(&mut self) -> &mut ParsecMap {
        &mut self.parsec_map
    }

    fn chain_mut(&mut self) -> &mut Chain {
        &mut self.chain
    }

    fn set_pfx_successfully_polled(&mut self, _: bool) {
        // Doesn't do anything
    }

    fn is_pfx_successfully_polled(&self) -> bool {
        false
    }

    fn handle_online_event(
        &mut self,
        payload: OnlinePayload,
        outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        let pub_id = *payload.p2p_node.public_id();

        if !self.chain.can_add_member(&payload.p2p_node) {
            info!("{} - ignore Online: {:?}.", self, payload);
            return Ok(());
        }

        info!("{} - handle Online: {:?}.", self, payload);
        self.chain.add_member(payload.p2p_node, payload.age);

        // Simulate handling AddElder as well
        info!("{} - handle AddElder: {}.", self, pub_id);
        let _ = self.chain.add_elder(pub_id)?;
        self.send_event(Event::NodeAdded(*pub_id.name()), outbox);

        // If the elder being added is us, start sending parsec gossips.
        if pub_id == *self.id() {
            self.parsec_timer_token = self.timer.schedule(GOSSIP_TIMEOUT);
        }

        Ok(())
    }

    fn handle_offline_event(
        &mut self,
        pub_id: PublicId,
        outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        info!("{} - handle Offline: {}.", self, pub_id);
        self.chain.remove_member(&pub_id);

        info!("{} - handle RemoveElder: {}.", self, pub_id);
        let _ = self.chain.remove_elder(pub_id)?;
        self.disconnect(&pub_id);
        self.send_event(Event::NodeLost(*pub_id.name()), outbox);
        Ok(())
    }

    fn handle_dkg_result_event(
        &mut self,
        _participants: &BTreeSet<PublicId>,
        _dkg_result: &DkgResultWrapper,
    ) -> Result<(), RoutingError> {
        // TODO
        Ok(())
    }

    fn handle_section_info_event(
        &mut self,
        elders_info: EldersInfo,
        old_pfx: Prefix<XorName>,
        _neighbour_change: EldersChange,
        _: &mut dyn EventBox,
    ) -> Result<Transition, RoutingError> {
        if self.chain.is_self_elder() {
            Ok(Transition::IntoElder {
                elders_info,
                old_pfx,
            })
        } else {
            debug!("{} - Unhandled SectionInfo event", self);
            Ok(Transition::Stay)
        }
    }

    fn handle_relocate_event(
        &mut self,
        details: RelocateDetails,
        _: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        info!("{} - handle Relocate: {:?}.", self, details);

        if !self.chain.our_prefix().matches(&details.destination) {
            self.chain.remove_member(&details.pub_id);
        }

        Ok(())
    }

    fn handle_their_key_info_event(
        &mut self,
        _key_info: SectionKeyInfo,
    ) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_send_ack_message_event(
        &mut self,
        _ack_payload: SendAckMessagePayload,
    ) -> Result<(), RoutingError> {
        Ok(())
    }

    fn handle_our_merge_event(&mut self) -> Result<(), RoutingError> {
        debug!("{} - Unhandled OurMerge event", self);
        Ok(())
    }

    fn handle_neighbour_merge_event(&mut self) -> Result<(), RoutingError> {
        debug!("{} - Unhandled NeighbourMerge event", self);
        Ok(())
    }
}

impl Display for Adult {
    fn fmt(&self, formatter: &mut Formatter) -> fmt::Result {
        write!(
            formatter,
            "Adult({}({:b}))",
            self.name(),
            self.chain.our_prefix()
        )
    }
}
