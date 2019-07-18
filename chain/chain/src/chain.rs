use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration as TimeDuration, Instant};

use chrono::prelude::{DateTime, Utc};
use chrono::Duration;
use log::{debug, info};

use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{ReceiptTransaction, TransactionResult};
use near_primitives::types::{AccountId, BlockIndex, MerkleHash, ShardId};
use near_store::Store;

use crate::error::{Error, ErrorKind};
use crate::store::{ChainStore, ChainStoreAccess, ChainStoreUpdate};
use crate::types::{
    Block, BlockHeader, BlockStatus, Provenance, RuntimeAdapter, ShardFullChunkOrOnePart, Tip,
};
use near_primitives::sharding::{ChunkHash, ShardChunk, ShardChunkHeader};

/// Maximum number of orphans chain can store.
pub const MAX_ORPHAN_SIZE: usize = 1024;

/// Maximum age of orhpan to store in the chain.
const MAX_ORPHAN_AGE_SECS: u64 = 300;

/// Refuse blocks more than this many block intervals in the future (as in bitcoin).
const ACCEPTABLE_TIME_DIFFERENCE: i64 = 12 * 10;

pub struct Orphan {
    block: Block,
    provenance: Provenance,
    added: Instant,
}

pub struct OrphanBlockPool {
    orphans: HashMap<CryptoHash, Orphan>,
    height_idx: HashMap<u64, Vec<CryptoHash>>,
    evicted: usize,
}

impl OrphanBlockPool {
    fn new() -> OrphanBlockPool {
        OrphanBlockPool { orphans: HashMap::default(), height_idx: HashMap::default(), evicted: 0 }
    }

    fn len(&self) -> usize {
        self.orphans.len()
    }

    fn len_evicted(&self) -> usize {
        self.evicted
    }

    fn add(&mut self, orphan: Orphan) {
        let height_hashes = self.height_idx.entry(orphan.block.header.height).or_insert(vec![]);
        height_hashes.push(orphan.block.hash());
        self.orphans.insert(orphan.block.hash(), orphan);

        if self.orphans.len() > MAX_ORPHAN_SIZE {
            let old_len = self.orphans.len();

            self.orphans.retain(|_, ref mut x| {
                x.added.elapsed() < TimeDuration::from_secs(MAX_ORPHAN_AGE_SECS)
            });
            let mut heights = self.height_idx.keys().cloned().collect::<Vec<u64>>();
            heights.sort_unstable();
            let mut removed_hashes: HashSet<CryptoHash> = HashSet::default();
            for h in heights.iter().rev() {
                if let Some(hash) = self.height_idx.remove(h) {
                    for h in hash {
                        let _ = self.orphans.remove(&h);
                        removed_hashes.insert(h);
                    }
                }
                if self.orphans.len() < MAX_ORPHAN_SIZE {
                    break;
                }
            }
            self.height_idx.retain(|_, ref mut xs| xs.iter().any(|x| !removed_hashes.contains(&x)));

            self.evicted += old_len - self.orphans.len();
        }
    }

    pub fn contains(&self, hash: &CryptoHash) -> bool {
        self.orphans.contains_key(hash)
    }

    pub fn remove_by_height(&mut self, height: BlockIndex) -> Option<Vec<Orphan>> {
        self.height_idx
            .remove(&height)
            .map(|hs| hs.iter().filter_map(|h| self.orphans.remove(h)).collect())
    }

    pub fn all_heights(&self) -> Vec<u64> {
        self.height_idx.keys().cloned().collect()
    }
}

/// Facade to the blockchain block processing and storage.
/// Provides current view on the state according to the chain state.
pub struct Chain {
    store: ChainStore,
    runtime_adapter: Arc<dyn RuntimeAdapter>,
    orphans: OrphanBlockPool,
    blocks_with_missing_chunks: OrphanBlockPool,
    genesis: BlockHeader,
}

