#![allow(clippy::let_unit_value)]
use super::{
    adapters::P2PAdapter,
    genesis::create_genesis_block,
};
use crate::{
    combined_database::CombinedDatabase,
    database::Database,
    fuel_core_graphql_api,
    fuel_core_graphql_api::Config as GraphQLConfig,
    schema::build_schema,
    service::{
        adapters::{
            BlockImporterAdapter,
            BlockProducerAdapter,
            ExecutorAdapter,
            MaybeRelayerAdapter,
            PoAAdapter,
            TxPoolAdapter,
            VerifierAdapter,
        },
        Config,
        SharedState,
        SubServices,
    },
};
use fuel_core_poa::Trigger;
use std::sync::Arc;
use tokio::sync::Mutex;

#[cfg(feature = "relayer")]
use crate::relayer::Config as RelayerConfig;
use crate::service::StaticGasPrice;
#[cfg(feature = "relayer")]
use fuel_core_types::blockchain::primitives::DaBlockHeight;

pub type PoAService =
    fuel_core_poa::Service<TxPoolAdapter, BlockProducerAdapter, BlockImporterAdapter>;
#[cfg(feature = "p2p")]
pub type P2PService = fuel_core_p2p::service::Service<Database>;
pub type TxPoolService = fuel_core_txpool::Service<P2PAdapter, Database, StaticGasPrice>;
pub type BlockProducerService = fuel_core_producer::block_producer::Producer<
    Database,
    TxPoolAdapter,
    ExecutorAdapter,
    StaticGasPrice,
>;
pub type GraphQL = fuel_core_graphql_api::api_service::Service;

