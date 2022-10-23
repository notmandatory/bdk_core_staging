use core::{fmt::Display, ops::RangeBounds};

use crate::{alloc::string::String, collections::*, BlockId, TxGraph, Vec};
use bitcoin::{hashes::Hash, BlockHash, OutPoint, TxOut, Txid};

#[derive(Clone, Debug, Default)]
pub struct SparseChain {
    /// Block height to checkpoint data.
    checkpoints: BTreeMap<u32, BlockHash>,
    /// Txids prepended by confirmation height.
    txid_by_height: BTreeSet<(u32, Txid)>,
    /// Confirmation heights of txids.
    txid_to_index: HashMap<Txid, u32>,
    /// A list of mempool txids.
    mempool: HashSet<Txid>,
    /// Limit number of checkpoints.
    checkpoint_limit: Option<usize>,
}

/// Represents an update failure of [`SparseChain`].
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateFailure {
    /// The [`Update`] is total bogus. Cannot be applied to any [`SparseChain`].
    Bogus(BogusReason),

    /// The [`Update`] cannot be applied to this [`SparseChain`] because the `last_valid` value does
    /// not match with the current state of the chain.
    Stale {
        got_last_valid: Option<BlockId>,
        expected_last_valid: Option<BlockId>,
    },

    /// The [`Update`] canot be applied, because there are inconsistent tx states.
    /// This only reports the first inconsistency.
    Inconsistent {
        inconsistent_txid: Txid,
        original_height: TxHeight,
        update_height: TxHeight,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum BogusReason {
    /// `last_valid` conflicts with `new_tip`.
    LastValidConflictsNewTip {
        new_tip: BlockId,
        last_valid: BlockId,
    },

    /// At least one `txid` has a confirmation height greater than `new_tip`.
    TxHeightGreaterThanTip {
        new_tip: BlockId,
        tx: (Txid, TxHeight),
    },
}

impl core::fmt::Display for UpdateFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        fn print_block(id: &BlockId) -> String {
            format!("{} @ {}", id.hash, id.height)
        }

        fn print_block_opt(id: &Option<BlockId>) -> String {
            match id {
                Some(id) => print_block(id),
                None => "None".into(),
            }
        }

        match self {
            Self::Bogus(reason) => {
                write!(f, "bogus update: ")?;
                match reason {
                    BogusReason::LastValidConflictsNewTip { new_tip, last_valid } =>
                        write!(f, "last_valid ({}) conflicts new_tip ({})", 
                            print_block(last_valid), print_block(new_tip)),

                    BogusReason::TxHeightGreaterThanTip { new_tip, tx: txid } =>
                        write!(f, "tx ({}) confirmation height ({}) is greater than new_tip ({})", 
                            txid.0, txid.1, print_block(new_tip)),
                }
            },
            Self::Stale { got_last_valid, expected_last_valid } =>
                write!(f, "stale update: got last_valid ({}) when expecting ({})", 
                    print_block_opt(got_last_valid), print_block_opt(expected_last_valid)),

            Self::Inconsistent { inconsistent_txid, original_height, update_height } =>
                write!(f, "inconsistent update: first inconsistent tx is ({}) which had confirmation height ({}), but is ({}) in the update", 
                    inconsistent_txid, original_height, update_height),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for UpdateFailure {}

impl SparseChain {
    /// Get the transaction ids in a particular checkpoint.
    ///
    /// The `Txid`s are ordered first by their confirmation height (ascending) and then lexically by their `Txid`.
    ///
    /// ## Panics
    ///
    /// This will panic if a checkpoint doesn't exist with `checkpoint_id`
    pub fn checkpoint_txids(
        &self,
        block_id: BlockId,
    ) -> impl DoubleEndedIterator<Item = &(u32, Txid)> + '_ {
        let block_hash = self
            .checkpoints
            .get(&block_id.height)
            .expect("the tracker did not have a checkpoint at that height");
        assert_eq!(
            block_hash, &block_id.hash,
            "tracker had a different block hash for checkpoint at that height"
        );

        let h = block_id.height;

        self.txid_by_height.range((h, Txid::all_zeros())..)
    }

    /// Get the BlockId for the last known tip.
    pub fn latest_checkpoint(&self) -> Option<BlockId> {
        self.checkpoints
            .iter()
            .last()
            .map(|(&height, &hash)| BlockId { height, hash })
    }

    /// Get the checkpoint id at the given height if it exists
    pub fn checkpoint_at(&self, height: u32) -> Option<BlockId> {
        self.checkpoints
            .get(&height)
            .map(|&hash| BlockId { height, hash })
    }

    /// Return height of tx (if any).
    pub fn transaction_height(&self, txid: &Txid) -> Option<TxHeight> {
        Some(if self.mempool.contains(txid) {
            TxHeight::Unconfirmed
        } else {
            TxHeight::Confirmed(*self.txid_to_index.get(txid)?)
        })
    }

    /// Return an iterator over the checkpoint locations in a height range.
    pub fn iter_checkpoints(
        &self,
        range: impl RangeBounds<u32>,
    ) -> impl DoubleEndedIterator<Item = BlockId> + '_ {
        self.checkpoints
            .range(range)
            .map(|(&height, &hash)| BlockId { height, hash })
    }

    /// Apply transactions that are all confirmed in a given block
    pub fn apply_block_txs(
        &mut self,
        block_id: BlockId,
        transactions: impl IntoIterator<Item = Txid>,
    ) -> Result<(), UpdateFailure> {
        let mut checkpoint = Update {
            txids: transactions
                .into_iter()
                .map(|txid| (txid, TxHeight::Confirmed(block_id.height)))
                .collect(),
            last_valid: self.latest_checkpoint(),
            invalidate: None,
            new_tip: block_id,
        };

        let matching_checkpoint = self.checkpoint_at(block_id.height);
        if matches!(matching_checkpoint, Some(id) if id != block_id) {
            checkpoint.invalidate = matching_checkpoint;
        }

        self.apply_update(checkpoint)
    }

    /// Applies a new [`Update`] to the tracker.
    #[must_use]
    pub fn apply_update(&mut self, update: Update) -> Result<(), UpdateFailure> {
        // if there is no `invalidate`, `last_valid` should be the last checkpoint in sparsechain
        // if there is `invalidate`, `last_valid` should be the checkpoint preceding `invalidate`
        let expected_last_valid = {
            let upper_bound = update.invalidate.map(|b| b.height).unwrap_or(u32::MAX);
            self.checkpoints
                .range(..upper_bound)
                .last()
                .map(|(&height, &hash)| BlockId { height, hash })
        };
        if update.last_valid != expected_last_valid {
            return Result::Err(UpdateFailure::Stale {
                got_last_valid: update.last_valid,
                expected_last_valid: expected_last_valid,
            });
        }

        // `new_tip.height` should be greater or equal to `last_valid.height`
        // if `new_tip.height` is equal to `last_valid.height`, the hashes should also be the same
        if let Some(last_valid) = expected_last_valid {
            if update.new_tip.height < last_valid.height
                || update.new_tip.height == last_valid.height
                    && update.new_tip.hash != last_valid.hash
            {
                return Result::Err(UpdateFailure::Bogus(
                    BogusReason::LastValidConflictsNewTip {
                        new_tip: update.new_tip,
                        last_valid,
                    },
                ));
            }
        }

        for (txid, tx_height) in &update.txids {
            // ensure new_height does not surpass latest checkpoint
            if matches!(tx_height, TxHeight::Confirmed(tx_h) if tx_h > &update.new_tip.height) {
                return Result::Err(UpdateFailure::Bogus(BogusReason::TxHeightGreaterThanTip {
                    new_tip: update.new_tip,
                    tx: (*txid, tx_height.clone()),
                }));
            }

            // ensure all currently confirmed txs are still at the same height (unless, if they are
            // to be invalidated)
            if let Some(&height) = self.txid_to_index.get(txid) {
                // no need to check consistency if height will be invalidated
                if matches!(update.invalidate, Some(invalid) if height >= invalid.height)
                    // tx is consistent if height stays the same
                    || matches!(tx_height, TxHeight::Confirmed(new_height) if *new_height == height)
                {
                    continue;
                }

                // inconsistent
                return Result::Err(UpdateFailure::Inconsistent {
                    inconsistent_txid: *txid,
                    original_height: TxHeight::Confirmed(height),
                    update_height: *tx_height,
                });
            }
        }

        if let Some(invalid) = &update.invalidate {
            self.invalidate_checkpoints(invalid.height);
        }

        // record latest checkpoint (if any)
        self.checkpoints
            .entry(update.new_tip.height)
            .or_insert(update.new_tip.hash);

        for (txid, conf) in update.txids {
            match conf {
                TxHeight::Confirmed(height) => {
                    if self.txid_by_height.insert((height, txid)) {
                        self.txid_to_index.insert(txid, height);
                        self.mempool.remove(&txid);
                    }
                }
                TxHeight::Unconfirmed => {
                    self.mempool.insert(txid);
                }
            }
        }

        self.prune_checkpoints();
        Result::Ok(())
    }

    /// Clear the mempool list. Use with caution.
    pub fn clear_mempool(&mut self) {
        self.mempool.clear()
    }

    /// Reverse everything of the Block with given hash and height.
    pub fn disconnect_block(&mut self, block_id: BlockId) {
        if let Some(checkpoint_hash) = self.checkpoints.get(&block_id.height) {
            if checkpoint_hash == &block_id.hash {
                // Can't guarantee that mempool is consistent with chain after we disconnect a block so we
                // clear it.
                self.invalidate_checkpoints(block_id.height);
                self.clear_mempool();
            }
        }
    }

    // Invalidate all checkpoints from the given height
    fn invalidate_checkpoints(&mut self, height: u32) {
        let _removed_checkpoints = self.checkpoints.split_off(&height);
        let removed_txids = self.txid_by_height.split_off(&(height, Txid::all_zeros()));

        for (exp_h, txid) in &removed_txids {
            let h = self.txid_to_index.remove(txid);
            debug_assert!(matches!(h, Some(h) if h == *exp_h));
        }

        if !removed_txids.is_empty() {
            self.mempool.clear()
        }
    }

    /// Iterates over confirmed txids, in increasing confirmations.
    pub fn iter_confirmed_txids(&self) -> impl Iterator<Item = &(u32, Txid)> + DoubleEndedIterator {
        self.txid_by_height.iter().rev()
    }

    /// Iterates over unconfirmed txids.
    pub fn iter_mempool_txids(&self) -> impl Iterator<Item = &Txid> {
        self.mempool.iter()
    }

    pub fn iter_txids(&self) -> impl Iterator<Item = (Option<u32>, Txid)> + '_ {
        let mempool_iter = self.iter_mempool_txids().map(|&txid| (None, txid));
        let confirmed_iter = self
            .iter_confirmed_txids()
            .map(|&(h, txid)| (Some(h), txid));
        mempool_iter.chain(confirmed_iter)
    }

    pub fn full_txout(&self, graph: &TxGraph, outpoint: OutPoint) -> Option<FullTxOut> {
        let height = self.transaction_height(&outpoint.txid)?;

        let txout = graph.txout(&outpoint).cloned()?;

        let spent_by = graph
            .outspend(&outpoint)
            .map(|txid_map| {
                // find txids
                let txids = txid_map
                    .iter()
                    .filter(|&txid| self.txid_to_index.contains_key(txid))
                    .collect::<Vec<_>>();
                debug_assert!(txids.len() <= 1, "conflicting txs in sparse chain");
                txids.get(0).cloned()
            })
            .flatten()
            .cloned();

        Some(FullTxOut {
            outpoint,
            txout,
            height,
            spent_by,
        })
    }

    pub fn set_checkpoint_limit(&mut self, limit: Option<usize>) {
        self.checkpoint_limit = limit;
    }

    fn prune_checkpoints(&mut self) -> Option<BTreeMap<u32, BlockHash>> {
        let limit = self.checkpoint_limit?;

        // find last height to be pruned
        let last_height = *self.checkpoints.keys().rev().nth(limit)?;
        // first height to be kept
        let keep_height = last_height + 1;

        let mut split = self.checkpoints.split_off(&keep_height);
        core::mem::swap(&mut self.checkpoints, &mut split);

        Some(split)
    }
}