impl Chain {
    pub fn new(
        store: Arc<Store>,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        genesis_time: DateTime<Utc>,
    ) -> Result<Chain, Error> {
        let mut store = ChainStore::new(store);

        // Get runtime initial state and create genesis block out of it.
        let (state_store_update, state_roots) = runtime_adapter.genesis_state();
        let genesis =
            Block::genesis(state_roots.clone(), genesis_time, runtime_adapter.num_shards());

        // Check if we have a head in the store, otherwise pick genesis block.
        let mut store_update = store.store_update();
        let head_res = store_update.head();
        let head: Tip;
        match head_res {
            Ok(h) => {
                head = h;

                // Check that genesis in the store is the same as genesis given in the config.
                let genesis_hash = store_update.get_block_hash_by_height(0)?;
                if genesis_hash != genesis.hash() {
                    return Err(ErrorKind::Other(format!(
                        "Genesis mismatch between storage and config: {:?} vs {:?}",
                        genesis_hash,
                        genesis.hash()
                    ))
                    .into());
                }

                // Check we have the header corresponding to the header_head.
                let header_head = store_update.header_head()?;
                if store_update.get_block_header(&header_head.last_block_hash).is_err() {
                    // Reset header head and "sync" head to be consistent with current block head.
                    store_update.save_header_head(&head)?;
                    store_update.save_sync_head(&head);
                } else {
                    // Reset sync head to be consistent with current header head.
                    store_update.save_sync_head(&header_head);
                }
                // TODO: perform validation that latest state in runtime matches the stored chain.
            }
            Err(err) => match err.kind() {
                ErrorKind::DBNotFoundErr(_) => {
                    runtime_adapter
                        .add_validator_proposals(
                            CryptoHash::default(),
                            genesis.hash(),
                            0,
                            vec![],
                            vec![],
                        )
                        .map_err(|err| ErrorKind::Other(err.to_string()))?;
                    store_update.save_block_header(genesis.header.clone());
                    store_update.save_block(genesis.clone());

                    for (chunk_header, state_root) in genesis.chunks.iter().zip(state_roots.iter())
                    {
                        store_update.save_post_state_root(&chunk_header.chunk_hash(), state_root);
                    }

                    head = Tip::from_header(&genesis.header);
                    store_update.save_head(&head)?;
                    store_update.save_sync_head(&head);

                    store_update.merge(state_store_update);

                    info!(target: "chain", "Init: saved genesis: {:?} / {:?}", genesis.hash(), state_roots);
                }
                e => return Err(e.into()),
            },
        }
        store_update.commit()?;

        info!(target: "chain", "Init: head: {} @ {} [{}]", head.total_weight.to_num(), head.height, head.last_block_hash);

        Ok(Chain {
            store,
            runtime_adapter,
            orphans: OrphanBlockPool::new(),
            blocks_with_missing_chunks: OrphanBlockPool::new(),
            genesis: genesis.header,
        })
    }

    pub fn all_heights_with_missing_chunks(&self) -> Vec<u64> {
        self.blocks_with_missing_chunks.all_heights()
    }

    /// Reset "sync" head to current header head.
    /// Do this when first transition to header syncing.
    pub fn reset_sync_head(&mut self) -> Result<Tip, Error> {
        let mut chain_store_update = self.store.store_update();
        let header_head = chain_store_update.header_head()?;
        chain_store_update.save_sync_head(&header_head);
        chain_store_update.commit()?;
        Ok(header_head)
    }

    /// Process a block header received during "header first" propagation.
    pub fn process_block_header(&mut self, header: &BlockHeader) -> Result<(), Error> {
        // We create new chain update, but it's not going to be committed so it's read only.
        let mut chain_update = ChainUpdate::new(
            &mut self.store,
            self.runtime_adapter.clone(),
            &self.orphans,
            &self.blocks_with_missing_chunks,
        );
        chain_update.process_block_header(header)?;
        Ok(())
    }

    /// Process a received or produced block, and unroll any orphans that may depend on it.
    /// Changes current state, and calls `block_accepted` callback in case block was successfully applied.
    pub fn process_block<F, F2>(
        &mut self,
        me: &Option<AccountId>,
        block: Block,
        provenance: Provenance,
        block_accepted: F,
        block_misses_chunks: F2,
    ) -> Result<Option<Tip>, Error>
    where
        F: Copy + FnMut(&Block, BlockStatus, Provenance) -> (),
        F2: Copy + FnMut(Vec<ShardChunkHeader>) -> (),
    {
        let height = block.header.height;
        let res =
            self.process_block_single(me, block, provenance, block_accepted, block_misses_chunks);
        if res.is_ok() {
            if let Some(new_res) =
                self.check_orphans(me, height + 1, block_accepted, block_misses_chunks)
            {
                return Ok(Some(new_res));
            }
        }
        res
    }

    /// Processes headers and adds them to store for syncing.
    pub fn sync_block_headers(&mut self, mut headers: Vec<BlockHeader>) -> Result<(), Error> {
        // Sort headers by heights if they are out of order.
        headers.sort_by(|left, right| left.height.cmp(&right.height));

        let _first_header = if let Some(header) = headers.first() {
            debug!(target: "chain", "Sync block headers: {} headers from {} at {}", headers.len(), header.hash(), header.height);
            header
        } else {
            return Ok(());
        };

        let all_known = if let Some(last_header) = headers.last() {
            self.store.get_block_header(&last_header.hash()).is_ok()
        } else {
            false
        };

        if !all_known {
            // Validate header and then add to the chain.
            for header in headers.iter() {
                let mut chain_update = ChainUpdate::new(
                    &mut self.store,
                    self.runtime_adapter.clone(),
                    &self.orphans,
                    &self.blocks_with_missing_chunks,
                );

                chain_update.validate_header(header, &Provenance::SYNC)?;
                chain_update.chain_store_update.save_block_header(header.clone());
                chain_update.commit()?;

                // Add validator proposals for given header.
                self.runtime_adapter
                    .add_validator_proposals(
                        header.prev_hash,
                        header.hash(),
                        header.height,
                        header.validator_proposal.clone(),
                        vec![],
                    )
                    .map_err(|err| ErrorKind::Other(err.to_string()))?;
            }
        }

        let mut chain_update = ChainUpdate::new(
            &mut self.store,
            self.runtime_adapter.clone(),
            &self.orphans,
            &self.blocks_with_missing_chunks,
        );

        if let Some(header) = headers.last() {
            // Update sync_head regardless of the total weight.
            chain_update.update_sync_head(header)?;
            // Update header_head if total weight changed.
            chain_update.update_header_head(header)?;
        }

        chain_update.commit()
    }

