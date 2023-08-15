pub mod schema {
    pub mod solana {
        pub mod snapshot {
            include!(concat!(env!("OUT_DIR"), "/solana.snapshot.rs"));
        }
    }
}

use {
    prost::Message,
    schema::solana::snapshot::{
        blockhash_queue::Age as ProtoBlockhashAge,
        epoch_rewards::{
            epoch_stake_reward::{
                reward_info::RewardKind as ProtoEpochStakeRewardKind,
                RewardInfo as ProtoEpochStakeRewardInfo,
            },
            EpochStakeReward as ProtoEpochStakeReward,
        },
        epoch_stake::{
            EpochAuthorizedVoter as ProtoEpochAuthorizedVoter,
            NodeIdToVoteAccounts as ProtoNodeIdToVoteAccounts,
        },
        stakes::{
            stake_delegations_entry::StakeDelegation as ProtoStakeDelegation,
            StakeDelegationsEntry as ProtoStakeDelegationsEntry, StakeHistory as ProtoStakeHistory,
            VoteAccountsEntry as ProtoVoteAccountsEntry,
        },
        Account as ProtoAccount, Bank as ProtoBank, BlockhashQueue as ProtoBlockhashQueue,
        EpochRewards as ProtoEpochRewards, EpochSchedule as ProtoEpochSchedule,
        EpochStake as ProtoEpochStake, FeeCalculator as ProtoFeeCalculator,
        FeeRateGovernor as ProtoFeeRateGovernor, HardFork as ProtoHardFork,
        IncrementalSnapshotPersistence as ProtoIncrementalSnapshotPersistence,
        Inflation as ProtoInflation, Rent as ProtoRent, RentCollector as ProtoRentCollector,
        Snapshot as ProtoSnapshot, Stakes as ProtoStakes,
    },
    solana_runtime::{
        bank::{
            Bank, BankFieldsToDeserialize, EpochRewardStatus, StakeReward,
            StartBlockHeightAndRewards,
        },
        blockhash_queue::{BlockhashQueue, HashAge as BlockhashAge},
        epoch_stakes::NodeVoteAccounts,
        rent_collector::RentCollector,
        serde_snapshot::BankIncrementalSnapshotPersistence,
        stakes::{Stakes, StakesCache, StakesEnum},
    },
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount},
        clock::Slot,
        epoch_schedule::EpochSchedule,
        fee_calculator::{FeeCalculator, FeeRateGovernor},
        genesis_config::GenesisConfig,
        hash::Hash,
        inflation::Inflation,
        pubkey::Pubkey,
        rent::Rent,
        stake::state::Delegation,
    },
    std::{collections::HashMap, ops::Deref, sync::Arc},
};

fn main() {
    let bank = {
        let genesis_config = GenesisConfig::default();
        let mut bank = Arc::new(Bank::new_for_tests(&genesis_config));
        for _ in 0..21 {
            bank = Arc::new(Bank::new_from_parent(
                &bank,
                &Pubkey::new_unique(),
                bank.slot() + 1,
            ));
            bank.fill_bank_with_ticks_for_tests();
        }
        bank.freeze();
        Arc::into_inner(bank).unwrap()
    };

    let serialized_snapshot = snapshot_bank(&bank);
    println!(
        "serialized snapshot: size: {}, {serialized_snapshot:?}",
        serialized_snapshot.len()
    );

    let deserialized_bank = rebuild_bank(serialized_snapshot);
}

fn snapshot_bank(bank: &Bank) -> Vec<u8> {
    let mut snapshot = ProtoSnapshot::default();
    snapshot.bank = Some(ProtoBank::from(bank));
    snapshot.encode_to_vec()
}

fn rebuild_bank(serialized_snapshot: Vec<u8>) -> Bank {
    let snapshot: ProtoSnapshot = Message::decode(serialized_snapshot.as_slice()).unwrap();
    let bank = snapshot.bank.unwrap();
    bank.into()
}

