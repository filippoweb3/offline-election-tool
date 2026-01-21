use mockall::automock;
use subxt::utils::Yes;
use subxt::storage::Address;
use crate::{primitives::Storage, subxt_client::Client};
use crate::raw_state_client::{NominationsLight, StakingLedger};
use pallet_staking::ValidatorPrefs;
use parity_scale_codec::{Decode, Encode};
use parity_scale_codec as codec;
use frame_support::BoundedVec;
use frame_election_provider_support::Voter;
use pallet_election_provider_multi_block::{unsigned::miner::MinerConfig};
use sp_core::Get;
use subxt::dynamic::Value;

use crate::primitives::{AccountId, Hash};
use subxt::ext::{scale_value};
use std::marker::PhantomData;

// Trait for chain client operations to enable dependency injection for testing
#[automock]
#[async_trait::async_trait]
pub trait ChainClientTrait: Send + Sync {
    async fn get_storage(&self, block: Option<Hash>) -> Result<Storage, Box<dyn std::error::Error + Send + Sync>>;
    async fn fetch_constant<T: serde::de::DeserializeOwned>(
        &self,
        pallet: &str,
        constant_name: &str,
    ) -> Result<T, Box<dyn std::error::Error>>
    where
        T: 'static;
}

// Implementation of ChainClientTrait for Client
#[async_trait::async_trait]
impl ChainClientTrait for Client {
    async fn get_storage(&self, block: Option<Hash>) -> Result<Storage, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(block) = block {
            Ok(self.chain_api().storage().at(block))
        } else {
            Ok(self.chain_api().storage().at_latest().await?)
        }
    }

    async fn fetch_constant<T: serde::de::DeserializeOwned>(
        &self,
        pallet: &str,
        constant_name: &str,
    ) -> Result<T, Box<dyn std::error::Error>> {
        // Call the inherent method on Client using fully qualified syntax to avoid recursion
        crate::subxt_client::Client::fetch_constant(self, pallet, constant_name).await
    }
}

// Trait to abstract over storage access so we can mock it in tests
#[async_trait::async_trait]
pub trait StorageTrait: Send + Sync {
    async fn fetch<Addr>(
        &self,
        address: &Addr,
    ) -> Result<Option<<Addr as Address>::Target>, Box<dyn std::error::Error + Send + Sync>>
    where
        Addr: Address<IsFetchable = Yes> + Sync + 'static;

    async fn fetch_or_default<Addr>(
        &self,
        address: &Addr,
    ) -> Result<<Addr as Address>::Target, Box<dyn std::error::Error + Send + Sync>>
    where
        Addr: Address<IsFetchable = Yes, IsDefaultable = Yes> + Sync + 'static;
}

#[async_trait::async_trait]
impl StorageTrait for Storage {
    async fn fetch<Addr>(
        &self,
        address: &Addr,
    ) -> Result<Option<<Addr as Address>::Target>, Box<dyn std::error::Error + Send + Sync>>
    where
        Addr: Address<IsFetchable = Yes> + Sync + 'static,
    {
        let val = Storage::fetch(self, address).await?;
        Ok(val)
    }

    async fn fetch_or_default<Addr>(
        &self,
        address: &Addr,
    ) -> Result<<Addr as Address>::Target, Box<dyn std::error::Error + Send + Sync>>
    where
        Addr: Address<IsFetchable = Yes, IsDefaultable = Yes> + Sync + 'static,
    {
        let val = Storage::fetch_or_default(self, address).await?;
        Ok(val)
    }
}

/// Phase enum matching the structure from pallet_election_provider_multi_block
#[derive(Debug, Clone, Copy, PartialEq, Eq, Decode, Encode)]
pub enum Phase {
	/// Nothing is happening, but it might.
	Off,
	/// Signed phase is open. The inner value is the number of blocks left in this phase.
	Signed(u32),
	/// We are validating results. The inner value is the number of blocks left in this phase.
	SignedValidation(u32),
	/// Unsigned phase. The inner value is the number of blocks left in this phase.
	Unsigned(u32),
	/// Snapshot is being created. The inner value is the remaining number of pages left to be fetched.
	Snapshot(u32),
	/// Snapshot is done, and we are waiting for `Export` to kick in.
	Done,
	/// Exporting has begun, and the given page was the last one received.
	Export(u32),
	/// The emergency phase. This locks the pallet such that only governance can change the state.
	Emergency,
}