    /// Check if state download is required, otherwise return hashes of blocks to fetch.
    pub fn check_state_needed(
        &mut self,
        block_fetch_horizon: BlockIndex,
    ) -> Result<(bool, Vec<CryptoHash>), Error> {
        let block_head = self.head()?;
        let header_head = self.header_head()?;
        let mut hashes = vec![];

        if block_head.total_weight >= header_head.total_weight {
            return Ok((false, hashes));
        }

        // Find common block between header chain and block chain.
        let mut oldest_height = 0;
        let mut current = self.get_block_header(&header_head.last_block_hash).map(|h| h.clone());
        while let Ok(header) = current {
            if header.height <= block_head.height {
                if self.is_on_current_chain(&header).is_ok() {
                    break;
                }
            }

            oldest_height = header.height;
            hashes.push(header.hash());
            current = self.get_previous_header(&header).map(|h| h.clone());
        }

        let sync_head = self.sync_head()?;
        if oldest_height < sync_head.height.saturating_sub(block_fetch_horizon) {
            return Ok((true, vec![]));
        }
        Ok((false, hashes))
    }

    /// Returns if given block header on the current chain.
    fn is_on_current_chain(&mut self, header: &BlockHeader) -> Result<(), Error> {
        let chain_header = self.get_header_by_height(header.height)?;
        if chain_header.hash() == header.hash() {
            Ok(())
        } else {
            Err(ErrorKind::Other(format!("{} not on current chain", header.hash())).into())
        }
    }

    /// Finds first of the given hashes that is known on the main chain.
    pub fn find_common_header(&mut self, hashes: &Vec<CryptoHash>) -> Option<BlockHeader> {
        for hash in hashes {
            if let Ok(header) = self.get_block_header(&hash).map(|h| h.clone()) {
                if let Ok(header_at_height) = self.get_header_by_height(header.height) {
                    if header.hash() == header_at_height.hash() {
                        return Some(header);
                    }
                }
            }
        }
        None
    }

    fn determine_status(&self, head: Option<Tip>, prev_head: Tip) -> BlockStatus {
        let has_head = head.is_some();
        let mut is_next_block = false;

        let old_hash = if let Some(head) = head {
            if head.prev_block_hash == prev_head.last_block_hash {
                is_next_block = true;
                None
            } else {
                Some(prev_head.last_block_hash)
            }
        } else {
            None
        };

        match (has_head, is_next_block) {
            (true, true) => BlockStatus::Next,
            (true, false) => BlockStatus::Reorg(old_hash.unwrap()),
            (false, _) => BlockStatus::Fork,
        }
    }

