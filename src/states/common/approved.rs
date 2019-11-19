// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::Base;
use crate::{
    chain::{
        AccumulatingEvent, Chain, EldersChange, EldersInfo, OnlinePayload, Proof, ProofSet,
        SectionKeyInfo, SendAckMessagePayload,
    },
    error::RoutingError,
    event::Event,
    id::{P2pNode, PublicId},
    messages::{DirectMessage, MessageContent, RoutingMessage},
    outbox::EventBox,
    parsec::{self, Block, DkgResultWrapper, Observation, ParsecMap},
    relocation::RelocateDetails,
    routing_table::{Authority, Prefix},
    state_machine::Transition,
    xor_name::XorName,
    BlsSignature, ConnectionInfo,
};
use log::LogLevel;
use rand::Rng;
use std::collections::BTreeSet;

/// Common functionality for node states post resource proof.
pub trait Approved: Base {
    fn parsec_map(&self) -> &ParsecMap;
    fn parsec_map_mut(&mut self) -> &mut ParsecMap;
    fn chain(&self) -> &Chain;
    fn chain_mut(&mut self) -> &mut Chain;
    fn send_event(&mut self, event: Event, outbox: &mut dyn EventBox);
    fn set_pfx_successfully_polled(&mut self, val: bool);
    fn is_pfx_successfully_polled(&self) -> bool;