impl From<&Bank> for ProtoBank {
    fn from(bank: &Bank) -> Self {
        let ancestors_for_bank_fields = HashMap::<Slot, usize>::from(&bank.ancestors); // TODO: it would be nice to not make a copy
        let bank_fields = bank.get_fields_to_serialize(&ancestors_for_bank_fields);
        let hard_forks = bank_fields
            .hard_forks
            .read()
            .unwrap()
            .iter()
            .map(|(slot, count)| ProtoHardFork {
                slot: *slot,
                count: *count as u64,
            })
            .collect();
        let blockhash_queue = bank_fields.blockhash_queue.read().unwrap().deref().into();
        let stakes = bank_fields.stakes.into();
        let epoch_stakes = bank_fields
            .epoch_stakes
            .iter()
            .map(|(epoch, epoch_stake)| {
                let to_proto_node_id_to_vote_accounts =
                    |(node_id, node_vote_accounts): (&Pubkey, &NodeVoteAccounts)| {
                        ProtoNodeIdToVoteAccounts {
                            node_id: node_id.to_bytes().into(),
                            total_stake: node_vote_accounts.total_stake,
                            vote_accounts: node_vote_accounts
                                .vote_accounts
                                .iter()
                                .map(|vote_account| vote_account.to_bytes().into())
                                .collect(),
                        }
                    };
                let to_proto_epoch_authorized_voter =
                    |(vote_account, authorized_voter): (&Pubkey, &Pubkey)| {
                        ProtoEpochAuthorizedVoter {
                            vote_account: vote_account.to_bytes().into(),
                            authorized_voter: authorized_voter.to_bytes().into(),
                        }
                    };
                let node_ids_to_vote_accounts = epoch_stake
                    .node_id_to_vote_accounts()
                    .iter()
                    .map(to_proto_node_id_to_vote_accounts)
                    .collect();
                let epoch_authorized_voters = epoch_stake
                    .epoch_authorized_voters()
                    .iter()
                    .map(to_proto_epoch_authorized_voter)
                    .collect();
                ProtoEpochStake {
                    epoch: *epoch,
                    total_stake: epoch_stake.total_stake(),
                    stakes: Some(epoch_stake.stakes().into()),
                    node_ids_to_vote_accounts,
                    epoch_authorized_voters,
                }
            })
            .collect();
        let epoch_rewards = bank
            .get_epoch_reward_status_to_serialize()
            .and_then(|epoch_reward_status| match epoch_reward_status {
                EpochRewardStatus::Active(start_block_height_and_rewards) => {
                    Some(start_block_height_and_rewards)
                }
                EpochRewardStatus::Inactive => None,
            })
            .map(|start_block_height_and_rewards| {
                let to_proto_epoch_stake_reward =
                    |stake_reward: &StakeReward| ProtoEpochStakeReward {
                        stake_pubkey: stake_reward.stake_pubkey.to_bytes().into(),
                        stake_account: Some(stake_reward.stake_account.clone().into()), // TODO: avoid the clone
                        stake_reward_info: Some(ProtoEpochStakeRewardInfo {
                            reward_kind: stake_reward.stake_reward_info.reward_type as i32, // TODO: change from cast to proper match
                            lamports: stake_reward.stake_reward_info.lamports.try_into().unwrap(),
                            post_balance: stake_reward.stake_reward_info.post_balance,
                            commission: stake_reward.stake_reward_info.commission.map(Into::into),
                        }),
                    };
                let epoch_stake_rewards = start_block_height_and_rewards
                    .calculated_epoch_stake_rewards
                    .iter()
                    .map(to_proto_epoch_stake_reward)
                    .collect();
                ProtoEpochRewards {
                    start_block_height: start_block_height_and_rewards.start_block_height,
                    epoch_stake_rewards,
                }
            });
        Self {
            epoch: bank_fields.epoch,
            block_height: bank_fields.block_height,
            slot: bank_fields.slot,
            hash: bank_fields.hash.to_bytes().into(),
            epoch_accounts_hash: bank
                .get_epoch_accounts_hash_to_serialize()
                .map(|eah| eah.as_ref().to_bytes().into()),
            signature_count: bank_fields.signature_count,
            capitalization: bank_fields.capitalization,
            parent_slot: bank_fields.parent_slot,
            parent_hash: bank_fields.parent_hash.to_bytes().into(),
            transaction_count: bank_fields.transaction_count,
            tick_height: bank_fields.tick_height,
            max_tick_height: bank_fields.max_tick_height,
            hashes_per_tick: bank_fields.hashes_per_tick,
            ticks_per_slot: bank_fields.ticks_per_slot,
            ns_per_slot: bank_fields
                .ns_per_slot
                .try_into()
                .expect("cast ns_per_slot to u64"),
            slots_per_year: bank_fields.slots_per_year,
            collector_id: bank_fields.collector_id.to_bytes().into(),
            collector_fees: bank_fields.collector_fees,
            collected_rent: bank_fields.collected_rent,
            accounts_data_size: bank_fields.accounts_data_len,
            is_delta: bank_fields.is_delta,
            ancestors: bank.ancestors.keys(),
            genesis_creation_time: bank_fields.genesis_creation_time,
            inflation: Some(bank_fields.inflation.into()),
            hard_forks,
            fee_rate_governor: Some(bank_fields.fee_rate_governor.into()),
            incremental_snapshot_persistence: bank
                .incremental_snapshot_persistence
                .as_ref()
                .map(Into::into),
            rent_collector: Some(bank_fields.rent_collector.into()),
            epoch_schedule: Some(bank_fields.epoch_schedule.into()),
            blockhash_queue: Some(blockhash_queue),
            stakes: Some(stakes),
            epoch_stakes,
            epoch_rewards,
        }
    }
}