impl Phase {
	/// Check if snapshots are available in this phase.
	/// 
	/// Snapshots are available in:
	/// - `Done` - snapshot is done, waiting for export
	/// - `Signed` - signed phase is open
	/// - `SignedValidation` - validating signed results
	/// - `Unsigned` - unsigned phase is open
	/// - `Export` - exporting has begun (snapshot still available but solutions no longer accepted)
	/// 
	/// Snapshots are NOT available in:
	/// - `Off` - election hasn't started
	/// - `Snapshot(n)` - snapshot is still being created
	/// - `Emergency` - emergency phase locks the pallet
	pub fn has_snapshot(&self) -> bool {
		match self {
			Phase::Done => true,
			Phase::Signed(_) => true,
			Phase::SignedValidation(_) => true,
			Phase::Unsigned(_) => true,
			Phase::Export(_) => true,
			Phase::Snapshot(_) => false,
			Phase::Off => false,
			Phase::Emergency => false,
		}
	}
}

// Generic voter type for use with MinerConfig
pub type VoterData<MC> = Voter<AccountId, <MC as MinerConfig>::MaxVotesPerVoter>;

pub type VoterSnapshotPage<MC> = 
	BoundedVec<VoterData<MC>, <MC as MinerConfig>::VoterSnapshotPerBlock>;

// Type aliases for snapshot pages using MinerConfig
pub type TargetSnapshotPage<MC> =
	BoundedVec<AccountId, <MC as MinerConfig>::TargetSnapshotPerBlock>;

pub struct ElectionSnapshotPage<MC: MinerConfig> {
	pub voters: Vec<VoterSnapshotPage<MC>>,
	pub targets: TargetSnapshotPage<MC>,
}

#[derive(Debug, Clone, Decode, Encode)]
pub struct ListBag {
    pub head: Option<AccountId>,
    pub tail: Option<AccountId>,
    // Note: bag_upper is the storage key, not stored in the value
    // _phantom is zero-sized and skipped by codec
}

#[derive(Debug, Clone, Decode, Encode)]
pub struct ListNode {
    pub id: AccountId,
    pub prev: Option<AccountId>,
    pub next: Option<AccountId>,
}
#[automock]
#[async_trait::async_trait]
pub trait MultiBlockClientTrait<C: ChainClientTrait + Send + Sync + 'static, MC: MinerConfig + Send + Sync + 'static, S: StorageTrait + From<Storage> + 'static> {
    async fn get_storage(&self, block: Option<Hash>) -> Result<S, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_block_details(&self, block: Option<Hash>) -> Result<BlockDetails<S>, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_phase(&self, storage: &S) -> Result<Phase, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_round(&self, storage: &S) -> Result<u32, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_desired_targets(&self, storage: &S, round: u32) -> Result<u32, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_block_number(&self, storage: &S) -> Result<u32, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_min_nominator_bond(&self, storage: &S) -> Result<u128, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_min_validator_bond(&self, storage: &S) -> Result<u128, Box<dyn std::error::Error + Send + Sync>>;
    async fn fetch_paged_voter_snapshot(&self, storage: &S, round: u32, page: u32) -> Result<VoterSnapshotPage<MC>, Box<dyn std::error::Error + Send + Sync>>;
    async fn fetch_paged_target_snapshot(&self, storage: &S, round: u32, page: u32) -> Result<TargetSnapshotPage<MC>, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_validator_prefs(&self, storage: &S, validator: AccountId) -> Result<ValidatorPrefs, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_nominator(&self, storage: &S, nominator: AccountId) -> Result<Option<NominationsLight<AccountId>>, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_controller_from_stash(&self, storage: &S, stash: AccountId) -> Result<Option<AccountId>, Box<dyn std::error::Error + Send + Sync>>;
    async fn ledger(&self, storage: &S, account: AccountId) -> Result<Option<StakingLedger>, Box<dyn std::error::Error + Send + Sync>>;
    async fn list_bags(&self, storage: &S, index: u64) -> Result<Option<ListBag>, Box<dyn std::error::Error + Send + Sync>>;
    async fn list_nodes(&self, storage: &S, account: AccountId) -> Result<Option<ListNode>, Box<dyn std::error::Error + Send + Sync>>;
}

