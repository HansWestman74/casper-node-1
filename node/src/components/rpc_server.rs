//! JSON-RPC server
//!
//! The JSON-RPC server provides clients with an API for querying state and
//! sending commands to the node.
//!
//! The actual server is run in backgrounded tasks. RPCs requests are translated into reactor
//! requests to various components.
//!
//! This module currently provides both halves of what is required for an API server:
//! a component implementation that interfaces with other components via being plugged into a
//! reactor, and an external facing http server that exposes various uri routes and converts
//! JSON-RPC requests into the appropriate component events.
//!
//! For the list of supported RPC methods, see:
//! <https://github.com/CasperLabs/ceps/blob/master/text/0009-client-api.md#rpcs>

mod config;
mod event;
mod http_server;
pub mod rpcs;

use std::{convert::Infallible, fmt::Debug, time::Instant};

use datasize::DataSize;
use futures::join;

use casper_execution_engine::core::engine_state::{
    self, BalanceRequest, BalanceResult, GetBidsRequest, GetEraValidatorsError, QueryRequest,
    QueryResult,
};
use casper_hashing::Digest;
use casper_types::{system::auction::EraValidators, Key, ProtocolVersion, URef};

use self::rpcs::chain::BlockIdentifier;
use super::Component;
use crate::{
    components::contract_runtime::EraValidatorsRequest,
    effect::{
        announcements::RpcServerAnnouncement,
        requests::{
            ChainspecLoaderRequest, ConsensusRequest, ContractRuntimeRequest, LinearChainRequest,
            MetricsRequest, NetworkInfoRequest, RpcRequest, StorageRequest,
        },
        EffectBuilder, EffectExt, Effects, Responder,
    },
    types::{NodeId, StatusFeed},
    utils::{self, ListeningError},
    NodeRng,
};
pub use config::Config;
pub(crate) use event::Event;

/// A helper trait capturing all of this components Request type dependencies.
pub(crate) trait ReactorEventT:
    From<Event>
    + From<RpcRequest<NodeId>>
    + From<RpcServerAnnouncement>
    + From<ChainspecLoaderRequest>
    + From<ContractRuntimeRequest>
    + From<ConsensusRequest>
    + From<LinearChainRequest<NodeId>>
    + From<MetricsRequest>
    + From<NetworkInfoRequest<NodeId>>
    + From<StorageRequest>
    + Send
{
}

impl<REv> ReactorEventT for REv where
    REv: From<Event>
        + From<RpcRequest<NodeId>>
        + From<RpcServerAnnouncement>
        + From<ChainspecLoaderRequest>
        + From<ContractRuntimeRequest>
        + From<ConsensusRequest>
        + From<LinearChainRequest<NodeId>>
        + From<MetricsRequest>
        + From<NetworkInfoRequest<NodeId>>
        + From<StorageRequest>
        + Send
        + 'static
{
}

#[derive(DataSize, Debug)]
pub(crate) struct RpcServer {
    /// The instant at which the node has started.
    node_startup_instant: Instant,
}

impl RpcServer {
    pub(crate) fn new<REv>(
        config: Config,
        effect_builder: EffectBuilder<REv>,
        api_version: ProtocolVersion,
        node_startup_instant: Instant,
    ) -> Result<Self, ListeningError>
    where
        REv: ReactorEventT,
    {
        let builder = utils::start_listening(&config.address)?;
        tokio::spawn(http_server::run(
            builder,
            effect_builder,
            api_version,
            config.qps_limit,
        ));

        Ok(RpcServer {
            node_startup_instant,
        })
    }
}

impl RpcServer {
    fn handle_query<REv: ReactorEventT>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        state_root_hash: Digest,
        base_key: Key,
        path: Vec<String>,
        responder: Responder<Result<QueryResult, engine_state::Error>>,
    ) -> Effects<Event> {
        let query = QueryRequest::new(state_root_hash, base_key, path);
        effect_builder
            .query_global_state(query)
            .event(move |result| Event::QueryGlobalStateResult {
                result,
                main_responder: responder,
            })
    }

    fn handle_era_validators<REv: ReactorEventT>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        state_root_hash: Digest,
        protocol_version: ProtocolVersion,
        responder: Responder<Result<EraValidators, GetEraValidatorsError>>,
    ) -> Effects<Event> {
        let request = EraValidatorsRequest::new(state_root_hash, protocol_version);
        effect_builder
            .get_era_validators_from_contract_runtime(request)
            .event(move |result| Event::QueryEraValidatorsResult {
                result,
                main_responder: responder,
            })
    }

    fn handle_get_balance<REv: ReactorEventT>(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        state_root_hash: Digest,
        purse_uref: URef,
        responder: Responder<Result<BalanceResult, engine_state::Error>>,
    ) -> Effects<Event> {
        let query = BalanceRequest::new(state_root_hash, purse_uref);
        effect_builder
            .get_balance(query)
            .event(move |result| Event::GetBalanceResult {
                result,
                main_responder: responder,
            })
    }
}