    /// Handles an accumulated `Online` event.
    fn handle_online_event(
        &mut self,
        payload: OnlinePayload,
        outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError>;

    /// Handles an accumulated `Offline` event.
    fn handle_offline_event(
        &mut self,
        pub_id: PublicId,
        outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError>;

    /// Handles a completed DKG.
    fn handle_dkg_result_event(
        &mut self,
        participants: &BTreeSet<PublicId>,
        dkg_result: &DkgResultWrapper,
    ) -> Result<(), RoutingError>;

    /// Handles an accumulated `OurMerge` event.
    fn handle_our_merge_event(&mut self) -> Result<(), RoutingError>;

    /// Handles an accumulated `NeighbourMerge` event.
    fn handle_neighbour_merge_event(&mut self) -> Result<(), RoutingError>;

    /// Handles an accumulated `SectionInfo` event.
    fn handle_section_info_event(
        &mut self,
        elders_info: EldersInfo,
        old_pfx: Prefix<XorName>,
        neighbour_change: EldersChange,
        outbox: &mut dyn EventBox,
    ) -> Result<Transition, RoutingError>;

    /// Handle an accumulated `TheirKeyInfo` event
    fn handle_their_key_info_event(&mut self, key_info: SectionKeyInfo)
        -> Result<(), RoutingError>;

    /// Handle an accumulated `SendAckMessage` event
    fn handle_send_ack_message_event(
        &mut self,
        ack_payload: SendAckMessagePayload,
    ) -> Result<(), RoutingError>;

    /// Handle an accumulated `Relocate` event
    fn handle_relocate_event(
        &mut self,
        payload: RelocateDetails,
        signature: BlsSignature,
        outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError>;

    /// Handle an accumulated `User` event
    fn handle_user_event(
        &mut self,
        payload: Vec<u8>,
        outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        self.send_event(Event::Consensus(payload), outbox);
        Ok(())
    }

    fn handle_parsec_request(
        &mut self,
        msg_version: u64,
        par_request: parsec::Request,
        p2p_node: P2pNode,
        outbox: &mut dyn EventBox,
    ) -> Result<Transition, RoutingError> {
        let log_ident = self.log_ident();
        let (response, poll) = self.parsec_map_mut().handle_request(
            msg_version,
            par_request,
            *p2p_node.public_id(),
            &log_ident,
        );

        if let Some(response) = response {
            self.send_direct_message(p2p_node.connection_info(), response);
        }

        if poll {
            self.parsec_poll(outbox)
        } else {
            Ok(Transition::Stay)
        }
    }

    fn handle_parsec_response(
        &mut self,
        msg_version: u64,
        par_response: parsec::Response,
        pub_id: PublicId,
        outbox: &mut dyn EventBox,
    ) -> Result<Transition, RoutingError> {
        let log_ident = self.log_ident();
        if self
            .parsec_map_mut()
            .handle_response(msg_version, par_response, pub_id, &log_ident)
        {
            self.parsec_poll(outbox)
        } else {
            Ok(Transition::Stay)
        }
    }

    fn send_parsec_gossip(&mut self, target: Option<(u64, P2pNode)>) {
        let (version, gossip_target) = match target {
            Some((v, p)) => (v, p),
            None => {
                let version = self.parsec_map().last_version();
                let recipients = self.parsec_map().gossip_recipients();
                if recipients.is_empty() {
                    // Parsec hasn't caught up with the event of us joining yet.
                    return;
                }

                let p2p_recipients: Vec<_> = recipients
                    .into_iter()
                    .filter_map(|pub_id| self.chain().get_member_p2p_node(pub_id.name()))
                    .cloned()
                    .collect();

                if p2p_recipients.is_empty() {
                    log_or_panic!(
                        LogLevel::Error,
                        "{} - Not connected to any gossip recipient.",
                        self
                    );
                    return;
                }

                let rand_index = self.rng().gen_range(0, p2p_recipients.len());
                (version, p2p_recipients[rand_index].clone())
            }
        };

        if let Some(msg) = self
            .parsec_map_mut()
            .create_gossip(version, gossip_target.public_id())
        {
            self.send_direct_message(gossip_target.connection_info(), msg);
        }
    }

    fn parsec_poll(&mut self, outbox: &mut dyn EventBox) -> Result<Transition, RoutingError> {
        while let Some(block) = self.parsec_map_mut().poll() {
            let parsec_version = self.parsec_map_mut().last_version();
            match block.payload() {
                Observation::Accusation { .. } => {
                    // FIXME: Handle properly
                    unreachable!("...")
                }
                Observation::Genesis {
                    group,
                    related_info,
                } => {
                    // FIXME: Validate with Chain info.

                    trace!(
                        "{} Parsec Genesis {}: group {:?} - related_info {}",
                        self,
                        parsec_version,
                        group,
                        related_info.len()
                    );

                    for pub_id in group {
                        // Notify upper layers about the new node.
                        self.send_event(Event::NodeAdded(*pub_id.name()), outbox);
                    }

                    self.chain_mut()
                        .handle_genesis_event(&group, &related_info)?;
                    self.set_pfx_successfully_polled(true);

                    continue;
                }
                Observation::OpaquePayload(event) => {
                    if let Some(proof) = block.proofs().iter().next().map(|p| Proof {
                        pub_id: *p.public_id(),
                        sig: *p.signature(),
                    }) {
                        trace!(
                            "{} Parsec OpaquePayload {}: {} - {:?}",
                            self,
                            parsec_version,
                            proof.pub_id(),
                            event
                        );
                        self.chain_mut().handle_opaque_event(event, proof)?;
                    }
                }
                Observation::Add { peer_id, .. } => {
                    log_or_panic!(
                        LogLevel::Error,
                        "{} Unexpected Parsec Add {}: - {}",
                        self,
                        parsec_version,
                        peer_id
                    );
                }
                Observation::Remove { peer_id, .. } => {
                    log_or_panic!(
                        LogLevel::Error,
                        "{} Unexpected Parsec Remove {}: - {}",
                        self,
                        parsec_version,
                        peer_id
                    );
                }
                obs @ Observation::StartDkg(_) | obs @ Observation::DkgMessage(_) => {
                    log_or_panic!(
                        LogLevel::Error,
                        "parsec_poll polled internal Observation {}: {:?}",
                        parsec_version,
                        obs
                    );
                }
                Observation::DkgResult {
                    participants,
                    dkg_result,
                } => {
                    self.handle_dkg_result_event(participants, dkg_result)?;
                }
            }

            match self.chain_poll(outbox)? {
                Transition::Stay => (),
                transition => return Ok(transition),
            }
        }

        self.check_voting_status();

        Ok(Transition::Stay)
    }

    fn chain_poll(&mut self, outbox: &mut dyn EventBox) -> Result<Transition, RoutingError> {
        let mut our_pfx = *self.chain_mut().our_prefix();
        while let Some(event) = self.chain_mut().poll()? {
            trace!("{} Handle accumulated event: {:?}", self, event);

            match event.content {
                AccumulatingEvent::StartDkg(_) => {
                    log_or_panic!(
                        LogLevel::Error,
                        "StartDkg came out of Parsec - this shouldn't happen"
                    );
                }
                AccumulatingEvent::Online(payload) => {
                    self.handle_online_event(payload, outbox)?;
                }
                AccumulatingEvent::Offline(pub_id) => {
                    self.handle_offline_event(pub_id, outbox)?;
                }
                AccumulatingEvent::OurMerge => self.handle_our_merge_event()?,
                AccumulatingEvent::NeighbourMerge(_) => self.handle_neighbour_merge_event()?,
                AccumulatingEvent::SectionInfo(elders_info)
                | AccumulatingEvent::NeighbourInfo(elders_info) => {
                    match self.handle_section_info_event(
                        elders_info,
                        our_pfx,
                        event.neighbour_change,
                        outbox,
                    )? {
                        Transition::Stay => (),
                        transition => return Ok(transition),
                    }
                }
                AccumulatingEvent::TheirKeyInfo(key_info) => {
                    self.handle_their_key_info_event(key_info)?
                }
                AccumulatingEvent::AckMessage(_payload) => {
                    // Update their_knowledge is handled within the chain.
                }
                AccumulatingEvent::SendAckMessage(payload) => {
                    self.handle_send_ack_message_event(payload)?
                }
                AccumulatingEvent::ParsecPrune => {
                    info!(
                        "{} Handling chain {:?} not yet implemented, ignoring.",
                        self, event
                    );
                }
                AccumulatingEvent::Relocate(payload) => {
                    self.invoke_handle_relocate_event(payload, event.signature, outbox)?
                }
                AccumulatingEvent::User(payload) => self.handle_user_event(payload, outbox)?,
            }

            our_pfx = *self.chain_mut().our_prefix();
        }

        Ok(Transition::Stay)
    }

    // Checking members vote status and vote to remove those non-resposive nodes.
    fn check_voting_status(&mut self) {
        let unresponsive_nodes = self.chain_mut().check_vote_status();
        let log_indent = self.log_ident();
        for pub_id in unresponsive_nodes.iter() {
            self.parsec_map_mut().vote_for(
                AccumulatingEvent::Offline(*pub_id).into_network_event(),
                &log_indent,
            );
        }
    }

    fn send_connection_request(
        &mut self,
        their_pub_id: PublicId,
        src: Authority<XorName>,
        dst: Authority<XorName>,
        _: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        if their_pub_id == *self.id() {
            trace!("{} - Not sending connection request to ourselves.", self);
            return Ok(());
        }

        if self.peer_map().has(&their_pub_id) {
            trace!(
                "{} - Not sending connection request to {} - already connected.",
                self,
                their_pub_id
            );
            return Ok(());
        }

        let msg_id = self.rng().gen();
        let content = MessageContent::ConnectionRequest {
            conn_info: self.our_connection_info()?,
            pub_id: *self.full_id().public_id(),
            msg_id,
        };

        trace!("{} - Sending connection request to {}.", self, their_pub_id);

        self.send_routing_message(RoutingMessage { src, dst, content })
            .map_err(|err| {
                debug!(
                    "{} - Failed to send connection request to {}: {:?}.",
                    self, their_pub_id, err
                );
                err
            })
    }

    fn handle_connection_request(
        &mut self,
        their_conn_info: ConnectionInfo,
        their_pub_id: PublicId,
        src: Authority<XorName>,
        dst: Authority<XorName>,
        _: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        if src.single_signing_name() != Some(their_pub_id.name()) {
            // Connection request not from the source node.
            return Err(RoutingError::InvalidSource);
        }

        if dst.single_signing_name() != Some(self.name()) {
            // Connection request not for us.
            return Err(RoutingError::InvalidDestination);
        }

        debug!(
            "{} - Received connection request from {:?}.",
            self, their_pub_id
        );

        self.peer_map_mut()
            .insert(their_pub_id, their_conn_info.clone());
        self.send_direct_message(&their_conn_info, DirectMessage::ConnectionResponse);

        Ok(())
    }

    fn invoke_handle_relocate_event(
        &mut self,
        details: RelocateDetails,
        signature: Option<BlsSignature>,
        outbox: &mut dyn EventBox,
    ) -> Result<(), RoutingError> {
        if let Some(signature) = signature {
            self.handle_relocate_event(details, signature, outbox)
        } else {
            log_or_panic!(
                LogLevel::Error,
                "{} - Unsigned Relocate event {:?}",
                self,
                details
            );
            Err(RoutingError::FailedSignature)
        }
    }
}

#[allow(unused)]
fn to_proof_set(block: &Block) -> ProofSet {
    let sigs = block
        .proofs()
        .iter()
        .map(|proof| (*proof.public_id(), *proof.signature()))
        .collect();
    ProofSet { sigs }
}