/// Represents an [`Update`] that could be applied to [`SparseChain`].
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// List of transactions in this checkpoint. They needs to be consistent with [`SparseChain`]'s
    /// state for the [`Update`] to be included.
    pub txids: HashMap<Txid, TxHeight>,

    /// This should be the latest valid checkpoint of [`SparseChain`]; used to avoid conflicts.
    /// If `invalidate == None`, then this would be be the latest checkpoint of [`SparseChain`].
    /// If `invalidate == Some`, then this would be the checkpoint directly preceding `invalidate`.
    /// If [`SparseChain`] is empty, `last_valid` should be `None`.
    pub last_valid: Option<BlockId>,

    /// Invalidates all checkpoints from this checkpoint (inclusive).
    pub invalidate: Option<BlockId>,

    /// The latest tip that this [`Update`] is aware of. Introduced transactions cannot surpass this
    /// tip.
    pub new_tip: BlockId,
}

impl Update {
    /// Helper function to create a template update.
    pub fn new(last_valid: Option<BlockId>, new_tip: BlockId) -> Self {
        Self {
            txids: HashMap::new(),
            last_valid,
            invalidate: None,
            new_tip,
        }
    }
}

/// Represents the height in which a transaction is confirmed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TxHeight {
    Confirmed(u32),
    Unconfirmed,
}

impl Display for TxHeight {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Confirmed(h) => core::write!(f, "confirmed_at({})", h),
            Self::Unconfirmed => core::write!(f, "unconfirmed"),
        }
    }
}

impl From<Option<u32>> for TxHeight {
    fn from(opt: Option<u32>) -> Self {
        match opt {
            Some(h) => Self::Confirmed(h),
            None => Self::Unconfirmed,
        }
    }
}

impl TxHeight {
    pub fn is_confirmed(&self) -> bool {
        matches!(self, Self::Confirmed(_))
    }
}

/// A `TxOut` with as much data as we can retreive about it
#[derive(Debug, Clone, PartialEq)]
pub struct FullTxOut {
    pub outpoint: OutPoint,
    pub txout: TxOut,
    pub height: TxHeight,
    pub spent_by: Option<Txid>,
}