    fn process_block_single<F, F2>(
        &mut self,
        me: &Option<AccountId>,
        block: Block,
        provenance: Provenance,
        mut block_accepted: F,
        mut block_misses_chunks: F2,
    ) -> Result<Option<Tip>, Error>
    where
        F: FnMut(&Block, BlockStatus, Provenance) -> (),
        F2: Copy + FnMut(Vec<ShardChunkHeader>) -> (),
    {
        if block.chunks.len() != self.runtime_adapter.num_shards() as usize {
            return Err(ErrorKind::IncorrectNumberOfChunkHeaders.into());
        }

        let prev_head = self.store.head()?;
        let mut chain_update = ChainUpdate::new(
            &mut self.store,
            self.runtime_adapter.clone(),
            &self.orphans,
            &self.blocks_with_missing_chunks,
        );
        let maybe_new_head = chain_update.process_block(me, &block, &provenance);

        if let Ok(_) = maybe_new_head {
            chain_update.commit()?;
        }

        match maybe_new_head {
            Ok(head) => {
                let status = self.determine_status(head.clone(), prev_head);

                // Notify other parts of the system of the update.
                block_accepted(&block, status, provenance);

                Ok(head)
            }
            Err(e) => match e.kind() {
                ErrorKind::Orphan => {
                    let block_hash = block.hash();
                    let orphan = Orphan { block, provenance, added: Instant::now() };

                    self.orphans.add(orphan);

                    debug!(
                        target: "chain",
                        "Process block: orphan: {:?}, # orphans {}{}",
                        block_hash,
                        self.orphans.len(),
                        if self.orphans.len_evicted() > 0 {
                            format!(", # evicted {}", self.orphans.len_evicted())
                        } else {
                            String::new()
                        },
                    );
                    Err(ErrorKind::Orphan.into())
                }
                ErrorKind::ChunksMissing(missing_chunks) => {
                    let block_hash = block.hash();
                    block_misses_chunks(missing_chunks.clone());
                    let orphan = Orphan { block, provenance, added: Instant::now() };

                    self.blocks_with_missing_chunks.add(orphan);

                    debug!(
                        target: "chain",
                        "Process block: missing chunks. Block hash: {:?}. Missing chunks: {:?}",
                        block_hash, missing_chunks,
                    );
                    Err(ErrorKind::ChunksMissing(missing_chunks).into())
                }
                ErrorKind::Unfit(ref msg) => {
                    debug!(
                        target: "chain",
                        "Block {} at {} is unfit at this time: {}",
                        block.hash(),
                        block.header.height,
                        msg
                    );
                    Err(ErrorKind::Unfit(msg.clone()).into())
                }
                _ => Err(ErrorKind::Other(format!("{:?}", e)).into()),
            },
        }
    }

    /// Check if any block with missing chunk is ready to be processed
    pub fn check_blocks_with_missing_chunks<F, F2>(
        &mut self,
        me: &Option<AccountId>,
        height: BlockIndex,
        block_accepted: F,
        block_misses_chunks: F2,
    ) where
        F: Copy + FnMut(&Block, BlockStatus, Provenance) -> (),
        F2: Copy + FnMut(Vec<ShardChunkHeader>) -> (),
    {
        let mut new_blocks_accepted = false;
        if let Some(orphans) = self.blocks_with_missing_chunks.remove_by_height(height) {
            for orphan in orphans.into_iter() {
                let res = self.process_block_single(
                    me,
                    orphan.block,
                    orphan.provenance,
                    block_accepted,
                    block_misses_chunks,
                );
                match res {
                    Ok(_) => {
                        debug!(target: "chain", "Block with missing chunks is accepted; me: {:?}", me);
                        new_blocks_accepted = true;
                    }
                    Err(_) => {
                        debug!(target: "chain", "Block with missing chunks is declined; me: {:?}", me);
                    }
                }
            }
        };

        if new_blocks_accepted {
            self.check_orphans(me, height + 1, block_accepted, block_misses_chunks);
        }
    }

    /// Check for orphans, once a block is successfully added.
    pub fn check_orphans<F, F2>(
        &mut self,
        me: &Option<AccountId>,
        mut height: BlockIndex,
        block_accepted: F,
        block_misses_chunks: F2,
    ) -> Option<Tip>
    where
        F: Copy + FnMut(&Block, BlockStatus, Provenance) -> (),
        F2: Copy + FnMut(Vec<ShardChunkHeader>) -> (),
    {
        let initial_height = height;

        let mut orphan_accepted = false;
        let mut maybe_new_head = None;

        // Check if there are orphans we can process.
        debug!(target: "chain", "Check orphans: at {}, # orphans {}", height, self.orphans.len());
        loop {
            if let Some(orphans) = self.orphans.remove_by_height(height) {
                debug!(target: "chain", "Check orphans: found {} orphans", orphans.len());
                for orphan in orphans.into_iter() {
                    let res = self.process_block_single(
                        me,
                        orphan.block,
                        orphan.provenance,
                        block_accepted,
                        block_misses_chunks,
                    );
                    match res {
                        Ok(maybe_tip) => {
                            maybe_new_head = maybe_tip;
                            orphan_accepted = true;
                        }
                        Err(_) => {
                            debug!(target: "chain", "Orphan declined");
                        }
                    }
                }

                if orphan_accepted {
                    // Accepted a block, so should check if there are now new orphans unlocked.
                    height += 1;
                    continue;
                }
            }
            break;
        }

        if initial_height != height {
            debug!(
                target: "chain",
                "Check orphans: {} blocks accepted since height {}, remaining # orphans {}",
                height - initial_height,
                initial_height,
                self.orphans.len(),
            );
        }

        maybe_new_head
    }