impl From<ProtoBank> for Bank {
    fn from(bank: ProtoBank) -> Self {
        println!("proto bank: {bank:?}");

        /*
            let bank_fields = BankFieldsToDeserialize {
                        blockhash_queue: todo!(),
            ancestors: todo!(),
            hash: *bytemuck::cast_ref(fb_bank.hash().unwrap()),
            parent_hash: *bytemuck::cast_ref(fb_bank.parent_hash().unwrap()),
            parent_slot: fb_bank.parent_slot(),
            hard_forks: todo!(),
            transaction_count: fb_bank.transaction_count(),
            tick_height: fb_bank.tick_height(),
            signature_count: fb_bank.signature_count(),
            capitalization: fb_bank.capitalization(),
            max_tick_height: fb_bank.max_tick_height(),
            hashes_per_tick: fb_bank.hashes_per_tick(),
            ticks_per_slot: fb_bank.ticks_per_slot(),
            ns_per_slot: *bytemuck::cast_ref(fb_bank.ns_per_slot().unwrap()),
            genesis_creation_time: todo!(),
            slots_per_year: fb_bank.slots_per_year(),
            slot: fb_bank.slot(),
            epoch: fb_bank.epoch(),
            block_height: fb_bank.block_height(),
            collector_id: *bytemuck::cast_ref(fb_bank.collector_id().unwrap()),
            collector_fees: fb_bank.collector_fees(),
            fee_calculator: todo!(),
            fee_rate_governor: todo!(),
            collected_rent: fb_bank.collected_rent(),
            rent_collector: todo!(), // TODO: I think this is unused?
            epoch_schedule: todo!(),
            inflation: todo!(),
            stakes: todo!(),       //Stakes<Delegation>,
            epoch_stakes: todo!(), //HashMap<Epoch, EpochStakes>,
            is_delta: fb_bank.is_delta(),
            accounts_data_len: fb_bank.accounts_data_size(),
            incremental_snapshot_persistence: todo!(), //Option<BankIncrementalSnapshotPersistence>,
            epoch_accounts_hash: todo!(),
            epoch_reward_status: todo!(), //EpochRewardStatus,






        /*
         * blockhash_queue: BlockhashQueue,
         * ancestors: AncestorsForSerialization,
         * hash: Hash,
         * parent_hash: Hash,
         * parent_slot: Slot,
         * hard_forks: HardForks,
         * transaction_count: u64,
         * tick_height: u64,
         * signature_count: u64,
         * capitalization: u64,
         * max_tick_height: u64,
         * hashes_per_tick: Option<u64>,
         * ticks_per_slot: u64,
         * ns_per_slot: u128,
         * genesis_creation_time: UnixTimestamp,
         * slots_per_year: f64,
         * slot: Slot,
         * epoch: Epoch,
         * block_height: u64,
         * collector_id: Pubkey,
         * collector_fees: u64,
         * fee_calculator: FeeCalculator,
         * fee_rate_governor: FeeRateGovernor,
         * collected_rent: u64,
         * rent_collector: RentCollector,
         * epoch_schedule: EpochSchedule,
         * inflation: Inflation,
         * stakes: Stakes<Delegation>,
         * epoch_stakes: HashMap<Epoch, EpochStakes>,
         * is_delta: bool,
         * accounts_data_len: u64,
         * incremental_snapshot_persistence: Option<BankIncrementalSnapshotPersistence>,
         * epoch_accounts_hash: Option<Hash>,
         * epoch_reward_status: EpochRewardStatus,
         */
            };

