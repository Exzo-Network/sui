// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use anyhow::Result;
use axum::{
    response::IntoResponse,
    routing::{get, post},
    Extension, Json, Router,
};
use clap::Parser;
use http::{Method, StatusCode};
use std::{net::SocketAddr, sync::Arc};
use sui::sui_commands::genesis;
use sui_cluster_test::{
    cluster::{Cluster, LocalNewCluster},
    config::{ClusterTestOpt, Env},
    faucet::{FaucetClient, FaucetClientFactory},
};
use sui_config::{sui_cluster_test_config_dir, SUI_KEYSTORE_FILENAME, SUI_NETWORK_CONFIG};
use sui_faucet::{FaucetRequest, FixedAmountRequest};
use sui_keys::keystore::{AccountKeystore, FileBasedKeystore};
use sui_swarm_config::genesis_config::GenesisConfig;
use tower::ServiceBuilder;
use tower_http::cors::{Any, CorsLayer};

/// Start a Sui validator and fullnode for easy testing.
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Port to start the Fullnode RPC server on
    #[clap(long, default_value = "9000")]
    fullnode_rpc_port: u16,

    /// Port to start the Sui faucet on
    #[clap(long, default_value = "9123")]
    faucet_port: u16,

    /// Port to start the Indexer RPC server on
    #[clap(long, default_value = "9124")]
    indexer_rpc_port: u16,

    /// Port for the Indexer Postgres DB
    /// 5432 is the default port for postgres on Mac
    #[clap(long, default_value = "5432")]
    pg_port: u16,

    /// Hostname for the Indexer Postgres DB
    #[clap(long, default_value = "localhost")]
    pg_host: String,

    /// The duration for epochs (defaults to one minute)
    #[clap(long, default_value = "60000")]
    epoch_duration_ms: u64,

    /// if we should run indexer
    #[clap(long, takes_value = false)]
    pub with_indexer: bool,

    /// TODO(gegao): remove this after indexer migration is complete.
    #[clap(long)]
    pub use_indexer_experimental_methods: bool,

    /// If we run the local config with a persisted state.
    #[clap(long, takes_value = false)]
    pub with_persisted: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let (_guard, _filter_handle) = telemetry_subscribers::TelemetryConfig::new()
        .with_env()
        .init();

    let args = Args::parse();
    let Args {
        fullnode_rpc_port,
        indexer_rpc_port,
        pg_port,
        pg_host,
        epoch_duration_ms,
        faucet_port,
        with_indexer,
        use_indexer_experimental_methods,
        with_persisted,
    } = args;

    let genesis_config_option = if with_persisted {
        let cluster_config_network_config = sui_cluster_test_config_dir()?.join(SUI_NETWORK_CONFIG);
        // Auto genesis if path is none and sui directory doesn't exists.
        if !cluster_config_network_config.exists() {
            genesis(
                None,
                None,
                Some(sui_cluster_test_config_dir()?),
                false,
                None,
                None,
            )
            .await?;
        }

        let sui_cluster_config_dir = sui_cluster_test_config_dir()?;
        let keystore_path = sui_cluster_config_dir.join(SUI_KEYSTORE_FILENAME);
        let existing_keys = FileBasedKeystore::new(&keystore_path)?.addresses();
        Some(GenesisConfig::for_local_testing_with_addresses(
            existing_keys,
        ))
    } else {
        None
    };

    let cluster = LocalNewCluster::start(
        &ClusterTestOpt {
            env: Env::NewLocal,
            fullnode_address: Some(format!("127.0.0.1:{}", fullnode_rpc_port)),
            indexer_address: with_indexer.then_some(format!("127.0.0.1:{}", indexer_rpc_port)),
            pg_address: with_indexer.then_some(format!(
                "postgres://postgres@{pg_host}:{pg_port}/sui_indexer"
            )),
            faucet_address: None,
            epoch_duration_ms: Some(epoch_duration_ms),
            use_indexer_experimental_methods,
        },
        genesis_config_option,
    )
    .await?;

    println!("Fullnode RPC URL: {}", cluster.fullnode_url());

    if with_indexer {
        println!(
            "Indexer RPC URL: {}",
            cluster.indexer_url().clone().unwrap_or_default()
        );
    }

    start_faucet(&cluster, faucet_port).await?;

    Ok(())
}

struct AppState {
    faucet: Arc<dyn FaucetClient + Sync + Send>,
}

async fn start_faucet(cluster: &LocalNewCluster, port: u16) -> Result<()> {
    let faucet = FaucetClientFactory::new_from_cluster(cluster).await;

    let app_state = Arc::new(AppState { faucet });

    let cors = CorsLayer::new()
        .allow_methods(vec![Method::GET, Method::POST])
        .allow_headers(Any)
        .allow_origin(Any);

    let app = Router::new()
        .route("/", get(health))
        .route("/gas", post(faucet_request))
        .layer(
            ServiceBuilder::new()
                .layer(cors)
                .layer(Extension(app_state))
                .into_inner(),
        );

    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    println!("Faucet URL: http://{}", addr);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

/// basic handler that responds with a static string
async fn health() -> &'static str {
    "OK"
}

async fn faucet_request(
    Extension(state): Extension<Arc<AppState>>,
    Json(payload): Json<FaucetRequest>,
) -> impl IntoResponse {
    let result = match payload {
        FaucetRequest::FixedAmountRequest(FixedAmountRequest { recipient }) => {
            state.faucet.request_sui_coins(recipient).await
        }
    };

    if !result.transferred_gas_objects.is_empty() {
        (StatusCode::CREATED, Json(result))
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, Json(result))
    }
}
