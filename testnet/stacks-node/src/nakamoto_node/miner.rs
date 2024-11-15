// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2023 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use clarity::boot_util::boot_code_id;
use clarity::vm::types::PrincipalData;
use libsigner::v0::messages::{MinerSlotID, SignerMessage};
use libsigner::StackerDBSession;
use rand::{thread_rng, Rng};
use stacks::burnchains::Burnchain;
use stacks::chainstate::burn::db::sortdb::SortitionDB;
use stacks::chainstate::burn::{BlockSnapshot, ConsensusHash};
use stacks::chainstate::coordinator::OnChainRewardSetProvider;
use stacks::chainstate::nakamoto::coordinator::load_nakamoto_reward_set;
use stacks::chainstate::nakamoto::miner::{NakamotoBlockBuilder, NakamotoTenureInfo};
use stacks::chainstate::nakamoto::staging_blocks::NakamotoBlockObtainMethod;
use stacks::chainstate::nakamoto::{NakamotoBlock, NakamotoChainState};
use stacks::chainstate::stacks::boot::{RewardSet, MINERS_NAME};
use stacks::chainstate::stacks::db::{StacksChainState, StacksHeaderInfo};
use stacks::chainstate::stacks::{
    CoinbasePayload, Error as ChainstateError, StacksTransaction, StacksTransactionSigner,
    TenureChangeCause, TenureChangePayload, TransactionAnchorMode, TransactionPayload,
    TransactionVersion,
};
use stacks::net::p2p::NetworkHandle;
use stacks::net::stackerdb::StackerDBs;
use stacks::net::{NakamotoBlocksData, StacksMessageType};
use stacks::util::get_epoch_time_secs;
use stacks::util::secp256k1::MessageSignature;
use stacks_common::types::chainstate::{StacksAddress, StacksBlockId};
use stacks_common::types::{PrivateKey, StacksEpochId};
use stacks_common::util::vrf::VRFProof;

use super::relayer::RelayerThread;
use super::sign_coordinator::SignCoordinator;
use super::{Config, Error as NakamotoNodeError, EventDispatcher, Keychain};
use crate::nakamoto_node::VRF_MOCK_MINER_KEY;
use crate::neon_node;
use crate::run_loop::nakamoto::Globals;
use crate::run_loop::RegisteredKey;

#[cfg(test)]
pub static TEST_MINE_STALL: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub static TEST_BROADCAST_STALL: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub static TEST_BLOCK_ANNOUNCE_STALL: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);
#[cfg(test)]
pub static TEST_SKIP_P2P_BROADCAST: std::sync::Mutex<Option<bool>> = std::sync::Mutex::new(None);

/// If the miner was interrupted while mining a block, how long should the
///  miner thread sleep before trying again?
const ABORT_TRY_AGAIN_MS: u64 = 200;

pub enum MinerDirective {
    /// The miner won sortition so they should begin a new tenure
    BeginTenure {
        parent_tenure_start: StacksBlockId,
        burnchain_tip: BlockSnapshot,
    },
    /// The miner should try to continue their tenure if they are the active miner
    ContinueTenure { new_burn_view: ConsensusHash },
    /// The miner did not win sortition
    StopTenure,
}

#[derive(PartialEq, Debug, Clone)]
/// Tenure info needed to construct a tenure change or tenure extend transaction
struct ParentTenureInfo {
    /// The number of blocks in the parent tenure
    parent_tenure_blocks: u64,
    /// The consensus hash of the parent tenure
    parent_tenure_consensus_hash: ConsensusHash,
}

/// Metadata required for beginning a new tenure
struct ParentStacksBlockInfo {
    /// Header metadata for the Stacks block we're going to build on top of
    stacks_parent_header: StacksHeaderInfo,
    /// nonce to use for this new block's coinbase transaction
    coinbase_nonce: u64,
    parent_tenure: Option<ParentTenureInfo>,
}

/// The reason the miner thread was spawned
#[derive(PartialEq, Clone, Debug)]
pub enum MinerReason {
    /// The miner thread was spawned to begin a new tenure
    BlockFound,
    /// The miner thread was spawned to extend an existing tenure
    Extended {
        /// Current consensus hash on the underlying burnchain.  Corresponds to the last-seen
        /// sortition.
        burn_view_consensus_hash: ConsensusHash,
    },
}

impl std::fmt::Display for MinerReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MinerReason::BlockFound => write!(f, "BlockFound"),
            MinerReason::Extended {
                burn_view_consensus_hash,
            } => write!(
                f,
                "Extended: burn_view_consensus_hash = {burn_view_consensus_hash:?}",
            ),
        }
    }
}

pub struct BlockMinerThread {
    /// node config struct
    config: Config,
    /// handle to global state
    globals: Globals,
    /// copy of the node's keychain
    keychain: Keychain,
    /// burnchain configuration
    burnchain: Burnchain,
    /// Last block mined
    last_block_mined: Option<NakamotoBlock>,
    /// Copy of the node's registered VRF key
    registered_key: RegisteredKey,
    /// Burnchain block snapshot which elected this miner
    burn_election_block: BlockSnapshot,
    /// Current burnchain tip
    burn_block: BlockSnapshot,
    /// The start of the parent tenure for this tenure
    parent_tenure_id: StacksBlockId,
    /// Handle to the node's event dispatcher
    event_dispatcher: EventDispatcher,
    /// The reason the miner thread was spawned
    reason: MinerReason,
    /// Handle to the p2p thread for block broadcast
    p2p_handle: NetworkHandle,
    signer_set_cache: Option<RewardSet>,
}

impl BlockMinerThread {
    /// Instantiate the miner thread
    pub fn new(
        rt: &RelayerThread,
        registered_key: RegisteredKey,
        burn_election_block: BlockSnapshot,
        burn_block: BlockSnapshot,
        parent_tenure_id: StacksBlockId,
        reason: MinerReason,
    ) -> BlockMinerThread {
        BlockMinerThread {
            config: rt.config.clone(),
            globals: rt.globals.clone(),
            keychain: rt.keychain.clone(),
            burnchain: rt.burnchain.clone(),
            last_block_mined: None,
            registered_key,
            burn_election_block,
            burn_block,
            event_dispatcher: rt.event_dispatcher.clone(),
            parent_tenure_id,
            reason,
            p2p_handle: rt.get_p2p_handle(),
            signer_set_cache: None,
        }
    }