pub struct MultiBlockClient<C: ChainClientTrait + Send + Sync + 'static, MC: MinerConfig + Send + Sync + 'static, S: StorageTrait + From<Storage> + 'static> {
    client: C,
    _phantom: PhantomData<(MC, S)>,
}

impl<MC: MinerConfig + Send + Sync + 'static> MultiBlockClient<Client, MC, Storage> {
    pub fn new(client: Client) -> Self {
        Self { client, _phantom: PhantomData }
    }
}

#[async_trait::async_trait]
impl<C: ChainClientTrait + Send + Sync + 'static, MC: MinerConfig + Send + Sync + 'static, S: StorageTrait + From<Storage> + Send + Sync + 'static> MultiBlockClientTrait<C, MC, S> for MultiBlockClient<C, MC, S> {
    async fn get_storage(&self, block: Option<Hash>) -> Result<S, Box<dyn std::error::Error + Send + Sync>> {
        let storage = self.client.get_storage(block).await?;
        Ok(S::from(storage))
    }

    // Get block-specific details for a given block.
    async fn get_block_details(&self, block: Option<Hash>) -> Result<BlockDetails<S>, Box<dyn std::error::Error + Send + Sync>> {
        let storage: S = self.get_storage(block).await?;
		let phase = self.get_phase(&storage).await?;
        let round = self.get_round(&storage).await?;
        let desired_targets_result = self.get_desired_targets(&storage, round).await;
        let desired_targets = match desired_targets_result {
            Ok(desired_targets) => desired_targets,
            Err(_) => {
                tracing::warn!("Desired targets not found, using default of 600");
                600
            }
        };
		let n_pages = MC::Pages::get();
		let block_number = self.get_block_number(&storage).await?;
		let block_hash = block;
        Ok(BlockDetails::<S> { 
			storage: storage.into(), 
			phase, 
			n_pages, 
			round, 
			desired_targets, 
			_block_number: block_number,
			block_hash,
		})
    }

    async fn get_phase(&self, storage: &S) -> Result<Phase, Box<dyn std::error::Error + Send + Sync>> {
        let phase_key = subxt::dynamic::storage("MultiBlockElection", "CurrentPhase", vec![]);
        let phase = storage.fetch_or_default(&phase_key).await?;
        let phase: Phase = codec::Decode::decode(&mut phase.encoded())?;
        Ok(phase)
    }

    async fn get_round(&self, storage: &S) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        let storage_key = subxt::dynamic::storage("MultiBlockElection", "Round", vec![]);
        let round = storage.fetch_or_default(&storage_key).await?;
        let round: u32 = codec::Decode::decode(&mut round.encoded())?;
        Ok(round)
    }

    async fn get_desired_targets(&self, storage: &S, round: u32) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        let storage_key = subxt::dynamic::storage(
            "MultiBlockElection",
            "DesiredTargets",
            vec![Value::from(round)],
        );
        let desired_targets_entry = storage
            .fetch(&storage_key)
            .await?
            .ok_or("DesiredTargets not found")?;
        let desired_targets: u32 = codec::Decode::decode(&mut desired_targets_entry.encoded())?;
        Ok(desired_targets)
    }

    async fn get_block_number(&self, storage: &S) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
        let storage_key = subxt::dynamic::storage("System", "Number", vec![]);
        let block_number_entry = storage.fetch(&storage_key)
            .await?
            .ok_or("Block number not found")?;
        let block_number: u32 = codec::Decode::decode(&mut block_number_entry.encoded())?;
        Ok(block_number)
    }

    async fn get_min_nominator_bond(&self, storage: &S) -> Result<u128, Box<dyn std::error::Error + Send + Sync>> {
        let storage_key = subxt::dynamic::storage("Staking", "MinNominatorBond", vec![]);
        let min_nominator_bond_entry = storage.fetch(&storage_key)
            .await?
            .ok_or("MinNominatorBond not found")?;
        let min_nominator_bond: u128 = codec::Decode::decode(&mut min_nominator_bond_entry.encoded())?;
        Ok(min_nominator_bond)
    }

    async fn get_min_validator_bond(&self, storage: &S) -> Result<u128, Box<dyn std::error::Error + Send + Sync>> {
        let storage_key = subxt::dynamic::storage("Staking", "MinValidatorBond", vec![]);
        let min_validator_bond_entry = storage.fetch(&storage_key)
            .await?
            .ok_or("MinValidatorBond not found")?;
        let min_validator_bond: u128 = codec::Decode::decode(&mut min_validator_bond_entry.encoded())?;
        Ok(min_validator_bond)
    }

    async fn fetch_paged_voter_snapshot(&self, storage: &S, round: u32, page: u32) -> Result<VoterSnapshotPage<MC>, Box<dyn std::error::Error + Send + Sync>> {
        let storage_key = subxt::dynamic::storage(
            "MultiBlockElection",
            "PagedVoterSnapshot",
            vec![Value::from(round), Value::from(page)],
        );
        let voter_snapshot_entry = storage.fetch(&storage_key)
            .await?
            .ok_or("Voter snapshot not found")?;

        let voter_snapshot: VoterSnapshotPage<MC> = codec::Decode::decode(&mut voter_snapshot_entry.encoded())?;

        Ok(voter_snapshot)
    }

    async fn fetch_paged_target_snapshot(&self, storage: &S, round: u32, page: u32) -> Result<TargetSnapshotPage<MC>, Box<dyn std::error::Error + Send + Sync>> {
        let storage_key = subxt::dynamic::storage(
            "MultiBlockElection",
            "PagedTargetSnapshot",
            vec![Value::from(round), Value::from(page)],
        );
        let target_snapshot_entry = storage.fetch(&storage_key)
            .await?
            .ok_or("Target snapshot not found")?;
        let target_snapshot: TargetSnapshotPage<MC> = codec::Decode::decode(&mut target_snapshot_entry.encoded())?;
        Ok(target_snapshot)
    }
    
    async fn get_validator_prefs(&self, storage: &S, validator: AccountId) -> Result<ValidatorPrefs, Box<dyn std::error::Error + Send + Sync>> {
        let encoded_validator = validator.encode();
        let storage_key = subxt::dynamic::storage("Staking", "Validators", vec![scale_value::Value::from(encoded_validator)]);
        let validator_prefs_entry = storage.fetch(&storage_key)
            .await?
            .ok_or("ValidatorPrefs not found")?;
        let validator_prefs: ValidatorPrefs = codec::Decode::decode(&mut validator_prefs_entry.encoded())?;
        Ok(validator_prefs)
    }

    async fn get_nominator(&self, storage: &S, nominator: AccountId) -> Result<Option<NominationsLight<AccountId>>, Box<dyn std::error::Error + Send + Sync>> {
        let encoded_nominator = nominator.encode();
        let storage_key = subxt::dynamic::storage("Staking", "Nominators", vec![scale_value::Value::from(encoded_nominator)]);
        match storage.fetch(&storage_key).await? {
            Some(entry) => {
                let nominations: NominationsLight<AccountId> = codec::Decode::decode(&mut entry.encoded())?;
                Ok(Some(nominations))
            }
            None => Ok(None),
        }
    }

    // Get controller account for a given stash account
    async fn get_controller_from_stash(&self, storage: &S, stash: AccountId) -> Result<Option<AccountId>, Box<dyn std::error::Error + Send + Sync>> {
        let encoded_stash = stash.encode();
        let storage_key = subxt::dynamic::storage("Staking", "Bonded", vec![scale_value::Value::from(encoded_stash)]);
        match storage.fetch(&storage_key).await? {
            Some(entry) => {
                let controller: AccountId = codec::Decode::decode(&mut entry.encoded())?;
                Ok(Some(controller))
            }
            None => Ok(None),
        }
    }

    async fn ledger(&self, storage: &S, account: AccountId) -> Result<Option<StakingLedger>, Box<dyn std::error::Error + Send + Sync>> {
        let encoded_account = account.encode();
        let storage_key = subxt::dynamic::storage("Staking", "Ledger", vec![scale_value::Value::from(encoded_account)]);
        match storage.fetch(&storage_key).await? {
            Some(entry) => {
                let ledger: StakingLedger = codec::Decode::decode(&mut entry.encoded())?;
                Ok(Some(ledger))
            }
            None => Ok(None),
        }
    }

    async fn list_bags(&self, storage: &S, index: u64) -> Result<Option<ListBag>, Box<dyn std::error::Error + Send + Sync>> {
        let storage_key = subxt::dynamic::storage("VoterList", "ListBags", vec![Value::from(index)]);
        let bags_entry = storage.fetch(&storage_key).await?;
        match bags_entry {
            Some(entry) => {
                let bags: ListBag = codec::Decode::decode(&mut entry.encoded())?;
                Ok(Some(bags))
            }
            None => Ok(None),
        }
    }
    async fn list_nodes(&self, storage: &S, account: AccountId) -> Result<Option<ListNode>, Box<dyn std::error::Error + Send + Sync>> {
        let encoded_account = account.encode();
        let storage_key = subxt::dynamic::storage("VoterList", "ListNodes", vec![Value::from(encoded_account)]);
        let nodes_entry = storage.fetch(&storage_key).await?;
        match nodes_entry {
            Some(entry) => {
                let nodes: ListNode = codec::Decode::decode(&mut entry.encoded())?;
                Ok(Some(nodes))
            }
            None => Ok(None),
        }
    }
}

