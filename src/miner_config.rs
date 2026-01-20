use crate::{
	models::Algorithm, multi_block_state_client::ChainClientTrait, primitives::{AccountId, Hash}, Chain
};
use frame_support::pallet_prelude::ConstU32;
use pallet_election_provider_multi_block as multi_block;
use frame_election_provider_support::{self, SequentialPhragmen, PhragMMS};
use sp_runtime::{PerU16, Percent, Perbill};
use serde::Deserialize;
use parity_scale_codec::{Decode};
use sp_npos_elections;

/// Constants fetched from chain
#[derive(Debug, Clone)]
pub struct MinerConstants {
	pub pages: u32,
	pub max_winners_per_page: u32,
	pub max_backers_per_winner: u32,
	pub voter_snapshot_per_block: u32,
	pub target_snapshot_per_block: u32,
	pub max_length: u32,
}

#[derive(Decode, Deserialize, Debug)]
pub struct BlockLength {
	pub max: PerDispatchClass,
}

impl BlockLength {
	pub fn total(&self) -> u32 {
		let mut total = 0_u32;
		total = total.saturating_add(self.max.normal);
		total = total.saturating_add(self.max.operational);
		total = total.saturating_add(self.max.mandatory);
		total
	}
}

#[derive(Decode, Deserialize, Debug)]
pub struct PerDispatchClass {
	pub normal: u32,
	pub operational: u32,
	pub mandatory: u32,
}

/// Helper function to fetch constants from chain API
pub async fn fetch_constants<C: ChainClientTrait>(
	client: &C,
) -> Result<MinerConstants, Box<dyn std::error::Error>> {
	let pages = client
		.fetch_constant::<u32>("MultiBlockElection", "Pages")
		.await?;
	let max_winners_per_page = client
		.fetch_constant::<u32>("MultiBlockElectionVerifier", "MaxWinnersPerPage")
		.await
		.unwrap_or(256);
	let max_backers_per_winner = client
		.fetch_constant::<u32>("MultiBlockElectionVerifier", "MaxBackersPerWinner")
		.await
		.unwrap_or(u32::MAX);
	let voter_snapshot_per_block = client
		.fetch_constant::<u32>("MultiBlockElection", "VoterSnapshotPerBlock")
		.await
		.unwrap_or(100);
	let target_snapshot_per_block = client
		.fetch_constant::<u32>("MultiBlockElection", "TargetSnapshotPerBlock")
		.await
		.unwrap_or(100);

	let block_length = client
		.fetch_constant::<BlockLength>("System", "BlockLength")
		.await
		.unwrap_or(BlockLength { max: PerDispatchClass { normal: 1, operational: 2, mandatory: 3 } });

	let max_length = Percent::from_percent(75) * block_length.total();

	Ok(MinerConstants {
		pages,
		max_winners_per_page,
		max_backers_per_winner,
		voter_snapshot_per_block,
		target_snapshot_per_block,
		max_length,
	})
}

// Runtime configuration holder - stores values fetched from chain
use std::sync::{OnceLock, Mutex};
use tokio::task_local;

static RUNTIME_CONFIG: OnceLock<MinerConstants> = OnceLock::new();

// Task-local storage for balancing iterations (each async task gets its own value)
// This prevents race conditions when multiple requests run concurrently
#[derive(Debug, Clone)]
struct ElectionConfig {
	algorithm: Algorithm,
	iterations: usize,
	max_votes_per_voter: u32,
}
task_local! {
	static ELECTION_CONFIG: ElectionConfig;
}

// Global fallback for CLI usage (single-threaded, no concurrency issues)
static ELECTION_CONFIG_FALLBACK: Mutex<ElectionConfig> = Mutex::new(ElectionConfig {
	algorithm: Algorithm::SeqPhragmen,
	iterations: 0,
	max_votes_per_voter: 16,
});

/// Set the runtime miner constants (should be called once at startup)
pub fn set_runtime_constants(constants: MinerConstants) {
	RUNTIME_CONFIG.set(constants).expect("Runtime constants already set");
}

#[cfg(test)]
use std::sync::Once;
#[cfg(test)]
static INIT: Once = Once::new();

#[cfg(test)]
pub fn initialize_runtime_constants() {
	INIT.call_once(|| {
		// Ignore error if constants are already set (e.g., by another test)
		let _ = set_runtime_constants(MinerConstants {
			pages: 1,
			max_winners_per_page: 1,
			max_backers_per_winner: 1,
			voter_snapshot_per_block: 2,
			target_snapshot_per_block: 2,
			max_length: 100000000,
		});
	});
}

