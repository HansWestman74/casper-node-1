pub(crate) mod config;
mod participation;
mod round_success_meter;
#[cfg(test)]
mod tests;

use std::{
    any::Any,
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Debug,
    iter,
    path::PathBuf,
};

use datasize::DataSize;
use itertools::Itertools;
use num_traits::AsPrimitive;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, trace, warn};

use casper_types::{system::auction::BLOCK_REWARD, U512};

use crate::{
    components::consensus::{
        config::{Config, ProtocolConfig},
        consensus_protocol::{
            BlockContext, ConsensusProtocol, ProposedBlock, ProtocolOutcome, ProtocolOutcomes,
        },
        highway_core::{
            active_validator::Effect as AvEffect,
            finality_detector::{FinalityDetector, FttExceeded},
            highway::{
                Dependency, GetDepOutcome, Highway, Params, PreValidatedVertex, ValidVertex,
                Vertex, VertexError,
            },
            state::{self, IndexObservation, IndexPanorama, Observation, Panorama},
            synchronizer::Synchronizer,
            validators::{ValidatorIndex, Validators},
        },
        traits::{ConsensusValueT, Context, NodeIdT},
        ActionId, TimerId,
    },
    types::{TimeDiff, Timestamp},
    NodeRng,
};

use self::round_success_meter::RoundSuccessMeter;

/// Never allow more than this many units in a piece of evidence for conflicting endorsements,
/// even if eras are longer than this.
const MAX_ENDORSEMENT_EVIDENCE_LIMIT: u64 = 10_000;

/// The timer for creating new units, as a validator actively participating in consensus.
const TIMER_ID_ACTIVE_VALIDATOR: TimerId = TimerId(0);
/// The timer for adding a vertex with a future timestamp.
const TIMER_ID_VERTEX_WITH_FUTURE_TIMESTAMP: TimerId = TimerId(1);
/// The timer for purging expired pending vertices from the queues.
const TIMER_ID_PURGE_VERTICES: TimerId = TimerId(2);
/// The timer for logging inactive validators.
const TIMER_ID_LOG_PARTICIPATION: TimerId = TimerId(3);
/// The timer for an alert no progress was made in a long time.
const TIMER_ID_STANDSTILL_ALERT: TimerId = TimerId(4);
/// The timer for logging synchronizer queue size.
const TIMER_ID_SYNCHRONIZER_LOG: TimerId = TimerId(5);
/// The timer to request the latest state from a random peer.
const TIMER_ID_REQUEST_STATE: TimerId = TimerId(6);

/// The action of adding a vertex from the `vertices_to_be_added` queue.
pub(crate) const ACTION_ID_VERTEX: ActionId = ActionId(0);

#[derive(DataSize, Debug)]
pub(crate) struct HighwayProtocol<I, C>
where
    I: DataSize,
    C: Context,
{
    /// Incoming blocks we can't add yet because we are waiting for validation.
    pending_values: HashMap<ProposedBlock<C>, HashSet<(ValidVertex<C>, I)>>,
    finality_detector: FinalityDetector<C>,
    highway: Highway<C>,
    /// A tracker for whether we are keeping up with the current round exponent or not.
    round_success_meter: RoundSuccessMeter<C>,
    synchronizer: Synchronizer<I, C>,
    pvv_cache: HashMap<Dependency<C>, PreValidatedVertex<C>>,
    evidence_only: bool,
    /// The panorama snapshot. This is updated periodically, and if it does not change for too
    /// long, an alert is raised.
    last_panorama: Panorama<C>,
    config: config::Config,
}