/// Block-specific details for a given block.
/// Contains the storage snapshot and metadata for that specific block.
/// Created via `MultiBlockClient::get_block_details()`.
#[derive(Debug, Clone)]
pub struct BlockDetails<S: StorageTrait> {
	pub storage: S,
	pub phase: Phase,
	pub n_pages: u32,
	pub round: u32,
	pub desired_targets: u32,
	pub _block_number: u32,
	pub block_hash: Option<Hash>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::mock;
    use mockall::predicate::*;
    use sp_runtime::Perbill;
    use subxt::dynamic::DecodedValueThunk;
    use subxt::metadata::{DecodeWithMetadata};

    mock! {
        #[derive(Debug, Clone)]
        pub DummyStorage {}

        #[async_trait::async_trait]
        impl StorageTrait for DummyStorage {
            async fn fetch<Addr>(
                &self,
                address: &Addr,
            ) -> Result<Option<<Addr as Address>::Target>, Box<dyn std::error::Error + Send + Sync>>
            where
                Addr: Address<IsFetchable = Yes> + Sync + 'static;

            async fn fetch_or_default<Addr>(
                &self,
                address: &Addr,
            ) -> Result<<Addr as Address>::Target, Box<dyn std::error::Error + Send + Sync>>
            where
                Addr: Address<IsFetchable = Yes, IsDefaultable = Yes> + Sync + 'static;
        }
    }