    pub fn set_shard_state(
        &mut self,
        shard_id: ShardId,
        hash: CryptoHash,
        payload: Vec<u8>,
        _receipts: Vec<ReceiptTransaction>,
    ) -> Result<(), Error> {
        // TODO(1046): update this with any required changes for chunks support.
        let header = self.get_block_header(&hash)?;
        let (_prev_hash, state_root) = (header.prev_hash, header.prev_state_root);

        // Save state in the runtime, will also check it's validity.
        self.runtime_adapter
            .set_state(shard_id, state_root, payload)
            .map_err(|err| ErrorKind::InvalidStatePayload(err.to_string()))?;

        // Update pointers to state root and receipts.
        let /*mut*/ chain_store_update = self.store.store_update();
        // TODO XXX
        //chain_store_update.save_post_state_root(&prev_hash, &state_root);
        //chain_store_update.save_receipt(&prev_hash, receipts);
        chain_store_update.commit()?;

        Ok(())
    }
}

/// Various chain getters.
impl Chain {
    /// Gets chain head.
    #[inline]
    pub fn head(&self) -> Result<Tip, Error> {
        self.store.head()
    }

    /// Gets chain header head.
    #[inline]
    pub fn header_head(&self) -> Result<Tip, Error> {
        self.store.header_head()
    }

    /// Gets "sync" head. This may be significantly different to current header chain.
    #[inline]
    pub fn sync_head(&self) -> Result<Tip, Error> {
        self.store.sync_head()
    }

    /// Header of the block at the head of the block chain (not the same thing as header_head).
    #[inline]
    pub fn head_header(&mut self) -> Result<&BlockHeader, Error> {
        self.store.head_header()
    }

    /// Gets a block by hash.
    #[inline]
    pub fn get_block(&mut self, hash: &CryptoHash) -> Result<&Block, Error> {
        self.store.get_block(hash)
    }

    /// Gets a chunk by hash.
    #[inline]
    pub fn get_chunk(&mut self, header: &ShardChunkHeader) -> Result<&ShardChunk, Error> {
        self.store.get_chunk(header)
    }

    /// Gets a block from the current chain by height.
    #[inline]
    pub fn get_block_by_height(&mut self, height: BlockIndex) -> Result<&Block, Error> {
        let hash = self.store.get_block_hash_by_height(height)?.clone();
        self.store.get_block(&hash)
    }

    /// Gets a block header by hash.
    #[inline]
    pub fn get_block_header(&mut self, hash: &CryptoHash) -> Result<&BlockHeader, Error> {
        self.store.get_block_header(hash)
    }

    /// Gets a vector of post-receipts by block hash and shard_id
    #[inline]
    pub fn get_receipts(
        &mut self,
        hash: &CryptoHash,
        shard_id: ShardId,
    ) -> Result<&Vec<ReceiptTransaction>, Error> {
        self.store.get_outgoing_receipts(hash, shard_id)
    }

    /// Returns block header from the current chain for given height if present.
    #[inline]
    pub fn get_header_by_height(&mut self, height: BlockIndex) -> Result<&BlockHeader, Error> {
        let hash = self.store.get_block_hash_by_height(height)?.clone();
        self.store.get_block_header(&hash)
    }

    /// Get previous block header.
    #[inline]
    pub fn get_previous_header(&mut self, header: &BlockHeader) -> Result<&BlockHeader, Error> {
        self.store.get_previous_header(header)
    }

    /// Check if block exists.
    #[inline]
    pub fn block_exists(&self, hash: &CryptoHash) -> Result<bool, Error> {
        self.store.block_exists(hash)
    }

    /// Get state root hash after applying header with given hash.
    #[inline]
    pub fn get_post_state_root(&mut self, hash: &ChunkHash) -> Result<&MerkleHash, Error> {
        self.store.get_post_state_root(hash)
    }

    /// Get transaction result for given hash of transaction.
    #[inline]
    pub fn get_transaction_result(
        &mut self,
        hash: &CryptoHash,
    ) -> Result<&TransactionResult, Error> {
        self.store.get_transaction_result(hash)
    }

    /// Returns underlying ChainStore.
    #[inline]
    pub fn store(&self) -> &ChainStore {
        &self.store
    }

    /// Returns mutable ChainStore.
    #[inline]
    pub fn mut_store(&mut self) -> &mut ChainStore {
        &mut self.store
    }

    /// Returns genesis block header.
    #[inline]
    pub fn genesis(&self) -> &BlockHeader {
        &self.genesis
    }

    /// Returns number of orphans currently in the orphan pool.
    #[inline]
    pub fn orphans_len(&self) -> usize {
        self.orphans.len()
    }

    /// Returns number of evicted orphans.
    #[inline]
    pub fn orphans_evicted_len(&self) -> usize {
        self.orphans.len_evicted()
    }

    /// Check if hash is for a known orphan.
    #[inline]
    pub fn is_orphan(&self, hash: &CryptoHash) -> bool {
        self.orphans.contains(hash)
    }
}

