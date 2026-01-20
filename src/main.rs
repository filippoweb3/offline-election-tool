use tracing::info;
use sp_core::{H256};
use clap::{arg, command, Parser, Subcommand};
use sp_core::crypto::set_default_ss58_version;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use crate::api::routes::root;
use crate::simulate::{SimulateService, SimulateServiceImpl};
use crate::snapshot::{SnapshotService, SnapshotServiceImpl};
use crate::models::{Chain, Algorithm};
use crate::multi_block_state_client::{MultiBlockClient};
use crate::primitives::Storage;
use crate::raw_state_client::RawClientTrait;
use crate::subxt_client::Client;

mod raw_state_client;
mod primitives;
mod snapshot;
mod models;
mod simulate;
mod api;
mod subxt_client;
mod multi_block_state_client;
mod miner_config;

#[derive(Parser, Debug)]
pub struct SimulateArgs {
    /// Block with Snapshot (Signed or Unsigned phase) 
    #[arg(short, long, default_value = "latest")]
    pub block: String,

    /// Election algorithm to use (seq-phragmen or phragmms)
    #[arg(short, long, default_value = "seq-phragmen")]
    pub algorithm: Algorithm,

    /// Number of iterations for the balancing algorithm
    #[arg(short, long, default_value = "0")]
    pub iterations: usize,

    /// Apply reduce algorithm to output assignments
    #[arg(long)]
    pub reduce: bool,

    /// Desired number of validators to elect (optional, uses chain default if not specified)
    #[arg(long)]
    pub desired_validators: Option<u32>,

    /// Maximum nominations per voter (optional, uses chain default if not specified)
    #[arg(long)]
    pub max_nominations: Option<u32>,

    /// Minimum nominator bond (optional, uses chain default if not specified)
    #[arg(long)]
    pub min_nominator_bond: Option<u128>,

    /// Minimum validator bond (optional, uses chain default if not specified)
    #[arg(long)]
    pub min_validator_bond: Option<u128>,

    /// Output file path (if not specified, prints to stdout)
    #[arg(short, long, default_value = "simulate.json")]
    pub output: String,

    /// Manual override JSON file path for voters and candidates
    #[arg(short = 'm', long)]
    pub manual_override: Option<String>,
}

#[derive(Parser, Debug)]
pub struct SnapshotArgs {
    /// Block with Snapshot (Signed or Unsigned phase) 
    #[arg(short, long, default_value = "latest")]
    pub block: String,

    /// Output file path (if not specified, prints to stdout)
    #[arg(short, long, default_value = "snapshot.json")]
    pub output: String,
}

#[derive(Subcommand, Debug)]
enum Action {
    /// Simulate the election using the specified algorithm (seq_phragmen or phragmms)
    Simulate(SimulateArgs),
    /// Retrieve actual snapshot containing validator candidates and their voters
    Snapshot(SnapshotArgs),

    /// Start REST API server
    Server {
        /// Server address to bind to
        #[arg(short, long, default_value = "127.0.0.1:3000")]
        address: String,
    },
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// RPC endpoint URL (must be aligned with the chain)
    #[arg(short, long)]
    rpc_endpoint: String,

    #[command(subcommand)]
    action: Action,
}