    impl From<Storage> for MockDummyStorage {
        fn from(_storage: Storage) -> Self {
            MockDummyStorage::new()
        }
    }

    pub fn fake_value_thunk_from<T: codec::Encode>(val: T) -> DecodedValueThunk {
        let metadata = dummy_metadata();
        let type_id = 0;
    
        let bytes = val.encode();
        let mut slice: &[u8] = &bytes;
    
        DecodedValueThunk::decode_with_metadata(&mut slice, type_id, &metadata).unwrap()
    }
    
    pub fn dummy_metadata() -> subxt::Metadata {
        let bytes = include_bytes!("../metadata/multi_block.scale");
        let mut slice: &[u8] = bytes;
        subxt::Metadata::decode(&mut slice).unwrap()
    }

    use crate::miner_config::polkadot::MinerConfig as PolkadotMinerConfig;
    use crate::primitives::Balance;
    use crate::raw_state_client::UnlockChunk;

    use crate::miner_config::initialize_runtime_constants;

    #[tokio::test]
    async fn test_get_phase() {
        let mut dummy_storage = MockDummyStorage::new();
        let address = subxt::dynamic::storage("MultiBlockElection", "CurrentPhase", vec![]);
        dummy_storage
            .expect_fetch_or_default()
            .with(eq(address.clone()))
            .returning(|_address| {
                let phase = Phase::Signed(10);
                let value = fake_value_thunk_from(phase);
                Ok(value)
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let phase = client.get_phase(&dummy_storage).await;
        assert_eq!(phase.unwrap(), Phase::Signed(10));
    }

    #[tokio::test]
    async fn test_get_round() {
        let mut dummy_storage = MockDummyStorage::new();
        let address = subxt::dynamic::storage("MultiBlockElection", "Round", vec![]);
        dummy_storage
            .expect_fetch_or_default()
            .with(eq(address.clone()))
            .returning(|_address| {
                let round = 10;
                let value = fake_value_thunk_from(round);
                Ok(value)
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let round = client.get_round(&dummy_storage).await;
        assert_eq!(round.unwrap(), 10);
    }

    #[tokio::test]
    async fn test_get_desired_targets() {
        let mut dummy_storage = MockDummyStorage::new();
        let round = 10;
        let address = subxt::dynamic::storage("MultiBlockElection", "DesiredTargets", vec![Value::from(round)]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let desired_targets = 10;
                let value = fake_value_thunk_from(desired_targets);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let desired_targets = client.get_desired_targets(&dummy_storage, round).await;
        assert_eq!(desired_targets.unwrap(), 10);
    }

    #[tokio::test]
    async fn test_get_block_number() {
        let mut dummy_storage = MockDummyStorage::new();
        let address = subxt::dynamic::storage("System", "Number", vec![]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let block_number = 10;
                let value = fake_value_thunk_from(block_number);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let block_number = client.get_block_number(&dummy_storage).await;
        assert_eq!(block_number.unwrap(), 10);
    }

    #[tokio::test]
    async fn test_get_min_nominator_bond() {
        let mut dummy_storage = MockDummyStorage::new();
        let address = subxt::dynamic::storage("Staking", "MinNominatorBond", vec![]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let min_nominator_bond: u128 = 10;
                let value = fake_value_thunk_from(min_nominator_bond);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let min_nominator_bond = client.get_min_nominator_bond(&dummy_storage).await;
        assert_eq!(min_nominator_bond.unwrap(), 10);
    }

    #[tokio::test]
    async fn test_get_min_validator_bond() {
        let mut dummy_storage = MockDummyStorage::new();
        let address = subxt::dynamic::storage("Staking", "MinValidatorBond", vec![]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let min_validator_bond: u128 = 10;
                let value = fake_value_thunk_from(min_validator_bond);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let min_validator_bond = client.get_min_validator_bond(&dummy_storage).await;
        assert_eq!(min_validator_bond.unwrap(), 10);
    }

    #[tokio::test]
    async fn test_fetch_paged_voter_snapshot() {
        let mut dummy_storage = MockDummyStorage::new();
        let round = 10;
        let page = 1;
        let address = subxt::dynamic::storage("MultiBlockElection", "PagedVoterSnapshot", vec![Value::from(round), Value::from(page)]);
        initialize_runtime_constants();
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let voter_snapshot: BoundedVec::<VoterData<PolkadotMinerConfig>, <PolkadotMinerConfig as MinerConfig>::VoterSnapshotPerBlock> = BoundedVec::new();
                let value = fake_value_thunk_from(voter_snapshot);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let voter_snapshot = client.fetch_paged_voter_snapshot(&dummy_storage, round, page).await;
        assert_eq!(voter_snapshot.unwrap(), BoundedVec::<VoterData<PolkadotMinerConfig>, <PolkadotMinerConfig as MinerConfig>::VoterSnapshotPerBlock>::new());
    }

    #[tokio::test]
    async fn test_fetch_paged_target_snapshot() {
        let mut dummy_storage = MockDummyStorage::new();
        let round = 10;
        let page = 1;
        let address = subxt::dynamic::storage("MultiBlockElection", "PagedTargetSnapshot", vec![Value::from(round), Value::from(page)]);
        initialize_runtime_constants();
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let target_snapshot: BoundedVec::<AccountId, <PolkadotMinerConfig as MinerConfig>::TargetSnapshotPerBlock> = BoundedVec::new();
                let value = fake_value_thunk_from(target_snapshot);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let target_snapshot = client.fetch_paged_target_snapshot(&dummy_storage, round, page).await;
        assert_eq!(target_snapshot.unwrap(), BoundedVec::<AccountId, <PolkadotMinerConfig as MinerConfig>::TargetSnapshotPerBlock>::new());
    }

    #[tokio::test]
    async fn test_get_validator_prefs() {
        let mut dummy_storage = MockDummyStorage::new();
        let validator = AccountId::new([0; 32]);
        let address = subxt::dynamic::storage("Staking", "Validators", vec![scale_value::Value::from(validator.encode())]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let validator_prefs = ValidatorPrefs {
                    commission: Perbill::from_parts(10),
                    blocked: false,
                };
                let value = fake_value_thunk_from(validator_prefs);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let validator_prefs = client.get_validator_prefs(&dummy_storage, validator).await;
        assert_eq!(validator_prefs.unwrap(), ValidatorPrefs {
            commission: Perbill::from_parts(10),
            blocked: false,
        });
    }

    #[tokio::test]
    async fn test_get_nominator() {
        let mut dummy_storage = MockDummyStorage::new();
        let nominator = AccountId::new([0; 32]);
        let address = subxt::dynamic::storage("Staking", "Nominators", vec![scale_value::Value::from(nominator.encode())]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let nominator = NominationsLight {
                    targets: vec![AccountId::new([0; 32])],
                    _submitted_in: 10,
                    suppressed: false,
                };
                let value = fake_value_thunk_from(nominator);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let nominator = client.get_nominator(&dummy_storage, nominator).await;
        let nominator = nominator.unwrap().unwrap();
        assert_eq!(nominator.targets, vec![AccountId::new([0; 32])]);
        assert_eq!(nominator._submitted_in, 10);
        assert_eq!(nominator.suppressed, false);
    }

    #[tokio::test]
    async fn test_get_controller_from_stash() {
        let mut dummy_storage = MockDummyStorage::new();
        let stash = AccountId::new([0; 32]);
        let address = subxt::dynamic::storage("Staking", "Bonded", vec![scale_value::Value::from(stash.encode())]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let controller = AccountId::new([0; 32]);
                let value = fake_value_thunk_from(controller);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let controller = client.get_controller_from_stash(&dummy_storage, stash).await;
        assert_eq!(controller.unwrap().unwrap(), AccountId::new([0; 32]));
    }

    #[tokio::test]
    async fn test_ledger() {
        let mut dummy_storage = MockDummyStorage::new();
        let account = AccountId::new([0; 32]);
        let address = subxt::dynamic::storage("Staking", "Ledger", vec![scale_value::Value::from(account.encode())]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let ledger = StakingLedger {
                    stash: AccountId::new([0; 32]),
                    total: 10,
                    active: 10,
                    unlocking: Vec::new()
                };
                let value = fake_value_thunk_from(ledger);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let ledger = client.ledger(&dummy_storage, account).await;
        let ledger = ledger.unwrap().unwrap();
        assert_eq!(ledger.stash, AccountId::new([0; 32]));
        assert_eq!(ledger.total, 10);
        assert_eq!(ledger.active, 10);
        let unlocking: Vec<UnlockChunk<Balance>> = Vec::new();
        assert_eq!(ledger.unlocking, unlocking);
    }

    #[tokio::test]
    async fn test_list_bags() {
        let mut dummy_storage = MockDummyStorage::new();
        let index: u64 = 1000;
        let address = subxt::dynamic::storage("VoterList", "ListBags", vec![Value::from(index)]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let head = AccountId::new([1; 32]);
                let tail = AccountId::new([2; 32]);
                let bag = ListBag {
                    head: Some(head),
                    tail: Some(tail),
                };
                let value = fake_value_thunk_from(bag);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let bag = client.list_bags(&dummy_storage, index).await;
        let bag = bag.unwrap().unwrap();
        assert_eq!(bag.head, Some(AccountId::new([1; 32])));
        assert_eq!(bag.tail, Some(AccountId::new([2; 32])));
    }

    #[tokio::test]
    async fn test_list_nodes() {
        let mut dummy_storage = MockDummyStorage::new();
        let account = AccountId::new([0; 32]);
        let address = subxt::dynamic::storage("VoterList", "ListNodes", vec![Value::from(account.encode())]);
        dummy_storage
            .expect_fetch()
            .with(eq(address.clone()))
            .returning(|_address| {
                let node = ListNode {
                    id: AccountId::new([0; 32]),
                    prev: Some(AccountId::new([1; 32])),
                    next: Some(AccountId::new([2; 32])),
                };
                let value = fake_value_thunk_from(node);
                Ok(Some(value))
            });
        let chain_client = MockChainClientTrait::new();
        let client = MultiBlockClient::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage> {client:chain_client, _phantom: PhantomData };
        let node = client.list_nodes(&dummy_storage, account).await;
        let node = node.unwrap().unwrap();
        assert_eq!(node.id, AccountId::new([0; 32]));
        assert_eq!(node.prev, Some(AccountId::new([1; 32])));
        assert_eq!(node.next, Some(AccountId::new([2; 32])));
    }
}