/// Set election algorithm, balancing iterations, and max nominations from args
/// 
/// Note: For concurrent API requests, use `with_election_config` instead
/// to ensure each request gets its own isolated value.
/// This function sets a global fallback value, which works for CLI usage.
pub fn set_election_config(chain: Chain, algorithm: Algorithm, iterations: usize, max_votes_per_voter: Option<u32>) {
	let max_votes_per_voter = if let Some(max_votes_per_voter) = max_votes_per_voter {
		max_votes_per_voter
	} else {
		match chain {
			Chain::Polkadot => 16,
			Chain::Kusama => 24,
			Chain::Substrate => 16,
		}
	};
	*ELECTION_CONFIG_FALLBACK.lock().unwrap() = ElectionConfig {
		algorithm,
		iterations,
		max_votes_per_voter,
	};
}

/// Run a future with a specific algorithm, balancing iterations, and max votes per voter set for this task.
pub async fn with_election_config<F, R>(chain: Chain, algorithm: Algorithm, iterations: usize, max_votes_per_voter: Option<u32>, f: F) -> R
where
	F: std::future::Future<Output = R>,
{
	let max_votes_per_voter = if let Some(max_votes_per_voter) = max_votes_per_voter {
		max_votes_per_voter
	} else {
		match chain {
			Chain::Polkadot => 16,
			Chain::Kusama => 24,
			Chain::Substrate => 16,
		}
	};
	ELECTION_CONFIG.scope(ElectionConfig {
		algorithm,
		iterations,
		max_votes_per_voter,
	}, f).await
}

/// Get the runtime miner constants
pub fn get_runtime_constants() -> &'static MinerConstants {
	RUNTIME_CONFIG.get().expect("Runtime constants not set - call set_runtime_constants first")
}

// Simple type aliases for constants 
pub struct Pages;
pub struct MaxWinnersPerPage;
pub struct MaxBackersPerWinner;
pub struct VoterSnapshotPerBlock;
pub struct TargetSnapshotPerBlock;
pub struct MaxLength;
pub struct BalancingIterations;
pub struct MaxVotesPerVoter;

// Dynamic solver wrapper that dispatches to the correct algorithm at runtime
#[derive(Clone, Debug)]
pub struct DynamicSolver;

// Helper to get current algorithm from task-local or fallback
pub fn get_current_algorithm() -> Algorithm {
	ELECTION_CONFIG.try_with(|v| v.algorithm)
		.unwrap_or_else(|_| ELECTION_CONFIG_FALLBACK.lock().unwrap().algorithm)
}

// Implement Get for constants
impl sp_core::Get<u32> for Pages {
	fn get() -> u32 { 
		get_runtime_constants().pages
	}
}

impl sp_core::Get<u32> for MaxWinnersPerPage {
	fn get() -> u32 { 
		get_runtime_constants().max_winners_per_page
	}
}

impl sp_core::Get<u32> for MaxBackersPerWinner {
	fn get() -> u32 { 
		get_runtime_constants().max_backers_per_winner
	}
}

impl sp_core::Get<u32> for VoterSnapshotPerBlock {
	fn get() -> u32 { 
		get_runtime_constants().voter_snapshot_per_block
	}
}

impl sp_core::Get<u32> for TargetSnapshotPerBlock {
	fn get() -> u32 { 
		get_runtime_constants().target_snapshot_per_block
	}
}

impl sp_core::Get<u32> for MaxLength {
	fn get() -> u32 { 
		get_runtime_constants().max_length
	}
}

impl sp_core::Get<u32> for MaxVotesPerVoter {
	fn get() -> u32 {
		// Try task-local first (for API requests), fall back to global (for CLI)
		// If not set in request, use the chain constant
		ELECTION_CONFIG.try_with(|v| v.max_votes_per_voter)
			.unwrap_or_else(|_| ELECTION_CONFIG_FALLBACK.lock().unwrap().max_votes_per_voter)
	}
}

impl sp_core::Get<Option<sp_npos_elections::BalancingConfig>> for BalancingIterations {
	fn get() -> Option<sp_npos_elections::BalancingConfig> {
		// Try task-local first (for API requests), fall back to global (for CLI)
		// This ensures each concurrent request gets its own value
		let iterations = ELECTION_CONFIG.try_with(|v| v.iterations)
			.unwrap_or_else(|_| ELECTION_CONFIG_FALLBACK.lock().unwrap().iterations);
		if iterations > 0 {
			Some(sp_npos_elections::BalancingConfig { iterations, tolerance: 0 })
		} else {
			None
		}
	}
}

// Implement NposSolver trait for DynamicSolver
// This allows it to dispatch to SequentialPhragmen or PhragMMS at runtime
impl frame_election_provider_support::NposSolver for DynamicSolver {
	type AccountId = AccountId;
	type Accuracy = Perbill;
	type Error = sp_npos_elections::Error;

	fn solve(
		to_elect: usize,
		targets: Vec<Self::AccountId>,
		voters: Vec<(
			Self::AccountId,
			frame_election_provider_support::VoteWeight,
			impl Clone + IntoIterator<Item = Self::AccountId>,
		)>,
	) -> Result<frame_election_provider_support::ElectionResult<Self::AccountId, Self::Accuracy>, Self::Error> {
		match get_current_algorithm() {
			Algorithm::SeqPhragmen => {
				SequentialPhragmen::<AccountId, Perbill, BalancingIterations>::solve(
					to_elect,
					targets,
					voters,
				)
			}
			Algorithm::Phragmms => {
				PhragMMS::<AccountId, Perbill, BalancingIterations>::solve(
					to_elect,
					targets,
					voters,
				)
			}
		}
	}