/// Chain update helper, contains information that is needed to process block
/// and decide to accept it or reject it.
/// If rejected nothing will be updated in underlying storage.
/// Safe to stop process mid way (Ctrl+C or crash).
struct ChainUpdate<'a> {
    runtime_adapter: Arc<dyn RuntimeAdapter>,
    chain_store_update: ChainStoreUpdate<'a, ChainStore>,
    orphans: &'a OrphanBlockPool,
    blocks_with_missing_chunks: &'a OrphanBlockPool,
}

impl<'a> ChainUpdate<'a> {
    pub fn new(
        store: &'a mut ChainStore,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        orphans: &'a OrphanBlockPool,
        blocks_with_missing_chunks: &'a OrphanBlockPool,
    ) -> Self {
        let chain_store_update = store.store_update();
        ChainUpdate { runtime_adapter, chain_store_update, orphans, blocks_with_missing_chunks }
    }

    /// Commit changes to the chain into the database.
    pub fn commit(self) -> Result<(), Error> {
        self.chain_store_update.commit()
    }

    /// Process block header as part of "header first" block propagation.
    /// We validate the header but we do not store it or update header head
    /// based on this. We will update these once we get the block back after
    /// requesting it.
    pub fn process_block_header(&mut self, header: &BlockHeader) -> Result<(), Error> {
        debug!(target: "chain", "Process block header: {} at {}", header.hash(), header.height);

        self.check_header_known(header)?;
        self.validate_header(header, &Provenance::NONE)?;
        Ok(())
    }

    /// Find previous header or return Orphan error if not found.
    pub fn get_previous_header(&mut self, header: &BlockHeader) -> Result<&BlockHeader, Error> {
        self.chain_store_update.get_previous_header(header).map_err(|e| match e.kind() {
            ErrorKind::DBNotFoundErr(_) => ErrorKind::Orphan.into(),
            other => other.into(),
        })
    }

    pub fn save_incoming_receipts_from_block(
        &mut self,
        me: &Option<AccountId>,
        parent_hash: CryptoHash,
        block: &Block,
    ) -> Result<(), Error> {
        let all_chunks_or_oneparts: Vec<ShardFullChunkOrOnePart> =
            self.chain_store_update.get_chunks_or_one_parts(
                me,
                parent_hash,
                block.header.height,
                self.runtime_adapter.clone(),
                block.chunks.as_ref(),
            )?;

        let mut receipts_by_shard_id = HashMap::new();

        let all_receipts = all_chunks_or_oneparts
            .iter()
            .map(|chunk_or_onepart| match chunk_or_onepart {
                ShardFullChunkOrOnePart::FullChunk(chunk) => chunk.receipts.clone(),
                ShardFullChunkOrOnePart::OnePart(onepart) => onepart.receipts.clone(),
                ShardFullChunkOrOnePart::NoChunk => vec![],
            })
            .flatten();

        for receipt in all_receipts {
            let shard_id = self.runtime_adapter.account_id_to_shard_id(&receipt.receiver);
            receipts_by_shard_id.entry(shard_id).or_insert_with(Vec::new).push(receipt);
        }

        for (shard_id, receipts) in receipts_by_shard_id {
            self.chain_store_update.save_incoming_receipt(&block.hash(), shard_id, receipts);
        }

        Ok(())
    }