pub fn init_sub_services(
    config: &Config,
    database: CombinedDatabase,
) -> anyhow::Result<(SubServices, SharedState)> {
    let last_block_header = database
        .on_chain()
        .get_current_block()?
        .map(|block| block.header().clone())
        .unwrap_or({
            let block = create_genesis_block(config);
            block.header().clone()
        });
    let last_height = *last_block_header.height();

    let executor = ExecutorAdapter::new(
        database.on_chain().clone(),
        database.relayer().clone(),
        fuel_core_upgradable_executor::config::Config {
            backtrace: config.vm.backtrace,
            utxo_validation_default: config.utxo_validation,
        },
    );

    let verifier = VerifierAdapter::new(config, database.on_chain().clone());

    let importer_adapter = BlockImporterAdapter::new(
        config.block_importer.clone(),
        database.on_chain().clone(),
        executor.clone(),
        verifier.clone(),
    );

    #[cfg(feature = "relayer")]
    let relayer_service = if let Some(config) = &config.relayer {
        Some(fuel_core_relayer::new_service(
            database.relayer().clone(),
            config.clone(),
        )?)
    } else {
        None
    };

    let relayer_adapter = MaybeRelayerAdapter {
        #[cfg(feature = "relayer")]
        relayer_synced: relayer_service.as_ref().map(|r| r.shared.clone()),
        #[cfg(feature = "relayer")]
        da_deploy_height: config.relayer.as_ref().map_or(
            DaBlockHeight(RelayerConfig::DEFAULT_DA_DEPLOY_HEIGHT),
            |config| config.da_deploy_height,
        ),
    };

    #[cfg(feature = "p2p")]
    let mut network = config.p2p.clone().map(|p2p_config| {
        let chain_config = config.snapshot_reader.chain_config();
        fuel_core_p2p::service::new_service(
            chain_config.consensus_parameters.chain_id(),
            p2p_config,
            database.on_chain().clone(),
            importer_adapter.clone(),
        )
    });

    #[cfg(feature = "p2p")]
    let p2p_adapter = {
        use crate::service::adapters::PeerReportConfig;

        // Hardcoded for now, but left here to be configurable in the future.
        // TODO: https://github.com/FuelLabs/fuel-core/issues/1340
        let peer_report_config = PeerReportConfig {
            successful_block_import: 5.,
            missing_block_headers: -100.,
            bad_block_header: -100.,
            missing_transactions: -100.,
            invalid_transactions: -100.,
        };
        P2PAdapter::new(
            network.as_ref().map(|network| network.shared.clone()),
            peer_report_config,
        )
    };

    #[cfg(not(feature = "p2p"))]
    let p2p_adapter = P2PAdapter::new();

    let gas_price_provider = StaticGasPrice::new(config.static_gas_price);
    let txpool = fuel_core_txpool::new_service(
        config.txpool.clone(),
        database.on_chain().clone(),
        importer_adapter.clone(),
        p2p_adapter.clone(),
        last_height,
        gas_price_provider.clone(),
    );
    let tx_pool_adapter = TxPoolAdapter::new(txpool.shared.clone());

    let block_producer = fuel_core_producer::Producer {
        config: config.block_producer.clone(),
        view_provider: database.on_chain().clone(),
        txpool: tx_pool_adapter.clone(),
        executor: Arc::new(executor),
        relayer: Box::new(relayer_adapter.clone()),
        lock: Mutex::new(()),
        gas_price_provider: gas_price_provider.clone(),
    };
    let producer_adapter = BlockProducerAdapter::new(block_producer);

    let poa_config: fuel_core_poa::Config = config.into();
    let mut production_enabled = !matches!(poa_config.trigger, Trigger::Never);

    if !production_enabled && config.debug {
        production_enabled = true;
        tracing::info!("Enabled manual block production because of `debug` flag");
    }

    let poa = (production_enabled).then(|| {
        fuel_core_poa::new_service(
            &last_block_header,
            poa_config,
            tx_pool_adapter.clone(),
            producer_adapter.clone(),
            importer_adapter.clone(),
            p2p_adapter.clone(),
        )
    });
    let poa_adapter = PoAAdapter::new(poa.as_ref().map(|service| service.shared.clone()));

    #[cfg(feature = "p2p")]
    let sync = fuel_core_sync::service::new_service(
        last_height,
        p2p_adapter.clone(),
        importer_adapter.clone(),
        super::adapters::ConsensusAdapter::new(
            verifier.clone(),
            config.relayer_consensus_config.clone(),
            relayer_adapter,
        ),
        config.sync,
    )?;

    // TODO: Figure out on how to move it into `fuel-core-graphql-api`.
    let chain_config = config.snapshot_reader.chain_config();
    let schema = crate::schema::dap::init(
        build_schema(),
        chain_config.consensus_parameters.clone(),
        config.debug,
    )
    .data(database.on_chain().clone());

    let graphql_worker = fuel_core_graphql_api::worker_service::new_service(
        tx_pool_adapter.clone(),
        importer_adapter.clone(),
        database.off_chain().clone(),
        chain_config.consensus_parameters.chain_id(),
    );

    let chain_config = config.snapshot_reader.chain_config();
    let graphql_config = GraphQLConfig {
        addr: config.addr,
        utxo_validation: config.utxo_validation,
        debug: config.debug,
        vm_backtrace: config.vm.backtrace,
        max_tx: config.txpool.max_tx,
        max_depth: config.txpool.max_depth,
        chain_name: chain_config.chain_name.clone(),
        consensus_parameters: chain_config.consensus_parameters.clone(),
        consensus_key: config.consensus_key.clone(),
    };

    let graph_ql = fuel_core_graphql_api::api_service::new_service(
        graphql_config,
        schema,
        database.on_chain().clone(),
        database.off_chain().clone(),
        Box::new(tx_pool_adapter),
        Box::new(producer_adapter),
        Box::new(poa_adapter.clone()),
        Box::new(p2p_adapter),
        Box::new(gas_price_provider),
        config.query_log_threshold_time,
        config.api_request_timeout,
    )?;

    let shared = SharedState {
        poa_adapter,
        txpool_shared_state: txpool.shared.clone(),
        #[cfg(feature = "p2p")]
        network: network.as_ref().map(|n| n.shared.clone()),
        #[cfg(feature = "relayer")]
        relayer: relayer_service.as_ref().map(|r| r.shared.clone()),
        graph_ql: graph_ql.shared.clone(),
        database,
        block_importer: importer_adapter,
        config: config.clone(),
    };

    #[allow(unused_mut)]
    // `FuelService` starts and shutdowns all sub-services in the `services` order
    let mut services: SubServices = vec![
        // GraphQL should be shutdown first, so let's start it first.
        Box::new(graph_ql),
        Box::new(txpool),
    ];

    if let Some(poa) = poa {
        services.push(Box::new(poa));
    }

    #[cfg(feature = "relayer")]
    if let Some(relayer) = relayer_service {
        services.push(Box::new(relayer));
    }

    #[cfg(feature = "p2p")]
    {
        if let Some(network) = network.take() {
            services.push(Box::new(network));
            services.push(Box::new(sync));
        }
    }

    services.push(Box::new(graphql_worker));

    Ok((services, shared))
}