	fn weight<T: frame_election_provider_support::WeightInfo>(voters: u32, targets: u32, vote_degree: u32) -> frame_election_provider_support::Weight {
		match get_current_algorithm() {
			Algorithm::SeqPhragmen => {
				SequentialPhragmen::<AccountId, Perbill, BalancingIterations>::weight::<T>(voters, targets, vote_degree)
			}
			Algorithm::Phragmms => {
				PhragMMS::<AccountId, Perbill, BalancingIterations>::weight::<T>(voters, targets, vote_degree)
			}
		}
	}
}

pub mod polkadot {
	use super::*;

	frame_election_provider_support::generate_solution_type!(
		#[compact]
		pub struct NposSolution16::<
			VoterIndex = u32,
			TargetIndex = u16,
			Accuracy = PerU16,
			MaxVoters = ConstU32::<22500>
		>(16)
	);

	#[derive(Debug, Clone)]
	pub struct MinerConfig;

	impl multi_block::unsigned::miner::MinerConfig for MinerConfig {
		type AccountId = AccountId;
		type Solution = NposSolution16;
		type Solver = DynamicSolver;
		type Pages = Pages;
		type MaxVotesPerVoter = MaxVotesPerVoter;
		type MaxWinnersPerPage = MaxWinnersPerPage;
		type MaxBackersPerWinner = MaxBackersPerWinner;
		type MaxBackersPerWinnerFinal = ConstU32<{ u32::MAX }>;
		type VoterSnapshotPerBlock = VoterSnapshotPerBlock;
		type TargetSnapshotPerBlock = TargetSnapshotPerBlock;
		type MaxLength = MaxLength;
		type Hash = Hash;
	}
}

pub mod kusama {
	use super::*;

	frame_election_provider_support::generate_solution_type!(
		#[compact]
		pub struct NposSolution24::<
			VoterIndex = u32,
			TargetIndex = u16,
			Accuracy = PerU16,
			MaxVoters = ConstU32::<12500>
		>(24)
	);

	#[derive(Debug, Clone)]
	pub struct MinerConfig;

	impl multi_block::unsigned::miner::MinerConfig for MinerConfig {
		type AccountId = AccountId;
		type Solution = NposSolution24;
		type Solver = SequentialPhragmen<AccountId, Perbill, BalancingIterations>;
		type Pages = Pages;
		type MaxVotesPerVoter = MaxVotesPerVoter;
		type MaxWinnersPerPage = MaxWinnersPerPage;
		type MaxBackersPerWinner = MaxBackersPerWinner;
		type MaxBackersPerWinnerFinal = ConstU32<{ u32::MAX }>;
		type VoterSnapshotPerBlock = VoterSnapshotPerBlock;
		type TargetSnapshotPerBlock = TargetSnapshotPerBlock;
		type MaxLength = MaxLength;
		type Hash = Hash;
	}
}

pub mod substrate {
    use super::*;

    frame_election_provider_support::generate_solution_type!(
        #[compact]
        pub struct NposSolution16::<
            VoterIndex = u16,
            TargetIndex = u16,
            Accuracy = Percent,
            MaxVoters = ConstU32::<22500>
        >(16)
    );

    #[derive(Debug, Clone)]
    pub struct MinerConfig;

    impl multi_block::unsigned::miner::MinerConfig for MinerConfig {
        type AccountId = AccountId;
        type Solution = NposSolution16;
        type Solver = SequentialPhragmen<AccountId, Perbill, BalancingIterations>;
        type Pages = Pages;
        type MaxVotesPerVoter = MaxVotesPerVoter;
        type MaxWinnersPerPage = MaxWinnersPerPage;
        type MaxBackersPerWinner = MaxBackersPerWinner;
        type MaxBackersPerWinnerFinal = ConstU32<{ u32::MAX }>;
        type VoterSnapshotPerBlock = VoterSnapshotPerBlock;
        type TargetSnapshotPerBlock = TargetSnapshotPerBlock;
        type MaxLength = MaxLength;
        type Hash = Hash;
    }
}

/// Simple macro to select the appropriate MinerConfig based on chain
/// Usage: with_miner_config!(chain, { code that uses MinerConfig })
#[macro_export]
macro_rules! with_miner_config {
	($chain:expr, $code:block) => {
		match $chain {
			$crate::models::Chain::Polkadot => {
				use $crate::miner_config::polkadot::MinerConfig;
				$code
			},
			$crate::models::Chain::Kusama => {
				use $crate::miner_config::kusama::MinerConfig;
				$code
			},
            $crate::models::Chain::Substrate => {
                use $crate::miner_config::substrate::MinerConfig;
                $code
            },
		}
	};
}