            */

        // TODO: impl
        Bank::default_for_tests()
    }
}

impl From<Inflation> for ProtoInflation {
    fn from(inflation: Inflation) -> Self {
        Self {
            initial: inflation.initial,
            terminal: inflation.terminal,
            taper: inflation.taper,
            foundation: inflation.foundation,
            foundation_term: inflation.foundation_term,
        }
    }
}

impl From<FeeRateGovernor> for ProtoFeeRateGovernor {
    fn from(fee_rate_governor: FeeRateGovernor) -> Self {
        Self {
            lamports_per_signature: fee_rate_governor.lamports_per_signature,
            target_signatures_per_slot: fee_rate_governor.target_signatures_per_slot,
            min_lamports_per_signature: fee_rate_governor.min_lamports_per_signature,
            max_lamports_per_signature: fee_rate_governor.max_lamports_per_signature,
            burn_percent: fee_rate_governor.burn_percent.into(),
        }
    }
}

impl From<&BankIncrementalSnapshotPersistence> for ProtoIncrementalSnapshotPersistence {
    fn from(incremental_snapshot_persistence: &BankIncrementalSnapshotPersistence) -> Self {
        Self {
            full_slot: incremental_snapshot_persistence.full_slot,
            full_hash: incremental_snapshot_persistence
                .full_hash
                .0
                .to_bytes()
                .into(),
            full_capitalization: incremental_snapshot_persistence.full_capitalization,
            incremental_hash: incremental_snapshot_persistence
                .incremental_hash
                .0
                .to_bytes()
                .into(),
            incremental_capitalization: incremental_snapshot_persistence.incremental_capitalization,
        }
    }
}

impl From<RentCollector> for ProtoRentCollector {
    fn from(rent_collector: RentCollector) -> Self {
        Self {
            epoch: rent_collector.epoch,
            epoch_schedule: Some(rent_collector.epoch_schedule.into()),
            slots_per_year: rent_collector.slots_per_year,
            rent: Some(rent_collector.rent.into()),
        }
    }
}

impl From<Rent> for ProtoRent {
    fn from(rent: Rent) -> Self {
        Self {
            lamports_per_byte_year: rent.lamports_per_byte_year,
            exemption_threshold: rent.exemption_threshold,
            burn_percent: rent.burn_percent.into(),
        }
    }
}

impl From<EpochSchedule> for ProtoEpochSchedule {
    fn from(epoch_schedule: EpochSchedule) -> Self {
        Self {
            slots_per_epoch: epoch_schedule.slots_per_epoch,
            leader_schedule_slot_offset: epoch_schedule.leader_schedule_slot_offset,
            warmup: epoch_schedule.warmup,
            first_normal_epoch: epoch_schedule.first_normal_epoch,
            first_normal_slot: epoch_schedule.first_normal_slot,
        }
    }
}