    #[cfg(test)]
    fn fault_injection_block_broadcast_stall(new_block: &NakamotoBlock) {
        if *TEST_BROADCAST_STALL.lock().unwrap() == Some(true) {
            // Do an extra check just so we don't log EVERY time.
            warn!("Fault injection: Broadcasting is stalled due to testing directive.";
                      "stacks_block_id" => %new_block.block_id(),
                      "stacks_block_hash" => %new_block.header.block_hash(),
                      "height" => new_block.header.chain_length,
                      "consensus_hash" => %new_block.header.consensus_hash
            );
            while *TEST_BROADCAST_STALL.lock().unwrap() == Some(true) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            info!("Fault injection: Broadcasting is no longer stalled due to testing directive.";
                  "block_id" => %new_block.block_id(),
                  "height" => new_block.header.chain_length,
                  "consensus_hash" => %new_block.header.consensus_hash
            );
        }
    }

    #[cfg(not(test))]
    fn fault_injection_block_broadcast_stall(_ignored: &NakamotoBlock) {}

    #[cfg(test)]
    fn fault_injection_block_announce_stall(new_block: &NakamotoBlock) {
        if *TEST_BLOCK_ANNOUNCE_STALL.lock().unwrap() == Some(true) {
            // Do an extra check just so we don't log EVERY time.
            warn!("Fault injection: Block announcement is stalled due to testing directive.";
                      "stacks_block_id" => %new_block.block_id(),
                      "stacks_block_hash" => %new_block.header.block_hash(),
                      "height" => new_block.header.chain_length,
                      "consensus_hash" => %new_block.header.consensus_hash
            );
            while *TEST_BLOCK_ANNOUNCE_STALL.lock().unwrap() == Some(true) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            info!("Fault injection: Block announcement is no longer stalled due to testing directive.";
                  "block_id" => %new_block.block_id(),
                  "height" => new_block.header.chain_length,
                  "consensus_hash" => %new_block.header.consensus_hash
            );
        }
    }

    #[cfg(not(test))]
    fn fault_injection_block_announce_stall(_ignored: &NakamotoBlock) {}

    #[cfg(test)]
    fn fault_injection_skip_block_broadcast() -> bool {
        if *TEST_SKIP_P2P_BROADCAST.lock().unwrap() == Some(true) {
            return true;
        }
        false
    }

    #[cfg(not(test))]
    fn fault_injection_skip_block_broadcast() -> bool {
        false
    }

    /// Stop a miner tenure by blocking the miner and then joining the tenure thread
    pub fn stop_miner(
        globals: &Globals,
        prior_miner: JoinHandle<Result<(), NakamotoNodeError>>,
    ) -> Result<(), NakamotoNodeError> {
        debug!(
            "Stopping prior miner thread ID {:?}",
            prior_miner.thread().id()
        );
        globals.block_miner();
        let prior_miner_result = prior_miner
            .join()
            .map_err(|_| NakamotoNodeError::MiningFailure(ChainstateError::MinerAborted))?;
        if let Err(e) = prior_miner_result {
            // it's okay if the prior miner thread exited with an error.
            // in many cases this is expected (i.e., a burnchain block occurred)
            // if some error condition should be handled though, this is the place
            //  to do that handling.
            debug!("Prior mining thread exited with: {e:?}");
        }
        globals.unblock_miner();
        Ok(())
    }

