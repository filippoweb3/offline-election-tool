use std::collections::HashSet;
use std::sync::Arc;

use mockall::automock;
use pallet_election_provider_multi_block::unsigned::miner::MinerConfig;
use sp_core::H256;
use sp_core::crypto::{Ss58Codec};
use sp_core::Get;
use futures::future::join_all;
use tracing::info;

use crate::multi_block_state_client::{BlockDetails, ChainClientTrait, ElectionSnapshotPage, MultiBlockClientTrait, StorageTrait, TargetSnapshotPage, VoterData, VoterSnapshotPage};
use crate::primitives::{AccountId, Storage};
use crate::raw_state_client::RawClientTrait;
use frame_support::BoundedVec;
use crate::{
    models::{Snapshot, SnapshotNominator, SnapshotValidator, StakingConfig}, 
    raw_state_client::RpcClient
};

#[automock]
#[async_trait::async_trait]
pub trait SnapshotService<MC, S>: Send + Sync
where
    MC: MinerConfig + Send + Sync + 'static,
    S: StorageTrait + From<Storage> + 'static,
{
    async fn build(
        &self,
        block: Option<H256>,
    ) -> Result<Snapshot, Box<dyn std::error::Error + Send + Sync>>;
    async fn get_snapshot_data_from_multi_block(
        &self,
        block_details: &BlockDetails<S>,
    ) -> Result<(ElectionSnapshotPage<MC>, StakingConfig), Box<dyn std::error::Error + Send + Sync>>;
}

pub struct SnapshotServiceImpl<
    RC: RpcClient + Send + Sync + 'static,
    CC: ChainClientTrait + Send + Sync + 'static,
    S: StorageTrait + From<Storage> + Clone + 'static,
    MC: MinerConfig + Send + Sync + Clone + 'static,
    MBC: MultiBlockClientTrait<CC, MC, S> + Send + Sync + 'static,
    RawC: RawClientTrait<RC> + Send + Sync + 'static,
>
where
{
    pub raw_state_client: Arc<RawC>,
    pub multi_block_state_client: Arc<MBC>,
    _phantom: std::marker::PhantomData<(RC, CC, S, MC)>,
}

impl<
    RC: RpcClient + Send + Sync + 'static,
    CC: ChainClientTrait + Send + Sync + 'static,
    S: StorageTrait + From<Storage> + Clone + 'static,
    MC: MinerConfig + Send + Sync + Clone + 'static,
    MBC: MultiBlockClientTrait<CC, MC, S> + Send + Sync + 'static,
    RawC: RawClientTrait<RC> + Send + Sync + 'static,
> SnapshotServiceImpl<RC, CC, S, MC, MBC, RawC> {
    pub fn new(multi_block_state_client: Arc<MBC>, raw_state_client: Arc<RawC>) -> Self {
        Self {
            multi_block_state_client,
            raw_state_client,
            _phantom: std::marker::PhantomData,
        }
    }
}

#[async_trait::async_trait]
impl<
    RC: RpcClient + Send + Sync + 'static,
    CC: ChainClientTrait + Send + Sync + 'static,
    S: StorageTrait + From<Storage> + Clone + 'static,
    MC: MinerConfig + Send + Sync + Clone + 'static,
    MBC: MultiBlockClientTrait<CC, MC, S> + Send + Sync + 'static,
    RawC: RawClientTrait<RC> + Send + Sync + 'static,