    /// Runs the block processing, including validation and finding a place for the new block in the chain.
    /// Returns new head if chain head updated.
    fn process_block(
        &mut self,
        me: &Option<AccountId>,
        block: &Block,
        provenance: &Provenance,
    ) -> Result<Option<Tip>, Error> {
        debug!(target: "chain", "Process block {} at {}, approvals: {}, tx: {}, me: {:?}", block.hash(), block.header.height, block.header.approval_sigs.len(), block.transactions.len(), me);

        // Check if we have already processed this block previously.
        self.check_known(&block)?;

        // Delay hitting the db for current chain head until we know this block is not already known.
        let head = self.chain_store_update.head()?;
        let is_next = block.header.prev_hash == head.last_block_hash;

        // First real I/O expense.
        let prev = self.get_previous_header(&block.header)?;
        let prev_hash = prev.hash();

        // Block is an orphan if we do not know about the previous full block.
        if !is_next && !self.chain_store_update.block_exists(&prev_hash)? {
            return Err(ErrorKind::Orphan.into());
        }

        // This is a fork in the context of both header and block processing
        // if this block does not immediately follow the chain head.
        // let is_fork = !is_next;

        // Check the header is valid before we proceed with the full block.
        self.process_header_for_block(&block.header, provenance)?;

        // Check that state root stored in the header matches the state root of the chunks
        let state_root = Block::compute_state_root(&block.chunks);
        if block.header.prev_state_root != state_root {
            return Err(ErrorKind::InvalidStateRoot.into());
        }

        // Add validated block to the db, even if it's not the selected fork.
        self.chain_store_update.save_block(block.clone());

        let prev_block = self.chain_store_update.get_block(&prev_hash)?.clone();

        self.save_incoming_receipts_from_block(me, prev_hash, &block)?;

        for (shard_id, (chunk_header, prev_chunk_header)) in
            (block.chunks.iter().zip(prev_block.chunks.iter())).enumerate()
        {
            let shard_id = shard_id as ShardId;
            if chunk_header.height_included == block.header.height {
                if chunk_header.prev_block_hash != block.header.prev_hash {
                    return Err(ErrorKind::InvalidChunk.into());
                }
                let chunk_hash = chunk_header.chunk_hash();
                if me.as_ref().map_or_else(
                    || false,
                    |me| {
                        self.runtime_adapter.cares_about_shard(me, block.header.prev_hash, shard_id)
                    },
                ) {
                    let receipts = self.chain_store_update.get_incoming_receipts_for_shard(
                        shard_id,
                        block.hash(),
                        prev_chunk_header,
                    )?;
                    let chunk = self.chain_store_update.get_chunk(&chunk_header)?;
                    let receipt_hashes = receipts.iter().map(|r| r.get_hash()).collect::<Vec<_>>();
                    let transaction_hashes =
                        chunk.transactions.iter().map(|t| t.get_hash()).collect::<Vec<_>>();

                    // Apply block to runtime.
                    let (
                        trie_changes,
                        state_root,
                        mut tx_results,
                        mut new_receipts,
                        validator_proposals,
                    ) = self
                        .runtime_adapter
                        .apply_transactions(
                            shard_id,
                            &chunk.header.prev_state_root,
                            chunk_header.height_included,
                            &chunk_header.prev_block_hash,
                            &receipts,
                            &chunk.transactions,
                        )
                        .map_err(|e| ErrorKind::Other(e.to_string()))?;

                    // If block checks out, record validator proposals for given block.
                    self.runtime_adapter
                        .add_validator_proposals(
                            block.header.prev_hash,
                            block.hash(),
                            block.header.height,
                            validator_proposals,
                            vec![],
                        )
                        .map_err(|err| ErrorKind::Other(err.to_string()))?;

                    self.chain_store_update.save_trie_changes(trie_changes);
                    // Save state root after applying transactions.
                    self.chain_store_update.save_post_state_root(&chunk_hash, &state_root);
                    // Save resulting receipts.
                    for (_receipt_shard_id, receipts) in new_receipts.drain() {
                        // The receipts in store are indexed by the SOURCE shard_id, not destination,
                        //    since they are later retrieved by the chunk producer of the source
                        //    shard to be distributed to the recipients.
                        self.chain_store_update.save_outgoing_receipt(
                            &block.hash(),
                            shard_id,
                            receipts,
                        );
                    }
                    // Save receipt and transaction results.
                    for (i, tx_result) in tx_results.drain(..).enumerate() {
                        if i < receipt_hashes.len() {
                            self.chain_store_update
                                .save_transaction_result(&receipt_hashes[i], tx_result);
                        } else {
                            self.chain_store_update.save_transaction_result(
                                &transaction_hashes[i - receipt_hashes.len()],
                                tx_result,
                            );
                        }
                    }
                }
            } else {
                if prev_chunk_header != chunk_header {
                    info!(
                        "MOO {:?} != {:?}, DEF: {}, GEN: {}",
                        prev_chunk_header,
                        chunk_header,
                        CryptoHash::default(),
                        Block::chunk_genesis_hash()
                    );
                    return Err(ErrorKind::InvalidChunk.into());
                }
            }
        }

        // Update the chain head if total weight has increased.
        let res = self.update_head(block)?;
        Ok(res)
    }

    /// Process a block header as part of processing a full block.
    /// We want to be sure the header is valid before processing the full block.
    fn process_header_for_block(
        &mut self,
        header: &BlockHeader,
        provenance: &Provenance,
    ) -> Result<(), Error> {
        self.validate_header(header, provenance)?;
        self.chain_store_update.save_block_header(header.clone());
        self.update_header_head(header)?;
        Ok(())
    }

    fn validate_header(
        &mut self,
        header: &BlockHeader,
        provenance: &Provenance,
    ) -> Result<(), Error> {
        // Refuse blocks from the too distant future.
        if header.timestamp > Utc::now() + Duration::seconds(ACCEPTABLE_TIME_DIFFERENCE) {
            return Err(ErrorKind::InvalidBlockFutureTime(header.timestamp).into());
        }

        // First I/O cost, delayed as late as possible.
        let prev_header = self.get_previous_header(header)?;

        // Prevent time warp attacks and some timestamp manipulations by forcing strict
        // time progression.
        if header.timestamp <= prev_header.timestamp {
            return Err(
                ErrorKind::InvalidBlockPastTime(prev_header.timestamp, header.timestamp).into()
            );
        }

        // If this is not the block we produced (hence trust in it) - validates block
        // producer, confirmation signatures and returns new total weight.
        if *provenance != Provenance::PRODUCED {
            let prev_header = self.get_previous_header(header)?.clone();
            let weight = self.runtime_adapter.compute_block_weight(&prev_header, header)?;
            if weight != header.total_weight {
                return Err(ErrorKind::InvalidBlockWeight.into());
            }
        }

        Ok(())
    }