impl<I: NodeIdT, C: Context + 'static> HighwayProtocol<I, C> {
    /// Creates a new boxed `HighwayProtocol` instance.
    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub(crate) fn new_boxed(
        instance_id: C::InstanceId,
        validator_stakes: BTreeMap<C::ValidatorId, U512>,
        faulty: &HashSet<C::ValidatorId>,
        inactive: &HashSet<C::ValidatorId>,
        protocol_config: &ProtocolConfig,
        config: &Config,
        prev_cp: Option<&dyn ConsensusProtocol<I, C>>,
        era_start_time: Timestamp,
        seed: u64,
        now: Timestamp,
    ) -> (Box<dyn ConsensusProtocol<I, C>>, ProtocolOutcomes<I, C>) {
        let validators_count = validator_stakes.len();
        let sum_stakes: U512 = validator_stakes.iter().map(|(_, stake)| *stake).sum();
        assert!(
            !sum_stakes.is_zero(),
            "cannot start era with total weight 0"
        );
        // For Highway, we need u64 weights. Scale down by  sum / u64::MAX,  rounded up.
        // If we round up the divisor, the resulting sum is guaranteed to be  <= u64::MAX.
        let scaling_factor = (sum_stakes + U512::from(u64::MAX) - 1) / U512::from(u64::MAX);
        let scale_stake = |(key, stake): (C::ValidatorId, U512)| {
            (key, AsPrimitive::<u64>::as_(stake / scaling_factor))
        };
        let mut validators: Validators<C::ValidatorId> =
            validator_stakes.into_iter().map(scale_stake).collect();

        for vid in faulty {
            validators.ban(vid);
        }
        for vid in inactive {
            validators.set_cannot_propose(vid);
        }

        assert!(
            validators.ensure_nonzero_proposing_stake(),
            "cannot start era with total weight 0"
        );

        let total_weight = u128::from(validators.total_weight());
        let ftt_fraction = protocol_config.highway.finality_threshold_fraction;
        assert!(
            ftt_fraction < 1.into(),
            "finality threshold must be less than 100%"
        );
        #[allow(clippy::integer_arithmetic)] // FTT is less than 1, so this can't overflow.
        let ftt = total_weight * *ftt_fraction.numer() as u128 / *ftt_fraction.denom() as u128;
        let ftt = (ftt as u64).into();

        let round_success_meter = prev_cp
            .and_then(|cp| cp.as_any().downcast_ref::<HighwayProtocol<I, C>>())
            .map(|highway_proto| highway_proto.next_era_round_succ_meter(era_start_time.max(now)))
            .unwrap_or_else(|| {
                RoundSuccessMeter::new(
                    protocol_config.highway.minimum_round_exponent,
                    protocol_config.highway.minimum_round_exponent,
                    protocol_config.highway.maximum_round_exponent,
                    era_start_time.max(now),
                    config.into(),
                )
            });
        // This will return the minimum round exponent if we just initialized the meter, i.e. if
        // there was no previous consensus instance or it had no round success meter.
        let init_round_exp = round_success_meter.new_exponent();

        info!(
            %init_round_exp,
            "initializing Highway instance",
        );

        // Allow about as many units as part of evidence for conflicting endorsements as we expect
        // a validator to create during an era. After that, they can endorse two conflicting forks
        // without getting faulty.
        let min_round_len = state::round_len(protocol_config.highway.minimum_round_exponent);
        let min_rounds_per_era = protocol_config
            .minimum_era_height
            .max((TimeDiff::from(1) + protocol_config.era_duration) / min_round_len);
        let endorsement_evidence_limit = min_rounds_per_era
            .saturating_mul(2)
            .min(MAX_ENDORSEMENT_EVIDENCE_LIMIT);

        let params = Params::new(
            seed,
            BLOCK_REWARD,
            (protocol_config.highway.reduced_reward_multiplier * BLOCK_REWARD).to_integer(),
            protocol_config.highway.minimum_round_exponent,
            protocol_config.highway.maximum_round_exponent,
            init_round_exp,
            protocol_config.minimum_era_height,
            era_start_time,
            era_start_time + protocol_config.era_duration,
            endorsement_evidence_limit,
        );

        let outcomes = Self::initialize_timers(now, era_start_time, &config.highway);

        let highway = Highway::new(instance_id, validators, params);
        let last_panorama = highway.state().panorama().clone();
        let hw_proto = Box::new(HighwayProtocol {
            pending_values: HashMap::new(),
            finality_detector: FinalityDetector::new(ftt),
            highway,
            round_success_meter,
            synchronizer: Synchronizer::new(validators_count, instance_id),
            pvv_cache: Default::default(),
            evidence_only: false,
            last_panorama,
            config: config.highway.clone(),
        });

        (hw_proto, outcomes)
    }

    fn initialize_timers(
        now: Timestamp,
        era_start_time: Timestamp,
        config: &config::Config,
    ) -> ProtocolOutcomes<I, C> {
        let mut outcomes = vec![ProtocolOutcome::ScheduleTimer(
            now + config.pending_vertex_timeout,
            TIMER_ID_PURGE_VERTICES,
        )];
        if let Some(interval) = config.log_participation_interval {
            outcomes.push(ProtocolOutcome::ScheduleTimer(
                now.max(era_start_time) + interval,
                TIMER_ID_LOG_PARTICIPATION,
            ));
        }
        if let Some(interval) = config.log_synchronizer_interval {
            outcomes.push(ProtocolOutcome::ScheduleTimer(
                now + interval,
                TIMER_ID_SYNCHRONIZER_LOG,
            ));
        }
        if let Some(timeout) = config.standstill_timeout {
            outcomes.push(ProtocolOutcome::ScheduleTimer(
                now.max(era_start_time) + timeout,
                TIMER_ID_STANDSTILL_ALERT,
            ));
        }
        outcomes
    }

    fn process_av_effects<E>(&mut self, av_effects: E, now: Timestamp) -> ProtocolOutcomes<I, C>
    where
        E: IntoIterator<Item = AvEffect<C>>,
    {
        av_effects
            .into_iter()
            .flat_map(|effect| self.process_av_effect(effect, now))
            .collect()
    }

    fn process_av_effect(&mut self, effect: AvEffect<C>, now: Timestamp) -> ProtocolOutcomes<I, C> {
        match effect {
            AvEffect::NewVertex(vv) => {
                self.log_unit_size(vv.inner(), "sending new unit");
                self.calculate_round_exponent(&vv, now);
                self.process_new_vertex(vv)
            }
            AvEffect::ScheduleTimer(timestamp) => {
                vec![ProtocolOutcome::ScheduleTimer(
                    timestamp,
                    TIMER_ID_ACTIVE_VALIDATOR,
                )]
            }
            AvEffect::RequestNewBlock(block_context) => {
                vec![ProtocolOutcome::CreateNewBlock(block_context)]
            }
            AvEffect::WeAreFaulty(fault) => {
                error!("this validator is faulty: {:?}", fault);
                vec![ProtocolOutcome::WeAreFaulty]
            }
        }
    }

    fn process_new_vertex(&mut self, vv: ValidVertex<C>) -> ProtocolOutcomes<I, C> {
        let mut outcomes = Vec::new();
        if let Vertex::Evidence(ev) = vv.inner() {
            let v_id = self
                .highway
                .validators()
                .id(ev.perpetrator())
                .expect("validator not found") // We already validated this vertex.
                .clone();
            outcomes.push(ProtocolOutcome::NewEvidence(v_id));
        }
        let msg = HighwayMessage::NewVertex(vv.into());
        outcomes.push(ProtocolOutcome::CreatedGossipMessage(msg.serialize()));
        outcomes.extend(self.detect_finality());
        outcomes
    }

    fn detect_finality(&mut self) -> ProtocolOutcomes<I, C> {
        let faulty_weight = match self.finality_detector.run(&self.highway) {
            Ok(iter) => return iter.map(ProtocolOutcome::FinalizedBlock).collect(),
            Err(FttExceeded(weight)) => weight.0,
        };
        error!(
            %faulty_weight,
            total_weight = %self.highway.state().total_weight().0,
            "too many faulty validators"
        );
        self.log_participation();
        vec![ProtocolOutcome::FttExceeded]
    }

    /// Adds the given vertices to the protocol state, if possible, or requests missing
    /// dependencies or validation. Recursively schedules events to add everything that is
    /// unblocked now.
    fn add_vertex(&mut self, now: Timestamp) -> ProtocolOutcomes<I, C> {
        let (maybe_pending_vertex, mut outcomes) = self.synchronizer.pop_vertex_to_add(
            &self.highway,
            &self.pending_values,
            self.config.max_requests_for_vertex,
        );
        let pending_vertex = match maybe_pending_vertex {
            None => return outcomes,
            Some(pending_vertex) => pending_vertex,
        };

        // If unit is sent by a doppelganger, deactivate this instance of an active
        // validator. Continue processing the unit so that it can be added to the state.
        if self.highway.is_doppelganger_vertex(pending_vertex.vertex()) {
            error!(
                vertex = ?pending_vertex.vertex(),
                "received vertex from a doppelganger. \
                 Are you running multiple nodes with the same validator key?",
            );
            self.deactivate_validator();
            outcomes.push(ProtocolOutcome::DoppelgangerDetected);
        }

        // If the vertex is invalid, drop all vertices that depend on this one, and disconnect from
        // the faulty senders.
        let sender = pending_vertex.sender().clone();
        let vv = match self.highway.validate_vertex(pending_vertex.into()) {
            Ok(vv) => vv,
            Err((pvv, err)) => {
                info!(?pvv, ?err, "invalid vertex");
                let vertices = vec![pvv.inner().id()];
                let faulty_senders = self.synchronizer.invalid_vertices(vertices);
                outcomes.extend(faulty_senders.into_iter().map(ProtocolOutcome::Disconnect));
                return outcomes;
            }
        };

        // If the vertex contains a consensus value, i.e. it is a proposal, request validation.
        let vertex = vv.inner();
        if let (Some(value), Some(timestamp), Some(swunit)) =
            (vertex.value(), vertex.timestamp(), vertex.unit())
        {
            let panorama = &swunit.wire_unit().panorama;
            let fork_choice = self.highway.state().fork_choice(panorama);
            if value.needs_validation() {
                self.log_proposal(vertex, "requesting proposal validation");
                let ancestor_values = self.ancestors(fork_choice).cloned().collect();
                let block_context = BlockContext::new(timestamp, ancestor_values);
                let proposed_block = ProposedBlock::new(value.clone(), block_context);
                if self
                    .pending_values
                    .entry(proposed_block.clone())
                    .or_default()
                    .insert((vv, sender.clone()))
                {
                    outcomes.push(ProtocolOutcome::ValidateConsensusValue {
                        sender,
                        proposed_block,
                    });
                }
                return outcomes;
            } else {
                self.log_proposal(vertex, "proposal does not need validation");
            }
        }

        // Either consensus value doesn't need validation or it's not a proposal.
        // We can add it to the state.
        outcomes.extend(self.add_valid_vertex(vv, now));
        // If we added new vertices to the state, check whether any dependencies we were
        // waiting for are now satisfied, and try adding the pending vertices as well.
        outcomes.extend(self.synchronizer.remove_satisfied_deps(&self.highway));
        // Check whether any new blocks were finalized.
        outcomes.extend(self.detect_finality());
        outcomes
    }

    fn calculate_round_exponent(&mut self, vv: &ValidVertex<C>, now: Timestamp) {
        let new_round_exp = self
            .round_success_meter
            .calculate_new_exponent(self.highway.state());
        // If the vertex contains a proposal, register it in the success meter.
        // It's important to do this _after_ the calculation above - otherwise we might try to
        // register the proposal before the meter is aware that a new round has started, and it
        // will reject the proposal.
        if vv.is_proposal() {
            let vertex = vv.inner();
            if let (Some(hash), Some(timestamp)) = (vertex.unit_hash(), vertex.timestamp()) {
                trace!(%now, timestamp = timestamp.millis(), "adding proposal to protocol state");
                self.round_success_meter.new_proposal(hash, timestamp);
            } else {
                error!(?vertex, "proposal without unit hash and timestamp");
            }
        }
        self.highway.set_round_exp(new_round_exp);
    }

    fn add_valid_vertex(&mut self, vv: ValidVertex<C>, now: Timestamp) -> ProtocolOutcomes<I, C> {
        if self.evidence_only && !vv.inner().is_evidence() {
            error!(vertex = ?vv.inner(), "unexpected vertex in evidence-only mode");
            return vec![];
        }
        if self.highway.has_vertex(vv.inner()) {
            return vec![];
        }
        if !vv.inner().is_proposal() {
            if let Some(hash) = vv.inner().unit_hash() {
                trace!(?hash, "adding unit to the protocol state");
            } else {
                trace!(vertex=?vv.inner(), "adding vertex to the protocol state");
            }
        }
        self.log_unit_size(vv.inner(), "adding new unit to the protocol state");
        self.log_proposal(vv.inner(), "adding valid proposal to the protocol state");
        let vertex_id = vv.inner().id();
        // Check whether we should change the round exponent.
        // It's important to do it before the vertex is added to the state - this way if the last
        // round has finished, we now have all the vertices from that round in the state, and no
        // newer ones.
        self.calculate_round_exponent(&vv, now);
        let av_effects = self.highway.add_valid_vertex(vv, now);
        // Once vertex is added to the state, we can remove it from the cache.
        self.pvv_cache.remove(&vertex_id);
        self.process_av_effects(av_effects, now)
    }

    /// Returns an instance of `RoundSuccessMeter` for the new era: resetting the counters where
    /// appropriate.
    fn next_era_round_succ_meter(&self, timestamp: Timestamp) -> RoundSuccessMeter<C> {
        self.round_success_meter.next_era(timestamp)
    }

    /// Returns an iterator over all the values that are in parents of the given block.
    fn ancestors<'a>(
        &'a self,
        mut maybe_hash: Option<&'a C::Hash>,
    ) -> impl Iterator<Item = &'a C::ConsensusValue> {
        iter::from_fn(move || {
            let hash = maybe_hash.take()?;
            let block = self.highway.state().block(hash);
            let value = Some(&block.value);
            maybe_hash = block.parent();
            value
        })
    }

    /// Prints a log statement listing the inactive and faulty validators.
    fn log_participation(&self) {
        let instance_id = self.highway.instance_id();
        let participation = participation::Participation::new(&self.highway);
        info!(?participation, %instance_id, "validator participation");
    }

    /// Logs the vertex' serialized size.
    fn log_unit_size(&self, vertex: &Vertex<C>, log_msg: &str) {
        if self.config.log_unit_sizes {
            if let Some(hash) = vertex.unit_hash() {
                let size = HighwayMessage::NewVertex(vertex.clone()).serialize().len();
                info!(size, %hash, "{}", log_msg);
            }
        }
    }

    /// Returns whether the switch block has already been finalized.
    fn finalized_switch_block(&self) -> bool {
        let is_switch = |block_hash: &C::Hash| self.highway.state().is_terminal_block(block_hash);
        self.finality_detector
            .last_finalized()
            .map_or(false, is_switch)
    }

    /// Request the latest state from a random peer.
    fn handle_request_state_timer(&mut self, now: Timestamp) -> ProtocolOutcomes<I, C> {
        if self.evidence_only || self.finalized_switch_block() {
            return vec![]; // Era has ended. No further progress is expected.
        }
        debug!(
            instance_id = ?self.highway.instance_id(),
            "requesting latest state from random peer",
        );
        // Request latest state from a peer and schedule the next request.
        let mut outcomes = self.latest_state_request();
        if let Some(interval) = self.config.request_state_interval {
            outcomes.push(ProtocolOutcome::ScheduleTimer(
                now + interval,
                TIMER_ID_REQUEST_STATE,
            ));
        }
        outcomes
    }

    /// Returns a `StandstillAlert` if no progress was made; otherwise schedules the next check.
    fn handle_standstill_alert_timer(&mut self, now: Timestamp) -> ProtocolOutcomes<I, C> {
        if self.evidence_only || self.finalized_switch_block() {
            // Era has ended and no further progress is expected, or shutdown on standstill is
            // turned off.
            return vec![];
        }
        let timeout = match self.config.standstill_timeout {
            None => return vec![],
            Some(timeout) => timeout,
        };
        if self.last_panorama == *self.highway.state().panorama() {
            info!(
                instance_id = ?self.highway.instance_id(),
                "no progress in the last {}, raising standstill alert",
                timeout,
            );
            return vec![ProtocolOutcome::StandstillAlert]; // No progress within the timeout.
        }
        debug!(
            instance_id = ?self.highway.instance_id(),
            "progress detected; scheduling next standstill check in {}",
            timeout,
        );
        // Record the current panorama and schedule the next standstill check.
        self.last_panorama = self.highway.state().panorama().clone();
        vec![ProtocolOutcome::ScheduleTimer(
            now + timeout,
            TIMER_ID_STANDSTILL_ALERT,
        )]
    }

    /// Prints a log message if the vertex is a proposal unit. Otherwise returns `false`.
    fn log_proposal(&self, vertex: &Vertex<C>, msg: &str) -> bool {
        let (wire_unit, hash) = match vertex.unit() {
            Some(swu) if swu.wire_unit().value.is_some() => (swu.wire_unit(), swu.hash()),
            _ => return false, // Not a proposal.
        };
        let creator = if let Some(creator) = self.highway.validators().id(wire_unit.creator) {
            creator
        } else {
            error!(?wire_unit, "{}: invalid creator", msg);
            return true;
        };
        info!(
            ?hash,
            ?creator,
            creator_index = wire_unit.creator.0,
            timestamp = %wire_unit.timestamp,
            round_exp = wire_unit.round_exp,
            seq_number = wire_unit.seq_number,
            "{}", msg
        );
        true
    }

    // Logs the details about the received vertex.
    fn log_received_vertex(&self, vertex: &Vertex<C>) {
        match vertex {
            Vertex::Unit(swu) => {
                let creator = if let Some(creator) = vertex
                    .creator()
                    .and_then(|vid| self.highway.validators().id(vid))
                {
                    creator
                } else {
                    error!(?vertex, "invalid creator");
                    return;
                };

                let wire_unit = swu.wire_unit();
                let hash = swu.hash();

                if vertex.is_proposal() {
                    info!(
                        ?hash,
                        ?creator,
                        creator_index = wire_unit.creator.0,
                        timestamp = %wire_unit.timestamp,
                        round_exp = wire_unit.round_exp,
                        seq_number = wire_unit.seq_number,
                        "received a proposal"
                    );
                } else {
                    trace!(
                        ?hash,
                        ?creator,
                        creator_index = wire_unit.creator.0,
                        timestamp = %wire_unit.timestamp,
                        round_exp = wire_unit.round_exp,
                        seq_number = wire_unit.seq_number,
                        "received a non-proposal unit"
                    );
                };
            }
            Vertex::Evidence(evidence) => trace!(?evidence, "received an evidence"),
            Vertex::Endorsements(endorsement) => trace!(?endorsement, "received an endorsement"),
            Vertex::Ping(ping) => trace!(?ping, "received ping"),
        }
    }

    /// Prevalidates the vertex but checks the cache for previously validated vertices.
    /// Avoids multiple validation of the same vertex.
    fn pre_validate_vertex(
        &mut self,
        v: Vertex<C>,
    ) -> Result<PreValidatedVertex<C>, (Vertex<C>, VertexError)> {
        let id = v.id();
        if let Some(prev_pvv) = self.pvv_cache.get(&id) {
            return Ok(prev_pvv.clone());
        }
        let pvv = self.highway.pre_validate_vertex(v)?;
        self.pvv_cache.insert(id, pvv.clone());
        Ok(pvv)
    }

    /// Creates a message to send our panorama to a random peer.
    fn latest_state_request(&self) -> ProtocolOutcomes<I, C> {
        let request: HighwayMessage<C> = HighwayMessage::LatestStateRequest(
            IndexPanorama::from_panorama(self.highway.state().panorama(), self.highway.state()),
        );
        let payload = (&request).serialize();
        vec![ProtocolOutcome::CreatedMessageToRandomPeer(payload)]
    }

    /// Creates a batch of dependency requests if the peer has more units by the validator `vidx`
    /// than we do; otherwise sends a batch of missing units to the peer.
    fn batch_request(
        &self,
        rng: &mut NodeRng,
        vid: ValidatorIndex,
        our_next_seq: u64,
        their_next_seq: u64,
    ) -> Vec<HighwayMessage<C>> {
        let state = self.highway.state();
        if our_next_seq == their_next_seq {
            return vec![];
        }
        if our_next_seq < their_next_seq {
            // We're behind. Request missing vertices.
            (our_next_seq..their_next_seq)
                .take(self.config.max_request_batch_size)
                .map(|unit_seq_number| {
                    let uuid = rng.next_u64();
                    debug!(?uuid, ?vid, ?unit_seq_number, "requesting dependency");
                    HighwayMessage::RequestDependencyByHeight {
                        uuid,
                        vid,
                        unit_seq_number,
                    }
                })
                .into_iter()
                .collect()
        } else {
            // We're ahead.
            match state.panorama().get(vid) {
                None => {
                    warn!(?vid, "received a request for non-existing validator");
                    vec![]
                }
                Some(observation) => match observation {
                    Observation::None => {
                        warn!(
                            ?vid,
                            our_next_seq,
                            ?observation,
                            "expected unit for validator but found none"
                        );
                        vec![]
                    }
                    Observation::Faulty => {
                        let ev = match state.maybe_evidence(vid) {
                            Some(ev) => ev.clone(),
                            None => {
                                warn!(
                                    ?vid, instance_id=?self.highway.instance_id(),
                                    "panorama marked validator as faulty but no evidence was found"
                                );
                                return vec![];
                            }
                        };
                        vec![HighwayMessage::NewVertex(Vertex::Evidence(ev))]
                    }
                    Observation::Correct(hash) => (their_next_seq..our_next_seq)
                        .take(self.config.max_request_batch_size)
                        .filter_map(|seq_num| {
                            let unit = state.find_in_swimlane(hash, seq_num).unwrap();
                            state
                                .wire_unit(unit, *self.highway.instance_id())
                                .map(|swu| HighwayMessage::NewVertex(Vertex::Unit(swu)))
                        })
                        .into_iter()
                        .collect(),
                },
            }
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(bound(
    serialize = "C::Hash: Serialize",
    deserialize = "C::Hash: Deserialize<'de>",
))]
pub(crate) enum HighwayMessage<C: Context> {
    NewVertex(Vertex<C>),
    // A dependency request. u64 is a random UUID identifying the request.
    RequestDependency(u64, Dependency<C>),
    RequestDependencyByHeight {
        uuid: u64,
        vid: ValidatorIndex,
        unit_seq_number: u64,
    },
    LatestStateRequest(IndexPanorama),
}

impl<C: Context> HighwayMessage<C> {
    pub(crate) fn serialize(&self) -> Vec<u8> {
        bincode::serialize(self).expect("should serialize message")
    }
}

impl<I, C> ConsensusProtocol<I, C> for HighwayProtocol<I, C>
where
    I: NodeIdT,
    C: Context + 'static,
{
    fn handle_message(
        &mut self,
        rng: &mut NodeRng,
        sender: I,
        msg: Vec<u8>,
        now: Timestamp,
    ) -> ProtocolOutcomes<I, C> {
        match bincode::deserialize(msg.as_slice()) {
            Err(err) => vec![ProtocolOutcome::InvalidIncomingMessage(
                msg,
                sender,
                err.into(),
            )],
            Ok(HighwayMessage::NewVertex(v))
                if self.highway.has_vertex(&v) || (self.evidence_only && !v.is_evidence()) =>
            {
                trace!(
                    has_vertex = self.highway.has_vertex(&v),
                    is_evidence = v.is_evidence(),
                    evidence_only = %self.evidence_only,
                    "received an irrelevant vertex"
                );
                vec![]
            }
            Ok(HighwayMessage::NewVertex(v)) => {
                let v_id = v.id();
                // If we already have that vertex, do not process it.
                if self.highway.has_dependency(&v_id) {
                    return vec![];
                }
                let pvv = match self.pre_validate_vertex(v) {
                    Ok(pvv) => pvv,
                    Err((_, err)) => {
                        trace!("received an invalid vertex");
                        // drop the vertices that might have depended on this one
                        let faulty_senders = self.synchronizer.invalid_vertices(vec![v_id]);
                        return iter::once(ProtocolOutcome::InvalidIncomingMessage(
                            msg,
                            sender,
                            err.into(),
                        ))
                        .chain(faulty_senders.into_iter().map(ProtocolOutcome::Disconnect))
                        .collect();
                    }
                };
                // Keep track of whether the prevalidated vertex was from an equivocator
                let is_faulty = match pvv.inner().creator() {
                    Some(creator) => self.highway.state().is_faulty(creator),
                    None => false,
                };

                if is_faulty && !self.synchronizer.is_dependency(&pvv.inner().id()) {
                    trace!("received a vertex from a faulty validator; dropping");
                    return vec![];
                }

                match pvv.timestamp() {
                    Some(timestamp) if timestamp > now + self.config.pending_vertex_timeout => {
                        trace!("received a vertex with a timestamp far in the future; dropping");
                        vec![]
                    }
                    Some(timestamp) if timestamp > now => {
                        // If it's not from an equivocator and from the future, add to queue
                        trace!("received a vertex from the future; storing for later");
                        self.synchronizer
                            .store_vertex_for_addition_later(timestamp, now, sender, pvv);
                        let timer_id = TIMER_ID_VERTEX_WITH_FUTURE_TIMESTAMP;
                        vec![ProtocolOutcome::ScheduleTimer(timestamp, timer_id)]
                    }
                    _ => {
                        // If it's not from an equivocator or it is a transitive dependency, add the
                        // vertex
                        self.log_received_vertex(pvv.inner());
                        self.synchronizer.schedule_add_vertex(sender, pvv, now)
                    }
                }
            }
            Ok(HighwayMessage::RequestDependency(uuid, dep)) => {
                trace!(?uuid, dependency=?dep, "received a request for a dependency");
                match self.highway.get_dependency(&dep) {
                    GetDepOutcome::None => {
                        info!(?dep, peer_id=?sender, "requested dependency doesn't exist");
                        vec![]
                    }
                    GetDepOutcome::Evidence(vid) => {
                        vec![ProtocolOutcome::SendEvidence(sender, vid)]
                    }
                    // TODO: Should this be done via a gossip service?
                    GetDepOutcome::Vertex(vv) => vec![ProtocolOutcome::CreatedTargetedMessage(
                        HighwayMessage::NewVertex(vv.into()).serialize(),
                        sender,
                    )],
                }
            }
            Ok(HighwayMessage::RequestDependencyByHeight {
                uuid,
                vid,
                unit_seq_number,
            }) => {
                debug!(
                    ?uuid,
                    ?vid,
                    ?unit_seq_number,
                    "received a request for a dependency"
                );
                match self.highway.get_dependency_by_index(vid, unit_seq_number) {
                    GetDepOutcome::None => {
                        info!(
                            ?vid,
                            ?unit_seq_number,
                            ?sender,
                            "requested dependency doesn't exist"
                        );
                        vec![]
                    }
                    GetDepOutcome::Evidence(vid) => {
                        vec![ProtocolOutcome::SendEvidence(sender, vid)]
                    }
                    // TODO: Should this be done via a gossip service?
                    GetDepOutcome::Vertex(vv) => {
                        vec![ProtocolOutcome::CreatedTargetedMessage(
                            HighwayMessage::NewVertex(vv.into()).serialize(),
                            sender,
                        )]
                    }
                }
            }
            Ok(HighwayMessage::LatestStateRequest(their_index_panorama)) => {
                trace!("received a request for the latest state");
                let state = self.highway.state();

                let create_message = |((vid, our_obs), their_obs): (
                    (ValidatorIndex, &IndexObservation),
                    &IndexObservation,
                )| {
                    match (*our_obs, *their_obs) {
                        (our_obs, their_obs) if our_obs == their_obs => vec![],

                        (IndexObservation::Faulty, _) => state
                            .maybe_evidence(vid)
                            .map(|evidence| {
                                HighwayMessage::NewVertex(Vertex::Evidence(evidence.clone()))
                            })
                            .into_iter()
                            .collect(),

                        (_, IndexObservation::Faulty) => {
                            let dependency = Dependency::Evidence(vid);
                            let uuid = rng.next_u64();
                            debug!(?uuid, "requesting evidence");
                            vec![HighwayMessage::RequestDependency(uuid, dependency)]
                        }

                        (
                            IndexObservation::NextSeq(our_next_seq),
                            IndexObservation::NextSeq(their_next_seq),
                        ) => self.batch_request(rng, vid, our_next_seq, their_next_seq),
                    }
                };

                IndexPanorama::from_panorama(state.panorama(), state)
                    .enumerate()
                    .zip(&their_index_panorama)
                    .map(create_message)
                    .flat_map(|msgs| {
                        msgs.into_iter().map(|msg| {
                            ProtocolOutcome::CreatedTargetedMessage(msg.serialize(), sender.clone())
                        })
                    })
                    .collect()
            }
        }
    }

    fn handle_timer(&mut self, now: Timestamp, timer_id: TimerId) -> ProtocolOutcomes<I, C> {
        match timer_id {
            TIMER_ID_ACTIVE_VALIDATOR => {
                let effects = self.highway.handle_timer(now);
                self.process_av_effects(effects, now)
            }
            TIMER_ID_VERTEX_WITH_FUTURE_TIMESTAMP => {
                self.synchronizer.add_past_due_stored_vertices(now)
            }
            TIMER_ID_PURGE_VERTICES => {
                let oldest = now.saturating_sub(self.config.pending_vertex_timeout);
                self.synchronizer.purge_vertices(oldest);
                self.pvv_cache.clear();
                let next_time = now + self.config.pending_vertex_timeout;
                vec![ProtocolOutcome::ScheduleTimer(next_time, timer_id)]
            }
            TIMER_ID_LOG_PARTICIPATION => {
                self.log_participation();
                match self.config.log_participation_interval {
                    Some(interval) if !self.evidence_only && !self.finalized_switch_block() => {
                        vec![ProtocolOutcome::ScheduleTimer(now + interval, timer_id)]
                    }
                    _ => vec![],
                }
            }
            TIMER_ID_REQUEST_STATE => self.handle_request_state_timer(now),
            TIMER_ID_STANDSTILL_ALERT => self.handle_standstill_alert_timer(now),
            TIMER_ID_SYNCHRONIZER_LOG => {
                self.synchronizer.log_len();
                match self.config.log_synchronizer_interval {
                    Some(interval) if !self.finalized_switch_block() => {
                        vec![ProtocolOutcome::ScheduleTimer(now + interval, timer_id)]
                    }
                    _ => vec![],
                }
            }
            _ => unreachable!("unexpected timer ID"),
        }
    }

    fn handle_is_current(&self, now: Timestamp) -> ProtocolOutcomes<I, C> {
        // Request latest protocol state of the current era.
        let mut outcomes = self.latest_state_request();
        // If configured, schedule periodic latest state requests.
        if let Some(interval) = self.config.request_state_interval {
            outcomes.push(ProtocolOutcome::ScheduleTimer(
                now + interval,
                TIMER_ID_REQUEST_STATE,
            ));
        }
        outcomes
    }

    fn handle_action(&mut self, action_id: ActionId, now: Timestamp) -> ProtocolOutcomes<I, C> {
        match action_id {
            ACTION_ID_VERTEX => self.add_vertex(now),
            _ => unreachable!("unexpected action ID"),
        }
    }

    fn propose(
        &mut self,
        proposed_block: ProposedBlock<C>,
        now: Timestamp,
    ) -> ProtocolOutcomes<I, C> {
        let (value, block_context) = proposed_block.destructure();
        let effects = self.highway.propose(value, block_context);
        self.process_av_effects(effects, now)
    }

    fn resolve_validity(
        &mut self,
        proposed_block: ProposedBlock<C>,
        valid: bool,
        now: Timestamp,
    ) -> ProtocolOutcomes<I, C> {
        if valid {
            let mut outcomes = self
                .pending_values
                .remove(&proposed_block)
                .into_iter()
                .flatten()
                .flat_map(|(vv, _)| self.add_valid_vertex(vv, now))
                .collect_vec();
            outcomes.extend(self.synchronizer.remove_satisfied_deps(&self.highway));
            outcomes.extend(self.detect_finality());
            outcomes
        } else {
            // TODO: Report proposer as faulty?
            // Drop vertices dependent on the invalid value.
            let dropped_vertices = self.pending_values.remove(&proposed_block);
            warn!(?proposed_block, ?dropped_vertices, "proposal is invalid");
            let dropped_vertex_ids = dropped_vertices
                .into_iter()
                .flatten()
                .map(|(vv, _)| {
                    self.log_proposal(vv.inner(), "dropping invalid proposal");
                    vv.inner().id()
                })
                .collect();
            // recursively remove vertices depending on the dropped ones
            let _faulty_senders = self.synchronizer.invalid_vertices(dropped_vertex_ids);
            // We don't disconnect from the faulty senders here: The block validator considers the
            // value "invalid" even if it just couldn't download the deploys, which could just be
            // because the original sender went offline.
            vec![]
        }
    }

    fn activate_validator(
        &mut self,
        our_id: C::ValidatorId,
        secret: C::ValidatorSecret,
        now: Timestamp,
        unit_hash_file: Option<PathBuf>,
    ) -> ProtocolOutcomes<I, C> {
        let ftt = self.finality_detector.fault_tolerance_threshold();
        let av_effects = self
            .highway
            .activate_validator(our_id, secret, now, unit_hash_file, ftt);
        self.process_av_effects(av_effects, now)
    }

    fn deactivate_validator(&mut self) {
        self.highway.deactivate_validator()
    }

    fn set_evidence_only(&mut self) {
        // TODO: We could also drop the finality detector and round success meter here. Maybe make
        // HighwayProtocol an enum with an EvidenceOnly variant?
        self.pending_values.clear();
        self.synchronizer.retain_evidence_only();
        self.highway.retain_evidence_only();
        self.evidence_only = true;
    }

    fn has_evidence(&self, vid: &C::ValidatorId) -> bool {
        self.highway.has_evidence(vid)
    }

    fn mark_faulty(&mut self, vid: &C::ValidatorId) {
        self.highway.mark_faulty(vid);
    }

    fn request_evidence(&self, sender: I, vid: &C::ValidatorId) -> ProtocolOutcomes<I, C> {
        self.highway
            .validators()
            .get_index(vid)
            .and_then(
                move |vidx| match self.highway.get_dependency(&Dependency::Evidence(vidx)) {
                    GetDepOutcome::None | GetDepOutcome::Evidence(_) => None,
                    GetDepOutcome::Vertex(vv) => {
                        let msg = HighwayMessage::NewVertex(vv.into());
                        Some(ProtocolOutcome::CreatedTargetedMessage(
                            msg.serialize(),
                            sender,
                        ))
                    }
                },
            )
            .into_iter()
            .collect()
    }

    /// Sets the pause status: While paused we don't create any new units, just pings.
    fn set_paused(&mut self, paused: bool) {
        self.highway.set_paused(paused);
    }

    fn validators_with_evidence(&self) -> Vec<&C::ValidatorId> {
        self.highway.validators_with_evidence().collect()
    }

    fn has_received_messages(&self) -> bool {
        !self.highway.state().is_empty()
            || !self.synchronizer.is_empty()
            || !self.pending_values.is_empty()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_active(&self) -> bool {
        self.highway.is_active()
    }

    fn instance_id(&self) -> &C::InstanceId {
        self.highway.instance_id()
    }

    fn next_round_length(&self) -> Option<TimeDiff> {
        self.highway.next_round_length()
    }
}