impl<REv> Component<REv> for RpcServer
where
    REv: ReactorEventT,
{
    type Event = Event;
    type ConstructionError = Infallible;

    fn handle_event(
        &mut self,
        effect_builder: EffectBuilder<REv>,
        _rng: &mut NodeRng,
        event: Self::Event,
    ) -> Effects<Self::Event> {
        match event {
            Event::RpcRequest(RpcRequest::SubmitDeploy { deploy, responder }) => effect_builder
                .announce_deploy_received(deploy, Some(responder))
                .ignore(),
            Event::RpcRequest(RpcRequest::GetBlock {
                maybe_id: Some(BlockIdentifier::Hash(hash)),
                responder,
            }) => effect_builder
                .get_block_with_metadata_from_storage(hash)
                .event(move |result| Event::GetBlockResult {
                    maybe_id: Some(BlockIdentifier::Hash(hash)),
                    result: Box::new(result),
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::GetBlock {
                maybe_id: Some(BlockIdentifier::Height(height)),
                responder,
            }) => effect_builder
                .get_block_at_height_with_metadata_from_storage(height)
                .event(move |result| Event::GetBlockResult {
                    maybe_id: Some(BlockIdentifier::Height(height)),
                    result: Box::new(result),
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::GetBlock {
                maybe_id: None,
                responder,
            }) => effect_builder
                .get_highest_block_with_metadata_from_storage()
                .event(move |result| Event::GetBlockResult {
                    maybe_id: None,
                    result: Box::new(result),
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::GetBlockTransfers {
                block_hash,
                responder,
            }) => effect_builder
                .get_block_transfers_from_storage(block_hash)
                .event(move |result| Event::GetBlockTransfersResult {
                    block_hash,
                    result: Box::new(result),
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::QueryGlobalState {
                state_root_hash,
                base_key,
                path,
                responder,
            }) => self.handle_query(effect_builder, state_root_hash, base_key, path, responder),
            Event::RpcRequest(RpcRequest::QueryEraValidators {
                state_root_hash,
                protocol_version,
                responder,
            }) => self.handle_era_validators(
                effect_builder,
                state_root_hash,
                protocol_version,
                responder,
            ),
            Event::RpcRequest(RpcRequest::GetBids {
                state_root_hash,
                responder,
            }) => {
                let get_bids_request = GetBidsRequest::new(state_root_hash);
                effect_builder
                    .get_bids(get_bids_request)
                    .event(move |result| Event::GetBidsResult {
                        result,
                        main_responder: responder,
                    })
            }
            Event::RpcRequest(RpcRequest::GetBalance {
                state_root_hash,
                purse_uref,
                responder,
            }) => self.handle_get_balance(effect_builder, state_root_hash, purse_uref, responder),
            Event::RpcRequest(RpcRequest::GetDeploy { hash, responder }) => effect_builder
                .get_deploy_and_metadata_from_storage(hash)
                .event(move |result| Event::GetDeployResult {
                    hash,
                    result: Box::new(result),
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::GetPeers { responder }) => effect_builder
                .network_peers()
                .event(move |peers| Event::GetPeersResult {
                    peers,
                    main_responder: responder,
                }),
            Event::RpcRequest(RpcRequest::GetStatus { responder }) => {
                let node_uptime = self.node_startup_instant.elapsed();
                async move {
                    let (last_added_block, peers, chainspec_info, consensus_status) = join!(
                        effect_builder.get_highest_block_from_storage(),
                        effect_builder.network_peers(),
                        effect_builder.get_chainspec_info(),
                        effect_builder.consensus_status()
                    );
                    let status_feed = StatusFeed::new(
                        last_added_block,
                        peers,
                        chainspec_info,
                        consensus_status,
                        node_uptime,
                    );
                    responder.respond(status_feed).await;
                }
                .ignore()
            }
            Event::GetBlockResult {
                maybe_id: _,
                result,
                main_responder,
            } => main_responder.respond(*result).ignore(),
            Event::GetBlockTransfersResult {
                result,
                main_responder,
                ..
            } => main_responder.respond(*result).ignore(),
            Event::QueryGlobalStateResult {
                result,
                main_responder,
            } => main_responder.respond(result).ignore(),
            Event::QueryEraValidatorsResult {
                result,
                main_responder,
            } => main_responder.respond(result).ignore(),
            Event::GetBidsResult {
                result,
                main_responder,
            } => main_responder.respond(result).ignore(),
            Event::GetBalanceResult {
                result,
                main_responder,
            } => main_responder.respond(result).ignore(),
            Event::GetDeployResult {
                hash: _,
                result,
                main_responder,
            } => main_responder.respond(*result).ignore(),
            Event::GetPeersResult {
                peers,
                main_responder,
            } => main_responder.respond(peers).ignore(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use assert_json_diff::assert_json_eq;
    use schemars::schema_for_value;
    use serde_json::Value;

    use crate::rpcs::docs::OPEN_RPC_SCHEMA;

    #[test]
    fn schema() {
        // The expected schema depends on the hashing algorithm selected by the `casper-mainnet`
        // feature.
        //
        // To generate the contents to replace the input JSON files, run the test with and without
        // the feature enabled and print the `actual_schema_string` by uncommenting the `println!`
        // towards the end of the test
        // ```
        // cargo t --features=casper-mainnet components::rpc_server::tests::schema -- --nocapture
        // cargo t --no-default-features components::rpc_server::tests::schema -- --nocapture
        // ```
        //
        // Note: Please review the diff of the input files to avoid any breaking changes.

        #[cfg(feature = "casper-mainnet")]
        let schema_path = format!(
            "{}/../resources/test/rpc_schema_hashing_V1.json",
            env!("CARGO_MANIFEST_DIR")
        );

        #[cfg(not(feature = "casper-mainnet"))]
        let schema_path = format!(
            "{}/../resources/test/rpc_schema_hashing_V2.json",
            env!("CARGO_MANIFEST_DIR")
        );

        let expected_schema = fs::read_to_string(schema_path).unwrap();
        let expected_schema: Value = serde_json::from_str(expected_schema.trim()).unwrap();

        let actual_schema = schema_for_value!(OPEN_RPC_SCHEMA.clone());
        let actual_schema_string = serde_json::to_string_pretty(&actual_schema).unwrap();
        let actual_schema: Value = serde_json::from_str(&actual_schema_string).unwrap();

        // println!("{}", actual_schema_string);

        assert_json_eq!(actual_schema, expected_schema);
    }
}
