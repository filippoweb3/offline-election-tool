use axum::{
    extract::{Query, State}, http::StatusCode, response::Json
};
use pallet_election_provider_multi_block::unsigned::miner::MinerConfig;
use serde::{Deserialize, Serialize};

use crate::{
    api::{routes::root::AppState, utils}, miner_config, models::Algorithm, multi_block_state_client::StorageTrait, primitives::Storage, simulate::{Override, SimulateService}, snapshot::SnapshotService
};

#[derive(Deserialize)]
pub struct SimulateRequestQuery {
    pub block: Option<String>,
}

#[derive(Deserialize)]
pub struct SimulateRequestBody {
    pub algorithm: Option<Algorithm>,
    pub iterations: Option<usize>,
    pub reduce: Option<bool>,
    pub desired_validators: Option<u32>,
    pub max_nominations: Option<u32>,
    pub min_nominator_bond: Option<u128>,
    pub min_validator_bond: Option<u128>,
    pub manual_override: Option<Override>,
}

#[derive(Serialize)]
pub struct SimulateResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<crate::models::SimulationResultOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn simulate_handler<
Sim: SimulateService + Send + Sync + 'static,
Snap: SnapshotService<MC, S> + Send + Sync + 'static,
MC: MinerConfig + Send + Sync + Clone + 'static,
S: StorageTrait + From<Storage> + Clone + 'static,
>(
    State(state): State<AppState<
        Sim,
        Snap,
        MC,
        S,
    >>,
    Query(params): Query<SimulateRequestQuery>,
    Json(body): Json<SimulateRequestBody>,
) -> (StatusCode, Json<SimulateResponse>)
{
    let block = match utils::parse_block(params.block) {
        Ok(block) => block,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, Json(SimulateResponse {
                result: None,
                error: Some(e.to_string()),
            }));
        }
    };
    
    let algorithm = body.algorithm.unwrap_or(Algorithm::SeqPhragmen);
    let iterations = body.iterations.unwrap_or(0);
    let desired_validators = body.desired_validators;
    let max_nominations = body.max_nominations;
    let apply_reduce = body.reduce.unwrap_or(false);
    let min_nominator_bond = body.min_nominator_bond;
    let min_validator_bond = body.min_validator_bond;
    let manual_override = body.manual_override;
    
    let span = tracing::Span::current();
    let result = tokio::task::spawn_blocking(move || {
        // Maintain the same scope as the main function
        let _enter = span.enter();
        // Create a single-thread runtime for this OS thread
        let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();

        rt.block_on(async {
            // Run simulation within task-local scope for algorithm, iterations, and max nominations
            miner_config::with_election_config(algorithm, iterations, max_nominations, 
                async move {
                    state.simulate_service.simulate(
                        block,
                        desired_validators,
                        apply_reduce,
                        manual_override,
                        min_nominator_bond,
                        min_validator_bond,
                    ).await
                }
            ).await
        })
    }).await.unwrap();

    let (status, response) = match result {
        Ok(result) => {
            let output_result = result.to_output(state.chain);
            (
                StatusCode::OK,
                SimulateResponse {
                    result: Some(output_result),
                    error: None,
                }
            )
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            SimulateResponse {
                result: None,
                error: Some(e.to_string()),
            }
        ),
    };

    (status, Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::miner_config::polkadot::MinerConfig as PolkadotMinerConfig;
    use crate::simulate::MockSimulateService;
    use crate::snapshot::MockSnapshotService;
    use crate::models::Chain;
    use crate::models::{RunParameters, SimulationResult, StakingStats};
    use std::sync::Arc;

    #[tokio::test]
    async fn test_simulate_handler() {
        let mut simulate_service = MockSimulateService::new();
        simulate_service.expect_simulate().returning( move |_, _, _, _, _, _| {
            Ok(SimulationResult {
                run_parameters: RunParameters {
                    algorithm: Algorithm::SeqPhragmen,
                    iterations: 0,
                    reduce: false,
                    max_nominations: 0,
                    min_nominator_bond: 0,
                    min_validator_bond: 0,
                    desired_validators: 0,
                },
                active_validators: vec![],
                staking_stats: StakingStats {
                    total_staked: 0,
                    lowest_staked: 0,
                    avg_staked: 0,
                },
            })
        });
        let snapshot_service: MockSnapshotService<PolkadotMinerConfig, Storage> = MockSnapshotService::new();
        let app_state = AppState {
            simulate_service: Arc::new(simulate_service),
            snapshot_service: Arc::new(snapshot_service),
            chain: Chain::Polkadot,
            _phantom: std::marker::PhantomData,
        };
        let app_state_extract = State(app_state);
        let result = simulate_handler(app_state_extract, Query(SimulateRequestQuery { block: None }), Json(SimulateRequestBody { algorithm: None, iterations: None, reduce: None, desired_validators: None, max_nominations: None, min_nominator_bond: None, min_validator_bond: None, manual_override: None })).await;
        assert_eq!(result.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn test_simulate_handler_invalid_block() {
        let snapshot_service: MockSnapshotService<PolkadotMinerConfig, Storage> = MockSnapshotService::new();
        let app_state = AppState {
            simulate_service: Arc::new(MockSimulateService::new()),
            snapshot_service: Arc::new(snapshot_service),
            chain: Chain::Polkadot,
            _phantom: std::marker::PhantomData,
        };
        let app_state_extract = State(app_state);
        let result = simulate_handler(app_state_extract, Query(SimulateRequestQuery { block: Some("invalid".to_string()) }), Json(SimulateRequestBody { algorithm: None, iterations: None, reduce: None, desired_validators: None, max_nominations: None, min_nominator_bond: None, min_validator_bond: None, manual_override: None })).await;
        assert_eq!(result.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_simulate_handler_error() {
        let mut simulate_service = MockSimulateService::new();
        simulate_service.expect_simulate().returning( move |_, _, _, _, _, _| {
            Err(Box::new(
                std::io::Error::new(std::io::ErrorKind::Other, "Error")
            ))
        });
        let snapshot_service: MockSnapshotService<PolkadotMinerConfig, Storage> = MockSnapshotService::new();
        let app_state = AppState {
            simulate_service: Arc::new(simulate_service),
            snapshot_service: Arc::new(snapshot_service),
            chain: Chain::Polkadot,
            _phantom: std::marker::PhantomData,
        };
        let app_state_extract = State(app_state);
        let result = simulate_handler(app_state_extract, Query(SimulateRequestQuery { block: None }), Json(SimulateRequestBody { algorithm: None, iterations: None, reduce: None, desired_validators: None, max_nominations: None, min_nominator_bond: None, min_validator_bond: None, manual_override: None })).await;
        assert_eq!(result.0, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