impl From<&BlockhashQueue> for ProtoBlockhashQueue {
    fn from(blockhash_queue: &BlockhashQueue) -> Self {
        Self {
            last_hash_index: blockhash_queue.last_hash_index,
            last_hash: blockhash_queue.last_hash.map(|hash| hash.to_bytes().into()),
            max_age: blockhash_queue.max_age.try_into().unwrap(),
            ages: blockhash_queue.ages.iter().map(Into::into).collect(),
        }
    }
}

impl From<(&Hash, &BlockhashAge)> for ProtoBlockhashAge {
    fn from(blockhash_age: (&Hash, &BlockhashAge)) -> Self {
        Self {
            hash: blockhash_age.0.to_bytes().into(),
            hash_index: blockhash_age.1.hash_index,
            timestamp: blockhash_age.1.timestamp,
            fee_calculator: Some(blockhash_age.1.fee_calculator.into()),
        }
    }
}

impl From<FeeCalculator> for ProtoFeeCalculator {
    fn from(fee_calculator: FeeCalculator) -> Self {
        Self {
            lamports_per_signature: fee_calculator.lamports_per_signature,
        }
    }
}

impl From<&StakesCache> for ProtoStakes {
    fn from(stakes_cache: &StakesCache) -> Self {
        let stakes = Stakes::<Delegation>::from(stakes_cache.0.read().unwrap().clone());
        stakes.into()
    }
}

impl From<&StakesEnum> for ProtoStakes {
    fn from(stakes: &StakesEnum) -> Self {
        let stakes = match stakes {
            StakesEnum::Accounts(stakes) => stakes.clone().into(),
            StakesEnum::Delegations(stakes) => stakes.clone(),
        };
        stakes.into()
    }
}

impl From<Stakes<Delegation>> for ProtoStakes {
    fn from(stakes: Stakes<Delegation>) -> Self {
        use solana_runtime::vote_account::VoteAccountsHashMap;
        let vote_accounts_hash_map: Arc<VoteAccountsHashMap> = (&stakes.vote_accounts).into();
        let proto_vote_accounts = vote_accounts_hash_map
            .iter()
            .map(|(pubkey, stake_account)| {
                let account: AccountSharedData = stake_account.1.clone().into();
                ProtoVoteAccountsEntry {
                    pubkey: pubkey.to_bytes().into(),
                    stake: stake_account.0,
                    vote_account: Some(account.into()),
                }
            })
            .collect();

        let proto_stake_delegations = stakes
            .stake_delegations
            .iter()
            .map(|(pubkey, delegation)| ProtoStakeDelegationsEntry {
                pubkey: pubkey.to_bytes().into(),
                delegation: Some(ProtoStakeDelegation {
                    voter_pubkey: delegation.voter_pubkey.to_bytes().into(),
                    stake: delegation.stake,
                    activation_epoch: delegation.activation_epoch,
                    deactivation_epoch: delegation.deactivation_epoch,
                    warmup_cooldown_rate: delegation.warmup_cooldown_rate,
                }),
            })
            .collect();

        let proto_stake_history = stakes
            .stake_history
            .iter()
            .map(|(epoch, stake_history_entry)| ProtoStakeHistory {
                epoch: *epoch,
                effective: stake_history_entry.effective,
                activating: stake_history_entry.activating,
                deactivating: stake_history_entry.deactivating,
            })
            .collect();

        Self {
            epoch: stakes.epoch,
            vote_accounts: proto_vote_accounts,
            stake_delegations: proto_stake_delegations,
            stake_history: proto_stake_history,
        }
    }
}

impl<A: ReadableAccount> From<A> for ProtoAccount {
    fn from(account: A) -> Self {
        Self {
            lamports: account.lamports(),
            data: account.data().into(),
            owner: account.owner().to_bytes().into(),
            executable: account.executable(),
            rent_epoch: account.rent_epoch(),
        }
    }
}