    pub fn run_miner(
        mut self,
        prior_miner: Option<JoinHandle<Result<(), NakamotoNodeError>>>,
    ) -> Result<(), NakamotoNodeError> {
        // when starting a new tenure, block the mining thread if its currently running.
        // the new mining thread will join it (so that the new mining thread stalls, not the relayer)
        debug!(
            "New miner thread starting";
            "had_prior_miner" => prior_miner.is_some(),
            "parent_tenure_id" => %self.parent_tenure_id,
            "thread_id" => ?thread::current().id(),
            "burn_block_consensus_hash" => %self.burn_block.consensus_hash,
            "reason" => %self.reason,
        );
        if let Some(prior_miner) = prior_miner {
            Self::stop_miner(&self.globals, prior_miner)?;
        }
        let mut stackerdbs = StackerDBs::connect(&self.config.get_stacker_db_file_path(), true)
            .map_err(|e| NakamotoNodeError::MiningFailure(ChainstateError::NetError(e)))?;

        // now, actually run this tenure
        loop {
            #[cfg(test)]
            if *TEST_MINE_STALL.lock().unwrap() == Some(true) {
                // Do an extra check just so we don't log EVERY time.
                warn!("Mining is stalled due to testing directive");
                while *TEST_MINE_STALL.lock().unwrap() == Some(true) {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                warn!("Mining is no longer stalled due to testing directive. Continuing...");
            }
            let new_block = loop {
                // If we're mock mining, we may not have processed the block that the
                // actual tenure winner committed to yet. So, before attempting to
                // mock mine, check if the parent is processed.
                if self.config.get_node_config(false).mock_mining {
                    let burn_db_path = self.config.get_burn_db_file_path();
                    let mut burn_db = SortitionDB::open(
                        &burn_db_path,
                        true,
                        self.burnchain.pox_constants.clone(),
                    )
                    .expect("FATAL: could not open sortition DB");
                    let burn_tip_changed = self.check_burn_tip_changed(&burn_db);
                    let mut chain_state = neon_node::open_chainstate_with_faults(&self.config)
                        .expect("FATAL: could not open chainstate DB");
                    match burn_tip_changed
                        .and_then(|_| self.load_block_parent_info(&mut burn_db, &mut chain_state))
                    {
                        Ok(..) => {}
                        Err(NakamotoNodeError::ParentNotFound) => {
                            info!("Mock miner has not processed parent block yet, sleeping and trying again");
                            thread::sleep(Duration::from_millis(ABORT_TRY_AGAIN_MS));
                            continue;
                        }
                        Err(e) => {
                            warn!("Mock miner failed to load parent info: {e:?}");
                            return Err(e);
                        }
                    }
                }

                match self.mine_block() {
                    Ok(x) => {
                        if !self.validate_timestamp(&x)? {
                            info!("Block mined too quickly. Will try again.";
                                  "block_timestamp" => x.header.timestamp,
                            );
                            continue;
                        }
                        break Some(x);
                    }
                    Err(NakamotoNodeError::MiningFailure(ChainstateError::MinerAborted)) => {
                        info!("Miner interrupted while mining, will try again");
                        // sleep, and try again. if the miner was interrupted because the burnchain
                        // view changed, the next `mine_block()` invocation will error
                        thread::sleep(Duration::from_millis(ABORT_TRY_AGAIN_MS));
                        continue;
                    }
                    Err(NakamotoNodeError::MiningFailure(
                        ChainstateError::NoTransactionsToMine,
                    )) => {
                        debug!("Miner did not find any transactions to mine");
                        break None;
                    }
                    Err(e) => {
                        warn!("Failed to mine block: {e:?}");

                        // try again, in case a new sortition is pending
                        self.globals
                            .raise_initiative(format!("MiningFailure: {:?}", &e));
                        return Err(NakamotoNodeError::MiningFailure(
                            ChainstateError::MinerAborted,
                        ));
                    }
                }
            };

            if let Some(mut new_block) = new_block {
                Self::fault_injection_block_broadcast_stall(&new_block);
                let (reward_set, signer_signature) = match self
                    .gather_signatures(&mut new_block, &mut stackerdbs)
                {
                    Ok(x) => x,
                    Err(e) => match e {
                        NakamotoNodeError::StacksTipChanged => {
                            info!("Stacks tip changed while waiting for signatures";
                                "signer_sighash" => %new_block.header.signer_signature_hash(),
                                "block_height" => new_block.header.chain_length,
                                "consensus_hash" => %new_block.header.consensus_hash,
                            );
                            return Err(e);
                        }
                        NakamotoNodeError::BurnchainTipChanged => {
                            info!("Burnchain tip changed while waiting for signatures";
                                "signer_sighash" => %new_block.header.signer_signature_hash(),
                                "block_height" => new_block.header.chain_length,
                                "consensus_hash" => %new_block.header.consensus_hash,
                            );
                            return Err(e);
                        }
                        _ => {
                            error!("Error while gathering signatures: {e:?}. Will try mining again.";
                                "signer_sighash" => %new_block.header.signer_signature_hash(),
                                "block_height" => new_block.header.chain_length,
                                "consensus_hash" => %new_block.header.consensus_hash,
                            );
                            continue;
                        }
                    },
                };

                new_block.header.signer_signature = signer_signature;
                if let Err(e) = self.broadcast(new_block.clone(), reward_set, &stackerdbs) {
                    warn!("Error accepting own block: {e:?}. Will try mining again.");
                    continue;
                } else {
                    info!(
                        "Miner: Block signed by signer set and broadcasted";
                        "signer_sighash" => %new_block.header.signer_signature_hash(),
                        "stacks_block_hash" => %new_block.header.block_hash(),
                        "stacks_block_id" => %new_block.header.block_id(),
                        "block_height" => new_block.header.chain_length,
                        "consensus_hash" => %new_block.header.consensus_hash,
                    );
                }

                // update mined-block counters and mined-tenure counters
                self.globals.counters.bump_naka_mined_blocks();
                if !self.last_block_mined.is_none() {
                    // this is the first block of the tenure, bump tenure counter
                    self.globals.counters.bump_naka_mined_tenures();
                }

                // wake up chains coordinator
                Self::fault_injection_block_announce_stall(&new_block);
                self.globals.coord().announce_new_stacks_block();

                self.last_block_mined = Some(new_block);
            }

            let Ok(sort_db) = SortitionDB::open(
                &self.config.get_burn_db_file_path(),
                true,
                self.burnchain.pox_constants.clone(),
            ) else {
                error!("Failed to open sortition DB. Will try mining again.");
                continue;
            };

            let wait_start = Instant::now();
            while wait_start.elapsed() < self.config.miner.wait_on_interim_blocks {
                thread::sleep(Duration::from_millis(ABORT_TRY_AGAIN_MS));
                if self.check_burn_tip_changed(&sort_db).is_err() {
                    return Err(NakamotoNodeError::BurnchainTipChanged);
                }
            }
        }
    }

    /// Load the signer set active for this miner's blocks. This is the
    ///  active reward set during `self.burn_election_block`. The miner
    ///  thread caches this information, and this method will consult
    ///  that cache (or populate it if necessary).
    fn load_signer_set(&mut self) -> Result<RewardSet, NakamotoNodeError> {
        if let Some(set) = self.signer_set_cache.as_ref() {
            return Ok(set.clone());
        }
        let sort_db = SortitionDB::open(
            &self.config.get_burn_db_file_path(),
            true,
            self.burnchain.pox_constants.clone(),
        )
        .map_err(|e| {
            NakamotoNodeError::SigningCoordinatorFailure(format!(
                "Failed to open sortition DB. Cannot mine! {e:?}"
            ))
        })?;

        let mut chain_state =
            neon_node::open_chainstate_with_faults(&self.config).map_err(|e| {
                NakamotoNodeError::SigningCoordinatorFailure(format!(
                    "Failed to open chainstate DB. Cannot mine! {e:?}"
                ))
            })?;

        let burn_election_height = self.burn_election_block.block_height;

        let reward_cycle = self
            .burnchain
            .block_height_to_reward_cycle(burn_election_height)
            .expect("FATAL: no reward cycle for sortition");

        let reward_info = match load_nakamoto_reward_set(
            reward_cycle,
            &self.burn_election_block.sortition_id,
            &self.burnchain,
            &mut chain_state,
            &self.parent_tenure_id,
            &sort_db,
            &OnChainRewardSetProvider::new(),
        ) {
            Ok(Some((reward_info, _))) => reward_info,
            Ok(None) => {
                return Err(NakamotoNodeError::SigningCoordinatorFailure(
                    "No reward set stored yet. Cannot mine!".into(),
                ));
            }
            Err(e) => {
                return Err(NakamotoNodeError::SigningCoordinatorFailure(format!(
                    "Failure while fetching reward set. Cannot initialize miner coordinator. {e:?}"
                )));
            }
        };

        let Some(reward_set) = reward_info.known_selected_anchor_block_owned() else {
            return Err(NakamotoNodeError::SigningCoordinatorFailure(
                "Current reward cycle did not select a reward set. Cannot mine!".into(),
            ));
        };

        self.signer_set_cache = Some(reward_set.clone());
        Ok(reward_set)
    }

    /// Gather a list of signatures from the signers for the block
    fn gather_signatures(
        &mut self,
        new_block: &mut NakamotoBlock,
        stackerdbs: &mut StackerDBs,
    ) -> Result<(RewardSet, Vec<MessageSignature>), NakamotoNodeError> {
        let Some(miner_privkey) = self.config.miner.mining_key else {
            return Err(NakamotoNodeError::MinerConfigurationFailed(
                "No mining key configured, cannot mine",
            ));
        };
        let sort_db = SortitionDB::open(
            &self.config.get_burn_db_file_path(),
            true,
            self.burnchain.pox_constants.clone(),
        )
        .map_err(|e| {
            NakamotoNodeError::SigningCoordinatorFailure(format!(
                "Failed to open sortition DB. Cannot mine! {e:?}"
            ))
        })?;

        let reward_set = self.load_signer_set()?;

        if self.config.get_node_config(false).mock_mining {
            return Ok((reward_set, Vec::new()));
        }

        let mut coordinator = SignCoordinator::new(
            &reward_set,
            miner_privkey,
            &self.config,
            self.globals.should_keep_running.clone(),
        )
        .map_err(|e| {
            NakamotoNodeError::SigningCoordinatorFailure(format!(
                "Failed to initialize the signing coordinator. Cannot mine! {e:?}"
            ))
        })?;

        let mut chain_state =
            neon_node::open_chainstate_with_faults(&self.config).map_err(|e| {
                NakamotoNodeError::SigningCoordinatorFailure(format!(
                    "Failed to open chainstate DB. Cannot mine! {e:?}"
                ))
            })?;

        let signature = coordinator.run_sign_v0(
            new_block,
            &self.burn_block,
            &self.burnchain,
            &sort_db,
            &mut chain_state,
            &stackerdbs,
            &self.globals.counters,
            &self.burn_election_block.consensus_hash,
        )?;

        return Ok((reward_set, signature));
    }

    /// Fault injection -- possibly fail to broadcast
    /// Return true to drop the block
    fn fault_injection_broadcast_fail(&self) -> bool {
        let drop_prob = self
            .config
            .node
            .fault_injection_block_push_fail_probability
            .unwrap_or(0)
            .min(100);
        let will_drop = if drop_prob > 0 {
            let throw: u8 = thread_rng().gen_range(0..100);
            throw < drop_prob
        } else {
            false
        };
        will_drop
    }

    /// Store a block to the chainstate, and if successful (it should be since we mined it),
    /// broadcast it via the p2p network.
    fn broadcast_p2p(
        &mut self,
        sort_db: &SortitionDB,
        chain_state: &mut StacksChainState,
        block: &NakamotoBlock,
        reward_set: RewardSet,
    ) -> Result<(), ChainstateError> {
        if Self::fault_injection_skip_block_broadcast() {
            warn!(
                "Fault injection: Skipping block broadcast for {}",
                block.block_id()
            );
            return Ok(());
        }

        let mut sortition_handle = sort_db.index_handle_at_ch(&block.header.consensus_hash)?;
        let chainstate_config = chain_state.config();
        let (headers_conn, staging_tx) = chain_state.headers_conn_and_staging_tx_begin()?;
        let accepted = NakamotoChainState::accept_block(
            &chainstate_config,
            &block,
            &mut sortition_handle,
            &staging_tx,
            headers_conn,
            reward_set,
            NakamotoBlockObtainMethod::Mined,
        )?;
        staging_tx.commit()?;

        if !accepted {
            // this can happen if the p2p network and relayer manage to receive this block prior to
            // the thread reaching this point -- this can happen because the signers broadcast the
            // signed block to the nodes independent of the miner, so the miner itself can receive
            // and store its own block outside of this thread.
            debug!("Did NOT accept block {} we mined", &block.block_id());

            // not much we can do here, but try and mine again and hope we produce a valid one.
            return Ok(());
        }

        // forward to p2p thread, but do fault injection
        if self.fault_injection_broadcast_fail() {
            info!("Fault injection: drop block {}", &block.block_id());
            return Ok(());
        }

        let block_id = block.block_id();
        debug!("Broadcasting block {}", &block_id);
        if let Err(e) = self.p2p_handle.broadcast_message(
            vec![],
            StacksMessageType::NakamotoBlocks(NakamotoBlocksData {
                blocks: vec![block.clone()],
            }),
        ) {
            warn!("Failed to broadcast block {}: {:?}", &block_id, &e);
        }
        Ok(())
    }

    fn broadcast(
        &mut self,
        block: NakamotoBlock,
        reward_set: RewardSet,
        stackerdbs: &StackerDBs,
    ) -> Result<(), NakamotoNodeError> {
        let mut chain_state = neon_node::open_chainstate_with_faults(&self.config)
            .expect("FATAL: could not open chainstate DB");
        let sort_db = SortitionDB::open(
            &self.config.get_burn_db_file_path(),
            true,
            self.burnchain.pox_constants.clone(),
        )
        .expect("FATAL: could not open sortition DB");

        if self.config.miner.mining_key.is_none() {
            return Err(NakamotoNodeError::MinerConfigurationFailed(
                "No mining key configured, cannot mine",
            ));
        };

        // push block via p2p block push
        self.broadcast_p2p(&sort_db, &mut chain_state, &block, reward_set)
            .map_err(NakamotoNodeError::AcceptFailure)?;

        let Some(ref miner_privkey) = self.config.miner.mining_key else {
            // should be unreachable, but we can't borrow this above broadcast_p2p() since it's
            // mutable
            return Err(NakamotoNodeError::MinerConfigurationFailed(
                "No mining key configured, cannot mine",
            ));
        };

        // also, push block via stackerdb to make sure stackers get it
        let rpc_socket = self.config.node.get_rpc_loopback().ok_or_else(|| {
            NakamotoNodeError::MinerConfigurationFailed("Failed to get RPC loopback socket")
        })?;
        let miners_contract_id = boot_code_id(MINERS_NAME, chain_state.mainnet);
        let mut miners_session = StackerDBSession::new(&rpc_socket.to_string(), miners_contract_id);

        SignCoordinator::send_miners_message(
            miner_privkey,
            &sort_db,
            &self.burn_block,
            &stackerdbs,
            SignerMessage::BlockPushed(block),
            MinerSlotID::BlockPushed,
            chain_state.mainnet,
            &mut miners_session,
            &self.burn_election_block.consensus_hash,
        )
        .map_err(NakamotoNodeError::SigningCoordinatorFailure)
    }

    /// Get the coinbase recipient address, if set in the config and if allowed in this epoch
    fn get_coinbase_recipient(&self, epoch_id: StacksEpochId) -> Option<PrincipalData> {
        if epoch_id < StacksEpochId::Epoch21 && self.config.miner.block_reward_recipient.is_some() {
            warn!("Coinbase pay-to-contract is not supported in the current epoch");
            None
        } else {
            self.config.miner.block_reward_recipient.clone()
        }
    }

    fn generate_tenure_change_tx(
        &self,
        nonce: u64,
        payload: TenureChangePayload,
    ) -> Result<StacksTransaction, NakamotoNodeError> {
        let is_mainnet = self.config.is_mainnet();
        let chain_id = self.config.burnchain.chain_id;
        let tenure_change_tx_payload = TransactionPayload::TenureChange(payload);

        let mut tx_auth = self.keychain.get_transaction_auth().unwrap();
        tx_auth.set_origin_nonce(nonce);

        let version = if is_mainnet {
            TransactionVersion::Mainnet
        } else {
            TransactionVersion::Testnet
        };

        let mut tx = StacksTransaction::new(version, tx_auth, tenure_change_tx_payload);

        tx.chain_id = chain_id;
        tx.anchor_mode = TransactionAnchorMode::OnChainOnly;
        let mut tx_signer = StacksTransactionSigner::new(&tx);
        self.keychain.sign_as_origin(&mut tx_signer);

        Ok(tx_signer.get_tx().unwrap())
    }

    /// Create a coinbase transaction.
    fn generate_coinbase_tx(
        &self,
        nonce: u64,
        epoch_id: StacksEpochId,
        vrf_proof: VRFProof,
    ) -> StacksTransaction {
        let is_mainnet = self.config.is_mainnet();
        let chain_id = self.config.burnchain.chain_id;
        let mut tx_auth = self.keychain.get_transaction_auth().unwrap();
        tx_auth.set_origin_nonce(nonce);

        let version = if is_mainnet {
            TransactionVersion::Mainnet
        } else {
            TransactionVersion::Testnet
        };

        let recipient_opt = self.get_coinbase_recipient(epoch_id);

        let mut tx = StacksTransaction::new(
            version,
            tx_auth,
            TransactionPayload::Coinbase(
                CoinbasePayload([0u8; 32]),
                recipient_opt,
                Some(vrf_proof),
            ),
        );
        tx.chain_id = chain_id;
        tx.anchor_mode = TransactionAnchorMode::OnChainOnly;
        let mut tx_signer = StacksTransactionSigner::new(&tx);
        self.keychain.sign_as_origin(&mut tx_signer);

        tx_signer.get_tx().unwrap()
    }

    // TODO: add tests from mutation testing results #4869
    #[cfg_attr(test, mutants::skip)]
    /// Load up the parent block info for mining.
    /// If we can't find the parent in the DB but we expect one, return Err(ParentNotFound).
    fn load_block_parent_info(
        &self,
        burn_db: &mut SortitionDB,
        chain_state: &mut StacksChainState,
    ) -> Result<ParentStacksBlockInfo, NakamotoNodeError> {
        // load up stacks chain tip
        let (stacks_tip_ch, stacks_tip_bh) =
            SortitionDB::get_canonical_stacks_chain_tip_hash(burn_db.conn()).map_err(|e| {
                error!("Failed to load canonical Stacks tip: {:?}", &e);
                NakamotoNodeError::ParentNotFound
            })?;

        let stacks_tip_block_id = StacksBlockId::new(&stacks_tip_ch, &stacks_tip_bh);
        let tenure_tip_opt = NakamotoChainState::get_highest_block_header_in_tenure(
            &mut chain_state.index_conn(),
            &stacks_tip_block_id,
            &self.burn_election_block.consensus_hash,
        )
        .map_err(|e| {
            error!(
                "Could not query header info for tenure tip {} off of {}: {:?}",
                &self.burn_election_block.consensus_hash, &stacks_tip_block_id, &e
            );
            NakamotoNodeError::ParentNotFound
        })?;

        // The nakamoto miner must always build off of a chain tip that is the highest of:
        // 1. The highest block in the miner's current tenure
        // 2. The highest block in the current tenure's parent tenure
        //
        // Where the current tenure's parent tenure is the tenure start block committed to in the current tenure's associated block commit.
        let stacks_tip_header = if let Some(tenure_tip) = tenure_tip_opt {
            debug!(
                "Stacks block parent ID is last block in tenure ID {}",
                &tenure_tip.consensus_hash
            );
            tenure_tip
        } else {
            // This tenure is empty on the canonical fork, so mine the first tenure block.
            debug!(
                "Stacks block parent ID is last block in parent tenure tipped by {}",
                &self.parent_tenure_id
            );

            // find the last block in the parent tenure, since this is the tip we'll build atop
            let parent_tenure_header =
                NakamotoChainState::get_block_header(chain_state.db(), &self.parent_tenure_id)
                    .map_err(|e| {
                        error!(
                            "Could not query header for parent tenure ID {}: {:?}",
                            &self.parent_tenure_id, &e
                        );
                        NakamotoNodeError::ParentNotFound
                    })?
                    .ok_or_else(|| {
                        error!("No header for parent tenure ID {}", &self.parent_tenure_id);
                        NakamotoNodeError::ParentNotFound
                    })?;

            let header_opt = NakamotoChainState::get_highest_block_header_in_tenure(
                &mut chain_state.index_conn(),
                &stacks_tip_block_id,
                &parent_tenure_header.consensus_hash,
            )
            .map_err(|e| {
                error!("Could not query parent tenure finish block: {:?}", &e);
                NakamotoNodeError::ParentNotFound
            })?;
            if let Some(header) = header_opt {
                header
            } else {
                // this is an epoch2 block
                debug!(
                    "Stacks block parent ID may be an epoch2x block: {}",
                    &self.parent_tenure_id
                );
                let epoch2_header =
                    NakamotoChainState::get_block_header(chain_state.db(), &self.parent_tenure_id)
                        .map_err(|e| {
                            error!(
                                "Could not query header info for epoch2x tenure block ID {}: {:?}",
                                &self.parent_tenure_id, &e
                            );
                            NakamotoNodeError::ParentNotFound
                        })?
                        .ok_or_else(|| {
                            error!(
                                "No header info for epoch2x tenure block ID {}",
                                &self.parent_tenure_id
                            );
                            NakamotoNodeError::ParentNotFound
                        })?;

                epoch2_header
            }
        };

        debug!(
            "Miner: stacks tip parent header is {} {:?}",
            &stacks_tip_header.index_block_hash(),
            &stacks_tip_header
        );
        let miner_address = self
            .keychain
            .origin_address(self.config.is_mainnet())
            .unwrap();
        match ParentStacksBlockInfo::lookup(
            chain_state,
            burn_db,
            &self.burn_block,
            miner_address,
            &self.parent_tenure_id,
            stacks_tip_header,
        ) {
            Ok(parent_info) => Ok(parent_info),
            Err(NakamotoNodeError::BurnchainTipChanged) => {
                self.globals.counters.bump_missed_tenures();
                Err(NakamotoNodeError::BurnchainTipChanged)
            }
            Err(e) => Err(e),
        }
    }

    /// Generate the VRF proof for the block we're going to build.
    /// Returns Some(proof) if we could make the proof
    /// Return None if we could not make the proof
    fn make_vrf_proof(&mut self) -> Option<VRFProof> {
        // if we're a mock miner, then make sure that the keychain has a keypair for the mocked VRF
        // key
        let vrf_proof = if self.config.get_node_config(false).mock_mining {
            self.keychain.generate_proof(
                VRF_MOCK_MINER_KEY,
                self.burn_block.sortition_hash.as_bytes(),
            )
        } else {
            self.keychain.generate_proof(
                self.registered_key.target_block_height,
                self.burn_block.sortition_hash.as_bytes(),
            )
        };

        debug!(
            "Generated VRF Proof: {} over {} ({},{}) with key {}",
            vrf_proof.to_hex(),
            &self.burn_block.sortition_hash,
            &self.burn_block.block_height,
            &self.burn_block.burn_header_hash,
            &self.registered_key.vrf_public_key.to_hex()
        );
        Some(vrf_proof)
    }

    fn validate_timestamp_info(
        &self,
        current_timestamp_secs: u64,
        stacks_parent_header: &StacksHeaderInfo,
    ) -> bool {
        let parent_timestamp = match stacks_parent_header.anchored_header.as_stacks_nakamoto() {
            Some(naka_header) => naka_header.timestamp,
            None => stacks_parent_header.burn_header_timestamp,
        };
        let time_since_parent_ms = current_timestamp_secs.saturating_sub(parent_timestamp) * 1000;
        if time_since_parent_ms < self.config.miner.min_time_between_blocks_ms {
            debug!("Parent block mined {time_since_parent_ms} ms ago. Required minimum gap between blocks is {} ms", self.config.miner.min_time_between_blocks_ms;
                "current_timestamp" => current_timestamp_secs,
                "parent_block_id" => %stacks_parent_header.index_block_hash(),
                "parent_block_height" => stacks_parent_header.stacks_block_height,
                "parent_block_timestamp" => stacks_parent_header.burn_header_timestamp,
            );
            false
        } else {
            true
        }
    }

    /// Check that the provided block is not mined too quickly after the parent block.
    /// This is to ensure that the signers do not reject the block due to the block being mined within the same second as the parent block.
    fn validate_timestamp(&self, x: &NakamotoBlock) -> Result<bool, NakamotoNodeError> {
        let chain_state = neon_node::open_chainstate_with_faults(&self.config)
            .expect("FATAL: could not open chainstate DB");
        let stacks_parent_header =
            NakamotoChainState::get_block_header(chain_state.db(), &x.header.parent_block_id)
                .map_err(|e| {
                    error!(
                        "Could not query header info for parent block ID {}: {:?}",
                        &x.header.parent_block_id, &e
                    );
                    NakamotoNodeError::ParentNotFound
                })?
                .ok_or_else(|| {
                    error!(
                        "No header info for parent block ID {}",
                        &x.header.parent_block_id
                    );
                    NakamotoNodeError::ParentNotFound
                })?;
        Ok(self.validate_timestamp_info(x.header.timestamp, &stacks_parent_header))
    }

    // TODO: add tests from mutation testing results #4869
    #[cfg_attr(test, mutants::skip)]
    /// Try to mine a Stacks block by assembling one from mempool transactions and sending a
    /// burnchain block-commit transaction.  If we succeed, then return the assembled block.
    fn mine_block(&mut self) -> Result<NakamotoBlock, NakamotoNodeError> {
        debug!("block miner thread ID is {:?}", thread::current().id());

        let burn_db_path = self.config.get_burn_db_file_path();
        let reward_set = self.load_signer_set()?;

        // NOTE: read-write access is needed in order to be able to query the recipient set.
        // This is an artifact of the way the MARF is built (see #1449)
        let mut burn_db =
            SortitionDB::open(&burn_db_path, true, self.burnchain.pox_constants.clone())
                .expect("FATAL: could not open sortition DB");

        self.check_burn_tip_changed(&burn_db)?;
        neon_node::fault_injection_long_tenure();

        let mut chain_state = neon_node::open_chainstate_with_faults(&self.config)
            .expect("FATAL: could not open chainstate DB");

        let mut mem_pool = self
            .config
            .connect_mempool_db()
            .expect("Database failure opening mempool");

        let target_epoch_id =
            SortitionDB::get_stacks_epoch(burn_db.conn(), self.burn_block.block_height + 1)
                .map_err(|_| NakamotoNodeError::SnapshotNotFoundForChainTip)?
                .expect("FATAL: no epoch defined")
                .epoch_id;
        let mut parent_block_info = self.load_block_parent_info(&mut burn_db, &mut chain_state)?;
        let vrf_proof = self
            .make_vrf_proof()
            .ok_or_else(|| NakamotoNodeError::BadVrfConstruction)?;

        if self.last_block_mined.is_none() && parent_block_info.parent_tenure.is_none() {
            warn!("Miner should be starting a new tenure, but failed to load parent tenure info");
            return Err(NakamotoNodeError::ParentNotFound);
        };

        // create our coinbase if this is the first block we've mined this tenure
        let tenure_start_info = self.make_tenure_start_info(
            &chain_state,
            &parent_block_info,
            vrf_proof,
            target_epoch_id,
        )?;

        parent_block_info.stacks_parent_header.microblock_tail = None;

        let signer_bitvec_len = reward_set.rewarded_addresses.len().try_into().ok();

        if !self.validate_timestamp_info(
            get_epoch_time_secs(),
            &parent_block_info.stacks_parent_header,
        ) {
            // treat a too-soon-to-mine block as an interrupt: this will let the caller sleep and then re-evaluate
            //  all the pre-mining checks (burnchain tip changes, signal interrupts, etc.)
            return Err(NakamotoNodeError::MiningFailure(
                ChainstateError::MinerAborted,
            ));
        }

        // build the block itself
        let (mut block, consumed, size, tx_events) = NakamotoBlockBuilder::build_nakamoto_block(
            &chain_state,
            &burn_db
                .index_handle_at_ch(&self.burn_block.consensus_hash)
                .map_err(|_| NakamotoNodeError::UnexpectedChainState)?,
            &mut mem_pool,
            &parent_block_info.stacks_parent_header,
            &self.burn_election_block.consensus_hash,
            self.burn_block.total_burn,
            tenure_start_info,
            self.config
                .make_nakamoto_block_builder_settings(self.globals.get_miner_status()),
            // we'll invoke the event dispatcher ourselves so that it calculates the
            //  correct signer_sighash for `process_mined_nakamoto_block_event`
            Some(&self.event_dispatcher),
            signer_bitvec_len.unwrap_or(0),
        )
        .map_err(|e| {
            if !matches!(
                e,
                ChainstateError::MinerAborted | ChainstateError::NoTransactionsToMine
            ) {
                error!("Relayer: Failure mining anchored block: {e}");
            }
            NakamotoNodeError::MiningFailure(e)
        })?;

        if block.txs.is_empty() {
            return Err(NakamotoNodeError::MiningFailure(
                ChainstateError::NoTransactionsToMine,
            ));
        }
        let mining_key = self.keychain.get_nakamoto_sk();
        let miner_signature = mining_key
            .sign(block.header.miner_signature_hash().as_bytes())
            .map_err(NakamotoNodeError::MinerSignatureError)?;
        block.header.miner_signature = miner_signature;

        info!(
            "Miner: Assembled block #{} for signer set proposal: {}, with {} txs",
            block.header.chain_length,
            block.header.block_hash(),
            block.txs.len();
            "signer_sighash" => %block.header.signer_signature_hash(),
            "consensus_hash" => %block.header.consensus_hash,
            "parent_block_id" => %block.header.parent_block_id,
            "timestamp" => block.header.timestamp,
        );

        self.event_dispatcher.process_mined_nakamoto_block_event(
            self.burn_block.block_height,
            &block,
            size,
            &consumed,
            tx_events,
        );

        // last chance -- confirm that the stacks tip is unchanged (since it could have taken long
        // enough to build this block that another block could have arrived), and confirm that all
        // Stacks blocks with heights higher than the canonical tip are processed.
        self.check_burn_tip_changed(&burn_db)?;
        Ok(block)
    }

    #[cfg_attr(test, mutants::skip)]
    /// Create the tenure start info for the block we're going to build
    fn make_tenure_start_info(
        &self,
        chainstate: &StacksChainState,
        parent_block_info: &ParentStacksBlockInfo,
        vrf_proof: VRFProof,
        target_epoch_id: StacksEpochId,
    ) -> Result<NakamotoTenureInfo, NakamotoNodeError> {
        let current_miner_nonce = parent_block_info.coinbase_nonce;
        let Some(parent_tenure_info) = &parent_block_info.parent_tenure else {
            return Ok(NakamotoTenureInfo {
                coinbase_tx: None,
                tenure_change_tx: None,
            });
        };
        if self.last_block_mined.is_some() {
            return Ok(NakamotoTenureInfo {
                coinbase_tx: None,
                tenure_change_tx: None,
            });
        }

        let parent_block_id = parent_block_info.stacks_parent_header.index_block_hash();
        let mut payload = TenureChangePayload {
            tenure_consensus_hash: self.burn_election_block.consensus_hash.clone(),
            prev_tenure_consensus_hash: parent_tenure_info.parent_tenure_consensus_hash,
            burn_view_consensus_hash: self.burn_election_block.consensus_hash.clone(),
            previous_tenure_end: parent_block_id,
            previous_tenure_blocks: u32::try_from(parent_tenure_info.parent_tenure_blocks)
                .expect("FATAL: more than u32 blocks in a tenure"),
            cause: TenureChangeCause::BlockFound,
            pubkey_hash: self.keychain.get_nakamoto_pkh(),
        };

        let (tenure_change_tx, coinbase_tx) = match &self.reason {
            MinerReason::BlockFound => {
                let tenure_change_tx =
                    self.generate_tenure_change_tx(current_miner_nonce, payload)?;
                let coinbase_tx =
                    self.generate_coinbase_tx(current_miner_nonce + 1, target_epoch_id, vrf_proof);
                (Some(tenure_change_tx), Some(coinbase_tx))
            }
            MinerReason::Extended {
                burn_view_consensus_hash,
            } => {
                let num_blocks_so_far = NakamotoChainState::get_nakamoto_tenure_length(
                    chainstate.db(),
                    &parent_block_id,
                )
                .map_err(NakamotoNodeError::MiningFailure)?;
                info!("Miner: Extending tenure";
                      "burn_view_consensus_hash" => %burn_view_consensus_hash,
                      "parent_block_id" => %parent_block_id,
                      "num_blocks_so_far" => num_blocks_so_far,
                );
                payload = payload.extend(
                    *burn_view_consensus_hash,
                    parent_block_id,
                    num_blocks_so_far,
                );
                let tenure_change_tx =
                    self.generate_tenure_change_tx(current_miner_nonce, payload)?;
                (Some(tenure_change_tx), None)
            }
        };

        Ok(NakamotoTenureInfo {
            coinbase_tx,
            tenure_change_tx,
        })
    }

    /// Check if the tenure needs to change -- if so, return a BurnchainTipChanged error
    /// The tenure should change if there is a new burnchain tip with a valid sortition
    fn check_burn_tip_changed(&self, sortdb: &SortitionDB) -> Result<(), NakamotoNodeError> {
        let cur_burn_chain_tip = SortitionDB::get_canonical_burn_chain_tip(sortdb.conn())
            .expect("FATAL: failed to query sortition DB for canonical burn chain tip");

        if cur_burn_chain_tip.consensus_hash != self.burn_block.consensus_hash {
            info!("Miner: Cancel block assembly; burnchain tip has changed");
            self.globals.counters.bump_missed_tenures();
            Err(NakamotoNodeError::BurnchainTipChanged)
        } else {
            Ok(())
        }
    }
}

impl ParentStacksBlockInfo {
    // TODO: add tests from mutation testing results #4869
    #[cfg_attr(test, mutants::skip)]
    /// Determine where in the set of forks to attempt to mine the next anchored block.
    /// `mine_tip_ch` and `mine_tip_bhh` identify the parent block on top of which to mine.
    /// `check_burn_block` identifies what we believe to be the burn chain's sortition history tip.
    /// This is used to mitigate (but not eliminate) a TOCTTOU issue with mining: the caller's
    /// conception of the sortition history tip may have become stale by the time they call this
    /// method, in which case, mining should *not* happen (since the block will be invalid).
    pub fn lookup(
        chain_state: &mut StacksChainState,
        burn_db: &mut SortitionDB,
        check_burn_block: &BlockSnapshot,
        miner_address: StacksAddress,
        parent_tenure_id: &StacksBlockId,
        stacks_tip_header: StacksHeaderInfo,
    ) -> Result<ParentStacksBlockInfo, NakamotoNodeError> {
        // the stacks block I'm mining off of's burn header hash and vtxindex:
        let parent_snapshot = SortitionDB::get_block_snapshot_consensus(
            burn_db.conn(),
            &stacks_tip_header.consensus_hash,
        )
        .expect("Failed to look up block's parent snapshot")
        .expect("Failed to look up block's parent snapshot");

        // don't mine off of an old burnchain block
        let burn_chain_tip = SortitionDB::get_canonical_burn_chain_tip(burn_db.conn())
            .expect("FATAL: failed to query sortition DB for canonical burn chain tip");

        if burn_chain_tip.consensus_hash != check_burn_block.consensus_hash {
            info!(
                "New canonical burn chain tip detected. Will not try to mine.";
                "new_consensus_hash" => %burn_chain_tip.consensus_hash,
                "old_consensus_hash" => %check_burn_block.consensus_hash,
                "new_burn_height" => burn_chain_tip.block_height,
                "old_burn_height" => check_burn_block.block_height
            );
            return Err(NakamotoNodeError::BurnchainTipChanged);
        }

        let Ok(Some(parent_tenure_header)) =
            NakamotoChainState::get_block_header(chain_state.db(), &parent_tenure_id)
        else {
            warn!("Failed loading parent tenure ID"; "parent_tenure_id" => %parent_tenure_id);
            return Err(NakamotoNodeError::ParentNotFound);
        };

        // check if we're mining a first tenure block (by checking if our parent block is in the tenure of parent_tenure_id)
        //  and if so, figure out how many blocks there were in the parent tenure
        let parent_tenure_info = if stacks_tip_header.consensus_hash
            == parent_tenure_header.consensus_hash
        {
            // in the same tenure
            let parent_tenure_blocks = if parent_tenure_header
                .anchored_header
                .as_stacks_nakamoto()
                .is_some()
            {
                let Ok(Some(last_parent_tenure_header)) =
                    NakamotoChainState::get_highest_block_header_in_tenure(
                        &mut chain_state.index_conn(),
                        &stacks_tip_header.index_block_hash(),
                        &parent_tenure_header.consensus_hash,
                    )
                else {
                    warn!("Failed loading last block of parent tenure"; "parent_tenure_id" => %parent_tenure_id);
                    return Err(NakamotoNodeError::ParentNotFound);
                };
                // the last known tenure block of our parent should be the stacks_tip. if not, error.
                if stacks_tip_header.index_block_hash()
                    != last_parent_tenure_header.index_block_hash()
                {
                    warn!("Last known tenure block of parent tenure should be the stacks tip";
                          "stacks_tip_header" => %stacks_tip_header.index_block_hash(),
                          "last_parent_tenure_header" => %last_parent_tenure_header.index_block_hash());
                    return Err(NakamotoNodeError::NewParentDiscovered);
                }
                1 + last_parent_tenure_header.stacks_block_height
                    - parent_tenure_header.stacks_block_height
            } else {
                1
            };
            let parent_tenure_consensus_hash = parent_tenure_header.consensus_hash.clone();
            Some(ParentTenureInfo {
                parent_tenure_blocks,
                parent_tenure_consensus_hash,
            })
        } else {
            None
        };

        debug!(
            "Looked up parent information";
            "parent_tenure_id" => %parent_tenure_id,
            "parent_tenure_consensus_hash" => %parent_tenure_header.consensus_hash,
            "parent_tenure_burn_hash" => %parent_tenure_header.burn_header_hash,
            "parent_tenure_burn_height" => parent_tenure_header.burn_header_height,
            "mining_consensus_hash" => %check_burn_block.consensus_hash,
            "mining_burn_hash" => %check_burn_block.burn_header_hash,
            "mining_burn_height" => check_burn_block.block_height,
            "stacks_tip_consensus_hash" => %parent_snapshot.consensus_hash,
            "stacks_tip_burn_hash" => %parent_snapshot.burn_header_hash,
            "stacks_tip_burn_height" => parent_snapshot.block_height,
        );

        let coinbase_nonce = {
            let principal = miner_address.into();
            let account = chain_state
                .with_read_only_clarity_tx(
                    &burn_db
                        .index_handle_at_block(&chain_state, &stacks_tip_header.index_block_hash())
                        .map_err(|_| NakamotoNodeError::UnexpectedChainState)?,
                    &stacks_tip_header.index_block_hash(),
                    |conn| StacksChainState::get_account(conn, &principal),
                )
                .unwrap_or_else(|| {
                    panic!(
                        "BUG: stacks tip block {} no longer exists after we queried it",
                        &stacks_tip_header.index_block_hash()
                    )
                });
            account.nonce
        };

        Ok(ParentStacksBlockInfo {
            stacks_parent_header: stacks_tip_header,
            coinbase_nonce,
            parent_tenure: parent_tenure_info,
        })
    }
}