fn write_output<T: serde::Serialize>(data: &T, file_path: String) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(data)?;
    if file_path != "-" {
        let mut file = File::create(file_path)?;
        file.write_all(json.as_bytes())?;
    } else {
        println!("{}", json);
    }
    Ok(())
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing for all commands
    // Use INFO level for CLI commands, DEBUG level for server
    let args = Args::parse();
    
    let log_level = if matches!(args.action, Action::Server { .. }) {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    
    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .init();

    let raw_client = raw_state_client::RawClient::new(&args.rpc_endpoint).await?;
    let subxt_client = subxt_client::Client::new(&args.rpc_endpoint).await?;
    
    let runtime_version = raw_client.get_runtime_version().await?;
    let chain = match runtime_version.spec_name.to_string().as_str() {
        "polkadot" => Chain::Polkadot,
        "kusama" => Chain::Kusama,
        "substrate" => Chain::Substrate,
        "statemint" => Chain::Polkadot,
        "statemine" => Chain::Kusama,
        _ => return Err("Unsupported chain".into()),
    };

    set_default_ss58_version(chain.ss58_address_format());

    // Fetch all constants from chain API
    let miner_constants = miner_config::fetch_constants(&subxt_client).await?;
    info!("Fetched constants: pages={}, max_winners_per_page={}, max_backers_per_winner={}, voter_snapshot_per_block={}, target_snapshot_per_block={}, max_length={}",
        miner_constants.pages,
        miner_constants.max_winners_per_page,
        miner_constants.max_backers_per_winner,
        miner_constants.voter_snapshot_per_block,
        miner_constants.target_snapshot_per_block,
        miner_constants.max_length,
    );
    
    // Set runtime constants
    miner_config::set_runtime_constants(miner_constants.clone());

    match args.action {
        Action::Simulate(simulate_args) => {
            let block: Option<H256> = if simulate_args.block == "latest" {
                None
            } else {
                Some(simulate_args.block.parse().unwrap())
            };

            let output = simulate_args.output.clone();
            info!("Running election simulation with {:?} algorithm...", simulate_args.algorithm);
            let desired_validators = simulate_args.desired_validators;
            let algorithm = simulate_args.algorithm;
            let iterations = simulate_args.iterations;
            let max_nominations = simulate_args.max_nominations;
            miner_config::set_election_config(chain, algorithm, iterations, max_nominations);
            let apply_reduce = simulate_args.reduce;
            let manual_override = if let Some(path) = simulate_args.manual_override.clone() {
                let file = std::fs::read(&path)
                    .map_err(|e| format!("Failed to read manual override file '{}': {}", path, e))?;
                let override_data: simulate::Override = serde_json::from_slice(&file)
                    .map_err(|e| format!("Failed to parse manual override JSON: {}", e))?;
                Some(override_data)
            } else {
                None
            };
            let min_nominator_bond = simulate_args.min_nominator_bond;
            let min_validator_bond = simulate_args.min_validator_bond;
            
            let election_result = with_miner_config!(chain, {
                let multi_block_client = Arc::new(MultiBlockClient::<Client, MinerConfig, Storage>::new(subxt_client.clone()));
                let raw_client_arc = Arc::new(raw_client);             
                let snapshot_service = Arc::new(SnapshotServiceImpl::new(multi_block_client.clone(), raw_client_arc.clone()));
                let simulate_service = SimulateServiceImpl::new(multi_block_client.clone(), snapshot_service.clone());               
                
                simulate_service.simulate(block, desired_validators, apply_reduce, manual_override, min_nominator_bond, min_validator_bond).await
            });
            if election_result.is_err() {  
                return Err(format!("Error in election simulation -> {}", election_result.err().unwrap()).into());
            }
            let result = election_result.unwrap();
            let output_result = result.to_output(chain);
            write_output(&output_result, output)?;
        }
        Action::Snapshot(snapshot_args) => {
            let block: Option<H256> = if snapshot_args.block == "latest" {
                None
            } else {
                Some(snapshot_args.block.parse().unwrap())
            };

            info!("Taking snapshot...");
            let snapshot = with_miner_config!(chain, {
                let multi_block_client = MultiBlockClient::<Client, MinerConfig, Storage>::new(subxt_client.clone());
                let snapshot_service = SnapshotServiceImpl::new(Arc::new(multi_block_client), Arc::new(raw_client));
                snapshot_service.build(block).await
            });
            if snapshot.is_err() {
                return Err(format!("Error generating snapshot -> {}", snapshot.err().unwrap()).into());
            }
            let snapshot = snapshot.unwrap();
            let output_snapshot = snapshot.to_output(chain);
            write_output(&output_snapshot, snapshot_args.output)?;
        }
        Action::Server { address } => {
            info!("Starting server on {}", address);
            let listener = tokio::net::TcpListener::bind(address).await?;
            with_miner_config!(chain, {
                let multi_block_client = Arc::new(MultiBlockClient::<Client, MinerConfig, Storage>::new(subxt_client.clone()));
                let raw_client_arc = Arc::new(raw_client);
                let snapshot_service = Arc::new(SnapshotServiceImpl::new(multi_block_client.clone(), raw_client_arc.clone()));
                let simulate_service = Arc::new(SimulateServiceImpl::new(multi_block_client.clone(), snapshot_service.clone()));
                let router = root::routes(simulate_service, snapshot_service, chain);
                axum::serve(listener, router)
                    .await
                    .unwrap_or_else(|e| panic!("Error starting server: {}", e));
            });
        }
    }
    Ok(())
}