    /// Update the header head if this header has most work.
    fn update_header_head(&mut self, header: &BlockHeader) -> Result<Option<Tip>, Error> {
        let header_head = self.chain_store_update.header_head()?;
        if header.total_weight > header_head.total_weight {
            let tip = Tip::from_header(header);
            self.chain_store_update.save_header_head(&tip)?;
            debug!(target: "chain", "Header head updated to {} at {}", tip.last_block_hash, tip.height);

            Ok(Some(tip))
        } else {
            Ok(None)
        }
    }

    /// Directly updates the head if we've just appended a new block to it or handle
    /// the situation where we've just added enough weight to have a fork with more
    /// work than the head.
    fn update_head(&mut self, block: &Block) -> Result<Option<Tip>, Error> {
        // if we made a fork with more work than the head (which should also be true
        // when extending the head), update it
        let head = self.chain_store_update.head()?;
        if block.header.total_weight > head.total_weight {
            let tip = Tip::from_header(&block.header);

            self.chain_store_update.save_body_head(&tip);
            debug!(target: "chain", "Head updated to {} at {}", tip.last_block_hash, tip.height);
            Ok(Some(tip))
        } else {
            Ok(None)
        }
    }

    /// Updates "sync" head with given block header.
    fn update_sync_head(&mut self, header: &BlockHeader) -> Result<(), Error> {
        let tip = Tip::from_header(header);
        self.chain_store_update.save_sync_head(&tip);
        debug!(target: "chain", "Sync head {} @ {}", tip.last_block_hash, tip.height);
        Ok(())
    }

    /// Quick in-memory check to fast-reject any block header we've already handled
    /// recently. Keeps duplicates from the network in check.
    /// ctx here is specific to the header_head (tip of the header chain)
    fn check_header_known(&mut self, header: &BlockHeader) -> Result<(), Error> {
        let header_head = self.chain_store_update.header_head()?;
        if header.hash() == header_head.last_block_hash
            || header.hash() == header_head.prev_block_hash
        {
            return Err(ErrorKind::Unfit("header already known".to_string()).into());
        }
        Ok(())
    }

    /// Quick in-memory check for fast-reject any block handled recently.
    fn check_known_head(&self, header: &BlockHeader) -> Result<(), Error> {
        let head = self.chain_store_update.head()?;
        let bh = header.hash();
        if bh == head.last_block_hash || bh == head.prev_block_hash {
            return Err(ErrorKind::Unfit("already known in head".to_string()).into());
        }
        Ok(())
    }

    /// Check if this block is in the set of known orphans.
    fn check_known_orphans(&self, header: &BlockHeader) -> Result<(), Error> {
        if self.orphans.contains(&header.hash()) {
            return Err(ErrorKind::Unfit("already known in orphans".to_string()).into());
        }
        if self.blocks_with_missing_chunks.contains(&header.hash()) {
            return Err(ErrorKind::Unfit(
                "already known in blocks with missing chunks".to_string(),
            )
            .into());
        }
        Ok(())
    }

    /// Check if this block is ini the store already.
    fn check_known_store(&self, header: &BlockHeader) -> Result<(), Error> {
        match self.chain_store_update.block_exists(&header.hash()) {
            Ok(true) => {
                let head = self.chain_store_update.head()?;
                if header.height > 50 && header.height < head.height - 50 {
                    // We flag this as an "abusive peer" but only in the case
                    // where we have the full block in our store.
                    // So this is not a particularly exhaustive check.
                    Err(ErrorKind::OldBlock.into())
                } else {
                    Err(ErrorKind::Unfit("already known in store".to_string()).into())
                }
            }
            Ok(false) => {
                // Not yet processed this block, we can proceed.
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Check if header is known: head, orphan or in store.
    #[allow(dead_code)]
    fn is_header_known(&self, header: &BlockHeader) -> Result<bool, Error> {
        let check = || {
            self.check_known_head(header)?;
            self.check_known_orphans(header)?;
            self.check_known_store(header)
        };
        match check() {
            Ok(()) => Ok(false),
            Err(err) => match err.kind() {
                ErrorKind::Unfit(_) => Ok(true),
                kind => Err(kind.into()),
            },
        }
    }

    /// Check if block is known: head, orphan or in store.
    fn check_known(&self, block: &Block) -> Result<(), Error> {
        self.check_known_head(&block.header)?;
        self.check_known_orphans(&block.header)?;
        self.check_known_store(&block.header)?;
        Ok(())
    }
}