> SnapshotService<MC, S> for SnapshotServiceImpl<RC, CC, S, MC, MBC, RawC>
where
    MC: MinerConfig<AccountId = AccountId> + Send,
    MC::TargetSnapshotPerBlock: Send,
    MC::VoterSnapshotPerBlock: Send,
    MC::Pages: Send,
    MC::MaxVotesPerVoter: Send,
{
    async fn build(
        &self,
        block: Option<H256>,
    ) -> Result<Snapshot, Box<dyn std::error::Error + Send + Sync>> {
        let multi_block_state_client = self.multi_block_state_client.as_ref();
        let block_details = multi_block_state_client.get_block_details(block).await?;
        let (snapshot, staking_config) = self.get_snapshot_data_from_multi_block(&block_details)
            .await
            .map_err(|e| format!("Error getting snapshot data: {}", e))?;

        let voters = snapshot.voters;
        let targets = snapshot.targets;
        
        let storage = &block_details.storage;
        
        let validator_futures: Vec<_> = targets.into_iter().map(|target| {
            async move {
                let validator_prefs = multi_block_state_client.get_validator_prefs(storage, target.clone())
                    .await
                    .map_err(|e| format!("Error getting validator prefs: {}", e))?;
                
                Ok::<SnapshotValidator, String>(SnapshotValidator {
                    stash: target.to_ss58check(),
                    commission: validator_prefs.commission.deconstruct() as f64 / 1_000_000_000.0,
                    blocked: validator_prefs.blocked,
                })
            }
        }).collect();
        
        let validators: Vec<SnapshotValidator> = join_all(validator_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        
        let mut nominators: Vec<SnapshotNominator> = Vec::new();
        for voter_page in voters {
            for voter in voter_page {
                let nominator = SnapshotNominator {
                    stash: voter.0.to_ss58check(),
                    stake: voter.1 as u128,
                    nominations: voter.2.iter().map(|nomination| nomination.to_ss58check()).collect(),
                };
                nominators.push(nominator);
            }
        }
        
        Ok(Snapshot { validators, nominators, config: staking_config })
    }

    async fn get_snapshot_data_from_multi_block(
        &self,
        block_details: &BlockDetails<S>,
    ) -> Result<(ElectionSnapshotPage<MC>, StakingConfig), Box<dyn std::error::Error + Send + Sync>>
    {
        let client = self.multi_block_state_client.as_ref();
        let staking_config = get_staking_config_from_multi_block(client, block_details).await?;
        if block_details.phase.has_snapshot() {
            let mut voters = Vec::new();
            for page in 0..block_details.n_pages {
                let voters_page = client.fetch_paged_voter_snapshot(&block_details.storage, block_details.round, page).await?;
                voters.push(voters_page);
            }

            let target_snapshot = client.fetch_paged_target_snapshot(&block_details.storage, block_details.round, block_details.n_pages - 1).await?;

            return Ok((
                ElectionSnapshotPage::<MC> {
                    voters,
                    targets: target_snapshot,
                },
                staking_config));
        }
        info!("No snapshot found, getting validators and nominators from staking storage");

        let raw_client = self.raw_state_client.as_ref();
        let validators = raw_client.get_validators(block_details.block_hash).await?;
        let validator_set: HashSet<AccountId> = validators.iter().cloned().collect();

        // Prepare data for ElectionSnapshotPage
        let mut list_bags = raw_client.get_all_list_bags(block_details.block_hash).await?;
        list_bags.sort_by(|a, b| b.cmp(a));

        // Traverse bags       
        let bag_futures: Vec<_> = list_bags.iter().map(|&bag_threshold| {
            let storage = &block_details.storage;
            async move {
                let mut bag_accounts: Vec<AccountId> = Vec::new();
                
                let list_bag = match client.list_bags(storage, bag_threshold).await {
                    Ok(Some(bag)) => bag,
                    _ => return Ok::<Vec<AccountId>, String>(bag_accounts),
                };
                
                let mut current_node = list_bag.head;
                while let Some(voter) = current_node {
                    bag_accounts.push(voter.clone());
                    
                    let current_list_node = client.list_nodes(storage, voter).await
                        .map_err(|e| e.to_string())?;
                    current_node = current_list_node.and_then(|n| n.next);
                }
                
                Ok(bag_accounts)
            }
        }).collect();
        
        let bag_results = join_all(bag_futures).await;
        
        let mut ordered_accounts: Vec<AccountId> = Vec::new();
        for result in bag_results {
            match result {
                Ok(accounts) => ordered_accounts.extend(accounts),
                Err(e) => return Err(format!("Error traversing bag: {}", e).into()),
            }
        }
        
        let mut voters: Vec<VoterData<MC>> = Vec::new();
        
        let voter_futures: Vec<_> = ordered_accounts.iter().map(|voter| {
            let voter = voter.clone();
            let storage = &block_details.storage;
            let validator_set = &validator_set;
            
            async move {
                let controller = match client.get_controller_from_stash(storage, voter.clone()).await {
                    Ok(Some(c)) => c,
                    Ok(None) => return Ok::<Option<VoterData<MC>>, String>(None),
                    Err(e) => return Err(e.to_string()),
                };
                
                let stake = match client.ledger(storage, controller).await {
                    Ok(Some(l)) if l.active > 0 => l,
                    Ok(_) => return Ok(None),
                    Err(e) => return Err(e.to_string()),
                };
                
                let nominations = client.get_nominator(storage, voter.clone()).await
                    .map_err(|e| e.to_string())?;
                
                if let Some(nominations) = nominations {
                    if !nominations.suppressed && !nominations.targets.is_empty() {
                        let max_nominations = MC::MaxVotesPerVoter::get();
                        let mut targets = nominations.targets.clone();
                        targets.truncate(max_nominations as usize);
                        let targets_mc = BoundedVec::try_from(
                            targets.into_iter().map(|t| t.into()).collect::<Vec<AccountId>>()
                        ).map_err(|_| "Too many targets in voter".to_string())?;
                        return Ok(Some((voter, stake.active as u64, targets_mc)));
                    }
                } else if validator_set.contains(&voter) {
                    return Ok(Some((
                        voter.clone(),
                        stake.active as u64,
                        BoundedVec::try_from(vec![voter]).map_err(|_| "Too many targets")?
                    )));
                }
                
                Ok(None)
            }
        }).collect();

        let results = join_all(voter_futures).await;
        // limit to snapshot capacity (per-page slots * pages) to match real snapshot size
        let max_voters = MC::VoterSnapshotPerBlock::get() as usize * block_details.n_pages as usize;
        for result in results {
            if voters.len() >= max_voters {
                break;
            }
            match result {
                Ok(Some(voter_data)) => voters.push(voter_data.clone()),
                Ok(None) => {},
                Err(e) => return Err(format!("Error processing voter: {}", e).into()),
            }
        }

        // Reorder voters to match real snapshot pages
        let total_voters = voters.len();
        let voters: Vec<Vec<_>> = voters
            .chunks(MC::VoterSnapshotPerBlock::get() as usize)
            .map(|chunk| chunk.to_vec())
            .rev()
            .collect();
        
        info!("Completed voter data fetching. Total voters: {}", total_voters);

        // Filter validators by min validator bond if > 0 requesting for ledger
        let min_validator_bond = staking_config.min_validator_bond;
        
        let storage = &block_details.storage;
        let validators_futures: Vec<_> = validators.into_iter().map(|validator| {
            let client = client;
            async move {
                let controller = client.get_controller_from_stash(storage, validator.clone()).await
                    .map_err(|e| e.to_string())?;
                if controller.is_none() {
                    return Ok(None);
                }
                let controller = controller.unwrap();
                let validator_ledger = client.ledger(storage, controller).await
                    .map_err(|e| e.to_string())?;
                let active_stake = validator_ledger.clone().map_or(0, |l| l.active as u64);
                let has_sufficient_bond = validator_ledger.clone().map_or(false, |l| l.active >= min_validator_bond);

                Ok::<Option<(AccountId, u64)>, String>(has_sufficient_bond.then_some((validator, active_stake)))
            }
        }).collect();
        let results = join_all(validators_futures)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        
        let mut targets_with_stake: Vec<(AccountId, u64)> = Vec::new();
        for validator in results {
            if let Some((validator, stake)) = validator {
                targets_with_stake.push((validator, stake));
            }
        }

        // Prepare data for ElectionSnapshotPage
        // divide in pages
        let voters: Vec<VoterSnapshotPage<MC>> = voters
            .into_iter()
            .map(|page| BoundedVec::try_from(page).map_err(|_| "Too many voters in chunk"))
            .collect::<Result<Vec<_>, _>>()?;

        let targets = TargetSnapshotPage::<MC>::try_from(
            targets_with_stake.into_iter().map(|(v, _)| v.into()).collect::<Vec<AccountId>>()
        ).map_err(|_| "Too many targets")?;

        let election_snapshot_page = ElectionSnapshotPage::<MC> {
            voters,
            targets,
        };

        Ok((election_snapshot_page, staking_config))
}
}

pub async fn get_staking_config_from_multi_block<
    C: ChainClientTrait + Send + Sync + 'static, 
    MC: MinerConfig + Send + Sync + 'static, 
    S: StorageTrait + From<Storage> + 'static,
    MBC: MultiBlockClientTrait<C, MC, S> + Send + Sync + 'static>(
    client: &MBC,
    block_details: &BlockDetails<S>,
) -> Result<StakingConfig, Box<dyn std::error::Error + Send + Sync>>
where
    MC: Send + Sync + 'static,
{
    let max_nominations = MC::MaxVotesPerVoter::get();
    let min_nominator_bond = client.get_min_nominator_bond(&block_details.storage).await?;
    let min_validator_bond = client.get_min_validator_bond(&block_details.storage).await?;
    Ok(StakingConfig { desired_validators: block_details.desired_targets, max_nominations, min_nominator_bond, min_validator_bond: min_validator_bond })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockall::{mock};
    use mockall::predicate::eq;
    use crate::miner_config::polkadot::MinerConfig as PolkadotMinerConfig;
    use crate::multi_block_state_client::{ListBag, ListNode, MockChainClientTrait, MockMultiBlockClientTrait, Phase, StorageTrait};
    use crate::primitives::{AccountId, Hash};
    use crate::raw_state_client::{MockRawClientTrait, MockRpcClient, NominationsLight, StakingLedger};
    use crate::miner_config::initialize_runtime_constants;
    use sp_runtime::Perbill;
    use pallet_staking::ValidatorPrefs;
    use subxt::utils::Yes;
    use subxt::storage::Address;
    
    mock! {
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

    // Implement From<Storage> for DummyStorage to satisfy the trait bound in tests
    // This allows MockDummyStorage to be used with get_storage/get_block_details
    impl From<crate::primitives::Storage> for MockDummyStorage {
        fn from(_storage: crate::primitives::Storage) -> Self {
            MockDummyStorage::new()
        }
    }

    impl Clone for MockDummyStorage {
        fn clone(&self) -> Self {
            MockDummyStorage::new()
        }
    }

    #[tokio::test]
    async fn test_get_staking_config() {
        let mut mock_client = MockMultiBlockClientTrait::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage>::new();

        mock_client
            .expect_get_min_nominator_bond()
            .returning(|_storage: &MockDummyStorage| Ok(100));

        mock_client
            .expect_get_min_validator_bond()
            .returning(|_storage: &MockDummyStorage| Ok(200));

        let result = get_staking_config_from_multi_block(&mock_client, &BlockDetails::<MockDummyStorage> {
            block_hash: Some(Hash::zero()),
            phase: Phase::Snapshot(0),
            round: 1,
            n_pages: 1,
            desired_targets: 10,
            storage: MockDummyStorage::new(),
            _block_number: 100,
        }).await;

        assert!(result.is_ok());
        let config = result.unwrap();
        assert_eq!(config.min_nominator_bond, 100);
        assert_eq!(config.min_validator_bond, 200);
        assert_eq!(config.desired_validators, 10);
        assert_eq!(config.max_nominations, 16);
    }

    #[tokio::test]
    async fn test_get_snapshot_data_from_multi_block() {
        let mut mock_client = MockMultiBlockClientTrait::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage>::new();

        mock_client
            .expect_get_min_nominator_bond()
            .returning(|_storage: &MockDummyStorage| Ok(100));

        mock_client
            .expect_get_min_validator_bond()
            .returning(|_storage: &MockDummyStorage| Ok(200));

        mock_client
            .expect_fetch_paged_voter_snapshot()
            .returning(|_storage: &MockDummyStorage, _round: u32, _page: u32| Ok(VoterSnapshotPage::<PolkadotMinerConfig>::new()));

        mock_client
            .expect_fetch_paged_target_snapshot()
            .returning(|_storage: &MockDummyStorage, _round: u32, _page: u32| Ok(TargetSnapshotPage::<PolkadotMinerConfig>::new()));

        let raw_client = MockRawClientTrait::<MockRpcClient>::new();

        let snapshot_service = SnapshotServiceImpl::new(Arc::new(mock_client), Arc::new(raw_client));
            
        let result = snapshot_service.get_snapshot_data_from_multi_block(&BlockDetails::<MockDummyStorage> {
            block_hash: Some(Hash::zero()),
            phase: Phase::Signed(10),
            round: 1,
            n_pages: 1,
            desired_targets: 10,
            storage: MockDummyStorage::new(),
            _block_number: 100,
        }).await;

        assert!(result.is_ok());
        let (snapshot, config) = result.unwrap();
        
        assert_eq!(snapshot.voters, vec![VoterSnapshotPage::<PolkadotMinerConfig>::new()]);
        assert_eq!(snapshot.targets, TargetSnapshotPage::<PolkadotMinerConfig>::new());
        assert_eq!(config.min_nominator_bond, 100);
        assert_eq!(config.min_validator_bond, 200);
        assert_eq!(config.desired_validators, 10);
        assert_eq!(config.max_nominations, 16);
    }   

    #[tokio::test]
    async fn test_get_snapshot_data_from_multi_block_no_snapshot() {
        initialize_runtime_constants();
        let mut mock_client = MockMultiBlockClientTrait::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage>::new();

        mock_client
            .expect_get_min_nominator_bond()
            .returning(|_storage: &MockDummyStorage| Ok(0));

        mock_client
            .expect_get_min_validator_bond()
            .returning(|_storage: &MockDummyStorage| Ok(0));

        let mut raw_client = MockRawClientTrait::<MockRpcClient>::new();

        raw_client
            .expect_get_validators()
            .returning(|_at: Option<H256>| Ok(vec![AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap()]));

        raw_client
            .expect_get_all_list_bags()
            .returning(|_at: Option<H256>| Ok(vec![100]));

        mock_client
            .expect_list_bags()
            .returning(|_storage: &MockDummyStorage, _index: u64| Ok(Some(ListBag {
                head: Some(AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap()),
                tail: None,
            })));

        mock_client
            .expect_list_nodes()
            .times(1)
            .return_once(|_storage: &MockDummyStorage, _account: AccountId| Ok(Some(ListNode {
                id: AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap(),
                prev: None,
                next: Some(AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap()),
            })));

        mock_client
            .expect_list_nodes()
            .times(1)
            .return_once(|_storage: &MockDummyStorage, _account: AccountId| Ok(Some(ListNode {
                id: AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap(),
                prev: None,
                next: None,
            })));
              
        mock_client
            .expect_get_controller_from_stash()
            .returning(|_storage: &MockDummyStorage, _stash: AccountId| Ok(Some(AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap())));

        mock_client
            .expect_ledger()
            .returning(|_storage: &MockDummyStorage, _account: AccountId| Ok(Some(StakingLedger {
                stash: AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap(),
                total: 100,
                active: 100,
                unlocking: vec![],
            })));
        
        mock_client
            .expect_get_nominator()
            .returning(|_storage: &MockDummyStorage, _nominator: AccountId| Ok(Some(NominationsLight {
                targets: vec![AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap()],
                _submitted_in: 10,
                suppressed: false,
            })));
        
        mock_client
            .expect_get_controller_from_stash()
            .returning(|_storage: &MockDummyStorage, _stash: AccountId| Ok(Some(AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap())));

        mock_client
            .expect_ledger()
            .returning(|_storage: &MockDummyStorage, _account: AccountId| Ok(Some(StakingLedger {
                stash: AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap(),
                total: 100,
                active: 100,
                unlocking: vec![],
            })));
        
        mock_client
            .expect_get_nominator()
            .returning(|_storage: &MockDummyStorage, _nominator: AccountId| Ok(None));

        mock_client
            .expect_get_controller_from_stash()
            .returning(|_storage: &MockDummyStorage, _stash: AccountId| Ok(Some(AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap())));

        mock_client
            .expect_ledger()
            .returning(|_storage: &MockDummyStorage, _account: AccountId| Ok(Some(StakingLedger {
                stash: AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap(),
                total: 100,
                active: 100,
                unlocking: vec![],
            })));

        let snapshot_service = SnapshotServiceImpl::new(Arc::new(mock_client), Arc::new(raw_client));
        let result = snapshot_service.get_snapshot_data_from_multi_block(&BlockDetails::<MockDummyStorage> {
            block_hash: Some(Hash::zero()),
            phase: Phase::Snapshot(10),
            round: 1,
            n_pages: 1,
            desired_targets: 10,
            storage: MockDummyStorage::new(),
            _block_number: 100,
        }).await;

        assert!(result.is_ok());
        let (snapshot, config) = result.unwrap();
        let voter_targets = BoundedVec::try_from(vec![AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap()]).map_err(|_| "Too many targets in voter").unwrap();
        let voter = (AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap(),
            100,
            voter_targets
        );
        let validator_voter_targets = BoundedVec::try_from(vec![AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap()]).map_err(|_| "Too many targets in voter").unwrap();
        let validator_voter = (AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap(),
            100,
            validator_voter_targets
        );
        let voter_page: VoterSnapshotPage<PolkadotMinerConfig> = BoundedVec::try_from(vec![voter, validator_voter]).map_err(|_| "Too many voters in chunk").unwrap();
        let voters = vec![voter_page];

        let targets: TargetSnapshotPage<PolkadotMinerConfig> = BoundedVec::try_from(vec![AccountId::from_ss58check("5CSbZ7wG456oty4WoiX6a1J88VUbrCXLhrKVJ9q95BsYH4TZ").unwrap()]).map_err(|_| "Too many targets in voter").unwrap();

        assert_eq!(snapshot.voters, voters);
        assert_eq!(snapshot.targets, targets);
        assert_eq!(config.min_nominator_bond, 0);
        assert_eq!(config.min_validator_bond, 0);
        assert_eq!(config.desired_validators, 10);
        assert_eq!(config.max_nominations, 16);
    }

    #[tokio::test]
    async fn test_build() {
        initialize_runtime_constants();
        let mut mock_client = MockMultiBlockClientTrait::<MockChainClientTrait, PolkadotMinerConfig, MockDummyStorage>::new();

        mock_client
            .expect_get_block_details()
            .with(eq(None))
            .returning(|_block: Option<H256>| Ok(BlockDetails::<MockDummyStorage> {
                block_hash: Some(Hash::zero()),
                phase: Phase::Signed(10),
                round: 1,
                n_pages: 1,
                desired_targets: 10,
                storage: MockDummyStorage::new(),
                _block_number: 100,
            }));

        mock_client
            .expect_get_min_nominator_bond()
            .returning(|_storage: &MockDummyStorage| Ok(0));

        mock_client
            .expect_get_min_validator_bond()
            .returning(|_storage: &MockDummyStorage| Ok(0));
        
        let voter_targets = BoundedVec::try_from(vec![AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap()]).map_err(|_| "Too many targets in voter").unwrap();
        let voter = (AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap(),
            100,
            voter_targets
        );
        let voter_page: VoterSnapshotPage<PolkadotMinerConfig> = BoundedVec::try_from(vec![voter]).map_err(|_| "Too many voters in chunk").unwrap();

        mock_client
            .expect_fetch_paged_voter_snapshot()
            .returning(move |_storage: &MockDummyStorage, _round: u32, _page: u32| Ok(voter_page.clone()));

        let targets = BoundedVec::try_from(vec![AccountId::from_ss58check("5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty").unwrap()]).map_err(|_| "Too many targets in voter").unwrap();
        mock_client
            .expect_fetch_paged_target_snapshot()
            .returning(move |_storage: &MockDummyStorage, _round: u32, _page: u32| Ok(targets.clone()));

        mock_client
            .expect_get_validator_prefs()
            .returning(|_storage: &MockDummyStorage, _validator: AccountId| Ok(ValidatorPrefs {
                commission: Perbill::from_parts(0),
                blocked: false,
            }));
        
        let raw_client = MockRawClientTrait::<MockRpcClient>::new();

        let snapshot_service = SnapshotServiceImpl::new(Arc::new(mock_client), Arc::new(raw_client));
        let result = snapshot_service.build(None).await;
        assert!(result.is_ok());
        let snapshot = result.unwrap();
        assert_eq!(snapshot.validators, vec![SnapshotValidator {
            stash: "5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty".to_string(),
            commission: 0.0,
            blocked: false,
        }]);
        assert_eq!(snapshot.nominators, vec![SnapshotNominator {
            stash: "5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty".to_string(),
            stake: 100,
            nominations: vec!["5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty".to_string()]
        }]);
        
        assert_eq!(snapshot.config, StakingConfig {
            desired_validators: 10,
            max_nominations: 16,
            min_nominator_bond: 0,
            min_validator_bond: 0,
        });
    }
}