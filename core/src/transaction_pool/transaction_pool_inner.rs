use super::{
    account_cache::AccountCache,
    garbage_collector::GarbageCollector,
    impls::TreapMap,
    nonce_pool::{InsertResult, NoncePool, TxWithReadyInfo},
};
use crate::{
    machine::Machine,
    verification::{PackingCheckResult, VerificationConfig},
};
use cfx_parameters::staking::DRIPS_PER_STORAGE_COLLATERAL_UNIT;
use cfx_statedb::Result as StateDbResult;
use cfx_types::{address_util::AddressUtil, Address, H256, U128, U256, U512};
use malloc_size_of_derive::MallocSizeOf as DeriveMallocSizeOf;
use metrics::{
    register_meter_with_group, Counter, CounterUsize, Meter, MeterTimer,
};
use primitives::{
    Account, Action, SignedTransaction, TransactionWithSignature,
};
use rlp::*;
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

type WeightType = u128;
lazy_static! {
    pub static ref MAX_WEIGHT: U256 = u128::max_value().into();
}

const FURTHEST_FUTURE_TRANSACTION_NONCE_OFFSET: u32 = 2000;
/// The max number of senders we compare gas price with a new inserted
/// transaction.
const GC_CHECK_COUNT: usize = 5;

lazy_static! {
    static ref TX_POOL_RECALCULATE: Arc<dyn Meter> =
        register_meter_with_group("timer", "tx_pool::recalculate");
    static ref TX_POOL_INNER_INSERT_TIMER: Arc<dyn Meter> =
        register_meter_with_group("timer", "tx_pool::inner_insert");
    static ref DEFERRED_POOL_INNER_INSERT: Arc<dyn Meter> =
        register_meter_with_group("timer", "deferred_pool::inner_insert");
    pub static ref TX_POOL_GET_STATE_TIMER: Arc<dyn Meter> =
        register_meter_with_group("timer", "tx_pool::get_nonce_and_storage");
    static ref TX_POOL_INNER_WITHOUTCHECK_INSERT_TIMER: Arc<dyn Meter> =
        register_meter_with_group(
            "timer",
            "tx_pool::inner_without_check_inert"
        );
    static ref GC_UNEXECUTED_COUNTER: Arc<dyn Counter<usize>> =
        CounterUsize::register_with_group("txpool", "gc_unexecuted");
    static ref GC_READY_COUNTER: Arc<dyn Counter<usize>> =
        CounterUsize::register_with_group("txpool", "gc_ready");
    static ref GC_METER: Arc<dyn Meter> =
        register_meter_with_group("txpool", "gc_txs_tps");
}

#[derive(DeriveMallocSizeOf)]
struct DeferredPool {
    buckets: HashMap<Address, NoncePool>,
}

impl DeferredPool {
    fn new() -> Self {
        DeferredPool {
            buckets: Default::default(),
        }
    }

    fn clear(&mut self) { self.buckets.clear() }

    fn insert(&mut self, tx: TxWithReadyInfo, force: bool) -> InsertResult {
        // It's safe to create a new bucket, cause inserting to a empty bucket
        // will always be success
        let bucket = self.buckets.entry(tx.sender).or_insert(NoncePool::new());
        bucket.insert(&tx, force)
    }

    fn contain_address(&self, addr: &Address) -> bool {
        self.buckets.contains_key(addr)
    }

    fn check_sender_and_nonce_exists(
        &self, sender: &Address, nonce: &U256,
    ) -> bool {
        if let Some(bucket) = self.buckets.get(sender) {
            bucket.check_nonce_exists(nonce)
        } else {
            false
        }
    }

    fn count_less(&self, sender: &Address, nonce: &U256) -> usize {
        if let Some(bucket) = self.buckets.get(sender) {
            bucket.count_less(nonce)
        } else {
            0
        }
    }

    fn remove_lowest_nonce(
        &mut self, addr: &Address,
    ) -> Option<TxWithReadyInfo> {
        match self.buckets.get_mut(addr) {
            None => None,
            Some(bucket) => {
                let ret = bucket.remove_lowest_nonce();
                if bucket.is_empty() {
                    self.buckets.remove(addr);
                }
                ret
            }
        }
    }

    fn get_lowest_nonce(&self, addr: &Address) -> Option<&U256> {
        self.buckets
            .get(addr)
            .and_then(|bucket| bucket.get_lowest_nonce_tx().map(|r| &r.nonce))
    }

    fn get_lowest_nonce_tx(
        &self, addr: &Address,
    ) -> Option<&SignedTransaction> {
        self.buckets
            .get(addr)
            .and_then(|bucket| bucket.get_lowest_nonce_tx())
    }

    fn recalculate_readiness_with_local_info(
        &mut self, addr: &Address, nonce: U256, balance: U256,
    ) -> Option<Arc<SignedTransaction>> {
        if let Some(bucket) = self.buckets.get(addr) {
            bucket.recalculate_readiness_with_local_info(nonce, balance)
        } else {
            None
        }
    }

    fn get_pending_info(
        &self, addr: &Address, nonce: &U256,
    ) -> Option<(usize, Arc<SignedTransaction>)> {
        if let Some(bucket) = self.buckets.get(addr) {
            bucket.get_pending_info(nonce)
        } else {
            None
        }
    }

    fn get_pending_transactions(
        &self, addr: &Address, start_nonce: &U256, local_nonce: &U256,
        local_balance: &U256,
    ) -> (Vec<Arc<SignedTransaction>>, Option<PendingReason>)
    {
        match self.buckets.get(addr) {
            Some(bucket) => {
                let pending_txs = bucket.get_pending_transactions(start_nonce);
                let pending_reason = pending_txs.first().and_then(|tx| {
                    bucket.check_pending_reason_with_local_info(
                        *local_nonce,
                        *local_balance,
                        tx.as_ref(),
                    )
                });
                (pending_txs, pending_reason)
            }
            None => (Vec::new(), None),
        }
    }

    fn check_tx_packed(&self, addr: Address, nonce: U256) -> bool {
        if let Some(bucket) = self.buckets.get(&addr) {
            if let Some(tx_with_ready_info) = bucket.get_tx_by_nonce(nonce) {
                tx_with_ready_info.is_already_packed()
            } else {
                false
            }
        } else {
            false
        }
    }

    fn last_succ_nonce(&self, addr: Address, from_nonce: U256) -> Option<U256> {
        let bucket = self.buckets.get(&addr)?;
        let mut next_nonce = from_nonce;
        loop {
            let nonce = bucket.succ_nonce(&next_nonce);
            if nonce.is_none() {
                break;
            }
            if nonce.unwrap() > next_nonce {
                break;
            }
            next_nonce += 1.into();
        }
        Some(next_nonce)
    }
}

#[derive(DeriveMallocSizeOf)]
struct ReadyAccountPool {
    treap: TreapMap<Address, Arc<SignedTransaction>, WeightType>,
    tx_weight_scaling: u64,
    tx_weight_exp: u8,
}

impl ReadyAccountPool {
    fn new(tx_weight_scaling: u64, tx_weight_exp: u8) -> Self {
        ReadyAccountPool {
            treap: TreapMap::new(),
            tx_weight_scaling,
            tx_weight_exp,
        }
    }

    fn clear(&mut self) {
        while self.len() != 0 {
            self.pop();
        }
    }

    fn len(&self) -> usize { self.treap.len() }

    fn get(&self, address: &Address) -> Option<Arc<SignedTransaction>> {
        self.treap.get(address).map(|tx| tx.clone())
    }

    fn remove(&mut self, address: &Address) -> Option<Arc<SignedTransaction>> {
        self.treap.remove(address)
    }

    fn update(
        &mut self, address: &Address, tx: Option<Arc<SignedTransaction>>,
    ) -> Option<Arc<SignedTransaction>> {
        let replaced = if let Some(tx) = tx {
            if tx.hash[0] & 254 == 0 {
                debug!("Sampled transaction {:?} in ready pool", tx.hash);
            }
            self.insert(tx)
        } else {
            self.remove(address)
        };
        replaced
    }

    fn insert(
        &mut self, tx: Arc<SignedTransaction>,
    ) -> Option<Arc<SignedTransaction>> {
        let scaled_weight = tx.gas_price / self.tx_weight_scaling;
        let base_weight = if scaled_weight == U256::zero() {
            0
        } else if scaled_weight >= *MAX_WEIGHT {
            u128::max_value()
        } else {
            scaled_weight.as_u128()
        };

        let mut weight = 1;
        for _ in 0..self.tx_weight_exp {
            weight *= base_weight;
        }

        self.treap.insert(tx.sender(), tx.clone(), weight)
    }

    fn pop(&mut self) -> Option<Arc<SignedTransaction>> {
        if self.treap.len() == 0 {
            return None;
        }

        let sum_gas_price = self.treap.sum_weight();
        let mut rand_value = rand::random();
        rand_value = rand_value % sum_gas_price;

        let tx = self
            .treap
            .get_by_weight(rand_value)
            .expect("Failed to pick transaction by weight")
            .clone();
        trace!("Get transaction from ready pool. tx: {:?}", tx.clone());

        self.remove(&tx.sender())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionStatus {
    Packed,
    Ready,
    Pending(PendingReason),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingReason {
    FutureNonce,
    NotEnoughCash,
}

#[derive(DeriveMallocSizeOf)]
pub struct TransactionPoolInner {
    capacity: usize,
    total_received_count: usize,
    unpacked_transaction_count: usize,
    /// Tracks all transactions in the transaction pool by account and nonce.
    /// Packed and executed transactions will eventually be garbage collected.
    deferred_pool: DeferredPool,
    /// Tracks the first unpacked ready transaction for accounts.
    /// Updated together with `ready_nonces_and_balances`.
    /// Also updated after transaction packing.
    ready_account_pool: ReadyAccountPool,
    /// The cache of the latest nonce and balance in the state.
    /// Updated with the storage data after a block is processed in consensus
    /// (set_tx_packed), after epoch execution, or during transaction
    /// insertion.
    ready_nonces_and_balances: HashMap<Address, (U256, U256)>,
    garbage_collector: GarbageCollector,
    /// Keeps all transactions in the transaction pool.
    /// It should contain the same transaction set as `deferred_pool`.
    txs: HashMap<H256, Arc<SignedTransaction>>,
    tx_sponsored_gas_map: HashMap<H256, (U256, u64)>,
}

impl TransactionPoolInner {
    pub fn new(
        capacity: usize, tx_weight_scaling: u64, tx_weight_exp: u8,
    ) -> Self {
        TransactionPoolInner {
            capacity,
            total_received_count: 0,
            unpacked_transaction_count: 0,
            deferred_pool: DeferredPool::new(),
            ready_account_pool: ReadyAccountPool::new(
                tx_weight_scaling,
                tx_weight_exp,
            ),
            ready_nonces_and_balances: HashMap::new(),
            garbage_collector: GarbageCollector::default(),
            txs: HashMap::new(),
            tx_sponsored_gas_map: HashMap::new(),
        }
    }

    pub fn clear(&mut self) {
        self.deferred_pool.clear();
        self.ready_account_pool.clear();
        self.ready_nonces_and_balances.clear();
        self.garbage_collector.clear();
        self.txs.clear();
        self.tx_sponsored_gas_map.clear();
        self.total_received_count = 0;
        self.unpacked_transaction_count = 0;
    }

    pub fn total_deferred(&self) -> usize { self.txs.len() }

    pub fn total_ready_accounts(&self) -> usize {
        self.ready_account_pool.len()
    }

    pub fn total_received(&self) -> usize { self.total_received_count }

    pub fn total_unpacked(&self) -> usize { self.unpacked_transaction_count }

    pub fn get(&self, tx_hash: &H256) -> Option<Arc<SignedTransaction>> {
        self.txs.get(tx_hash).map(|x| x.clone())
    }

    pub fn get_by_address2nonce(
        &self, address: Address, nonce: U256,
    ) -> Option<Arc<SignedTransaction>> {
        let bucket = self.deferred_pool.buckets.get(&address)?;
        bucket.get_tx_by_nonce(nonce).map(|tx| tx.transaction)
    }

    pub fn is_full(&self) -> bool {
        return self.total_deferred() >= self.capacity;
    }

    pub fn get_current_timestamp(&self) -> u64 {
        let start = SystemTime::now();
        let since_the_epoch = start.duration_since(UNIX_EPOCH).unwrap();
        since_the_epoch.as_secs()
    }

    /// A sender has a transaction which is garbage collectable if
    ///    1. there is at least a transaction whose nonce is less than
    /// `ready_nonce`
    ///    2. the nonce of all transactions are greater than or equal to
    /// `ready_nonce` and it is not garbage collected during the last
    /// `TIME_WINDOW` seconds
    ///
    /// We will pick a sender who has maximum number of transactions which are
    /// garbage collectable. And if there is a tie, the one who has minimum
    /// timestamp will be picked.
    pub fn collect_garbage(&mut self, new_tx: &SignedTransaction) {
        let count_before_gc = self.total_deferred();
        while self.is_full() && !self.garbage_collector.is_empty() {
            let current_timestamp = self.get_current_timestamp();
            let victim = {
                let mut cnt = GC_CHECK_COUNT;
                let mut poped_nodes = Vec::new();
                let mut victim = None;
                let mut min_gas_price = new_tx.gas_price;
                while !self.garbage_collector.is_empty() && cnt != 0 {
                    let node = self.garbage_collector.pop().unwrap();
                    // Accounts which are not in `deferred_pool` may be inserted
                    // into `garbage_collector`, we can just
                    // ignore them.
                    if !self.deferred_pool.contain_address(&node.sender) {
                        continue;
                    }
                    poped_nodes.push(node.clone());

                    // This node has executed transactions to GC. No need to
                    // check more.
                    if node.count > 0 {
                        victim = Some(node);
                        break;
                    }

                    // We do not GC a transaction from the same sender.
                    if node.sender == new_tx.sender {
                        continue;
                    }

                    // If all accounts are ready, we choose the one whose first
                    // tx has the minimal gas price.
                    let to_remove_tx = self
                        .deferred_pool
                        .get_lowest_nonce_tx(&node.sender)
                        .unwrap();
                    if to_remove_tx.gas_price < min_gas_price {
                        min_gas_price = to_remove_tx.gas_price;
                        victim = Some(node);
                    }
                    cnt -= 1;
                }
                // Insert back other nodes to keep `garbage_collector`
                // unchanged.
                for node in poped_nodes {
                    if victim.is_some()
                        && node.sender == victim.as_ref().unwrap().sender
                    {
                        // skip victim
                        continue;
                    }
                    self.garbage_collector.insert(
                        &node.sender,
                        node.count,
                        node.timestamp,
                    );
                }
                match victim {
                    Some(victim) => victim,
                    None => return,
                }
            };
            let addr = victim.sender;

            // All transactions are not garbage collectable.

            let (ready_nonce, _) = self
                .get_local_nonce_and_balance(&addr)
                .unwrap_or((0.into(), 0.into()));

            let to_remove_tx =
                self.deferred_pool.get_lowest_nonce_tx(&addr).unwrap();

            // We have to garbage collect an unexecuted transaction.
            // TODO: Implement more heuristic strategies
            if to_remove_tx.nonce >= ready_nonce {
                assert_eq!(victim.count, 0);
                GC_UNEXECUTED_COUNTER.inc(1);
                warn!("an unexecuted tx is garbage-collected.");
            }

            // maintain ready account pool
            if let Some(ready_tx) = self.ready_account_pool.get(&addr) {
                if ready_tx.hash() == to_remove_tx.hash() {
                    warn!("a ready tx is garbage-collected");
                    GC_READY_COUNTER.inc(1);
                    self.ready_account_pool.remove(&addr);
                }
            }

            if !self
                .deferred_pool
                .check_tx_packed(addr.clone(), to_remove_tx.nonce)
            {
                self.unpacked_transaction_count = self
                    .unpacked_transaction_count
                    .checked_sub(1)
                    .unwrap_or_else(|| {
                        error!("unpacked_transaction_count under-flows.");
                        0
                    });
            }

            let removed_tx = self
                .deferred_pool
                .remove_lowest_nonce(&addr)
                .unwrap()
                .get_arc_tx()
                .clone();

            // maintain ready info
            if !self.deferred_pool.contain_address(&addr) {
                self.ready_nonces_and_balances.remove(&addr);
            // The picked sender has no transactions now, and has been popped
            // from `garbage_collector`.
            } else {
                if victim.count > 0 {
                    self.garbage_collector.insert(
                        &addr,
                        victim.count - 1,
                        current_timestamp,
                    );
                } else {
                    self.garbage_collector.insert(&addr, 0, current_timestamp);
                }
            }

            // maintain txs
            self.txs.remove(&removed_tx.hash());
            self.tx_sponsored_gas_map.remove(&removed_tx.hash());
        }

        GC_METER.mark(count_before_gc - self.total_deferred());
    }

    /// Collect garbage and return the remaining quota of the pool to insert new
    /// transactions.
    pub fn remaining_quota(&self) -> usize {
        let len = self.total_deferred();
        self.capacity - len + self.garbage_collector.gc_size()
    }

    pub fn capacity(&self) -> usize { self.capacity }

    // the new inserting will fail if tx_pool is full (even if `force` is true)
    fn insert_transaction_without_readiness_check(
        &mut self, transaction: Arc<SignedTransaction>, packed: bool,
        force: bool, state_nonce_and_balance: Option<(U256, U256)>,
        (sponsored_gas, sponsored_storage): (U256, u64),
    ) -> InsertResult
    {
        let _timer = MeterTimer::time_func(
            TX_POOL_INNER_WITHOUTCHECK_INSERT_TIMER.as_ref(),
        );
        if !self.deferred_pool.check_sender_and_nonce_exists(
            &transaction.sender(),
            &transaction.nonce(),
        ) {
            self.collect_garbage(transaction.as_ref());
            if self.is_full() {
                return InsertResult::Failed("Transaction Pool is full".into());
            }
        }
        let result = {
            let _timer =
                MeterTimer::time_func(DEFERRED_POOL_INNER_INSERT.as_ref());
            self.deferred_pool.insert(
                TxWithReadyInfo {
                    transaction: transaction.clone(),
                    packed,
                    sponsored_gas,
                    sponsored_storage,
                },
                force,
            )
        };

        match &result {
            InsertResult::NewAdded => {
                // This will only happen when called by
                // `insert_transaction_with_readiness_check`, so
                // state_nonce_and_balance will never be `None`.
                let (state_nonce, state_balance) =
                    state_nonce_and_balance.unwrap();
                self.update_nonce_and_balance(
                    &transaction.sender(),
                    state_nonce,
                    state_balance,
                );
                let count = self
                    .deferred_pool
                    .count_less(&transaction.sender(), &state_nonce);
                let timestamp = self
                    .garbage_collector
                    .get_timestamp(&transaction.sender())
                    .unwrap_or(self.get_current_timestamp());
                self.garbage_collector.insert(
                    &transaction.sender(),
                    count,
                    timestamp,
                );
                self.txs.insert(transaction.hash(), transaction.clone());
                self.tx_sponsored_gas_map.insert(
                    transaction.hash(),
                    (sponsored_gas, sponsored_storage),
                );
                if !packed {
                    self.unpacked_transaction_count += 1;
                }
            }
            InsertResult::Failed(_) => {}
            InsertResult::Updated(replaced_tx) => {
                if !replaced_tx.is_already_packed() {
                    self.unpacked_transaction_count = self
                        .unpacked_transaction_count
                        .checked_sub(1)
                        .unwrap_or_else(|| {
                            error!("unpacked_transaction_count under-flows.");
                            0
                        });
                }
                self.txs.remove(&replaced_tx.hash());
                self.txs.insert(transaction.hash(), transaction.clone());
                self.tx_sponsored_gas_map.remove(&replaced_tx.hash());
                self.tx_sponsored_gas_map.insert(
                    transaction.hash(),
                    (sponsored_gas, sponsored_storage),
                );
                if !packed {
                    self.unpacked_transaction_count += 1;
                }
            }
        }

        result
    }

    pub fn get_account_pending_info(
        &self, address: &Address,
    ) -> Option<(U256, U256, U256, H256)> {
        let (local_nonce, _local_balance) = self
            .get_local_nonce_and_balance(address)
            .unwrap_or((U256::from(0), U256::from(0)));
        match self.deferred_pool.get_pending_info(address, &local_nonce) {
            Some((pending_count, pending_tx)) => Some((
                local_nonce,
                U256::from(pending_count),
                pending_tx.nonce(),
                pending_tx.hash(),
            )),
            None => {
                Some((local_nonce, U256::from(0), U256::from(0), H256::zero()))
            }
        }
    }

    pub fn get_account_pending_transactions(
        &self, address: &Address, maybe_start_nonce: Option<U256>,
        maybe_limit: Option<usize>,
    ) -> (
        Vec<Arc<SignedTransaction>>,
        Option<TransactionStatus>,
        usize,
    )
    {
        let (local_nonce, local_balance) = self
            .get_local_nonce_and_balance(address)
            .unwrap_or((U256::from(0), U256::from(0)));
        let start_nonce = maybe_start_nonce.unwrap_or(local_nonce);
        let (pending_txs, pending_reason) =
            self.deferred_pool.get_pending_transactions(
                address,
                &start_nonce,
                &local_nonce,
                &local_balance,
            );
        if pending_txs.is_empty() {
            return (Vec::new(), None, 0);
        }
        let first_tx_status = match pending_reason {
            None => {
                // Sanity check with `ready_account_pool`.
                match self.ready_account_pool.get(address) {
                    None => {
                        error!(
                            "Ready tx not in ready_account_pool: tx={:?}",
                            pending_txs.first()
                        );
                    }
                    Some(ready_tx) => {
                        let first_tx = pending_txs.first().expect("not empty");
                        if ready_tx.hash() != first_tx.hash() {
                            error!("ready_account_pool and deferred_pool are inconsistent! ready_tx={:?} first_pending={:?}", ready_tx.hash(), first_tx.hash());
                        }
                    }
                }
                TransactionStatus::Ready
            }
            Some(reason) => TransactionStatus::Pending(reason),
        };
        let pending_count = pending_txs.len();
        let limit = maybe_limit.unwrap_or(usize::MAX);
        (
            pending_txs.into_iter().take(limit).collect(),
            Some(first_tx_status),
            pending_count,
        )
    }

    pub fn get_local_nonce_and_balance(
        &self, address: &Address,
    ) -> Option<(U256, U256)> {
        self.ready_nonces_and_balances.get(address).map(|x| *x)
    }

    fn update_nonce_and_balance(
        &mut self, address: &Address, nonce: U256, balance: U256,
    ) {
        if !self.deferred_pool.contain_address(address) {
            return;
        }
        let count = self.deferred_pool.count_less(address, &nonce);
        let timestamp = self
            .garbage_collector
            .get_timestamp(address)
            .unwrap_or(self.get_current_timestamp());
        self.garbage_collector.insert(address, count, timestamp);
        self.ready_nonces_and_balances
            .insert((*address).clone(), (nonce, balance));
    }

    fn get_and_update_nonce_and_balance_from_storage(
        &mut self, address: &Address, state: &AccountCache,
    ) -> StateDbResult<(U256, U256)> {
        let nonce_and_balance = state.get_nonce_and_balance(address)?;
        if !self.deferred_pool.contain_address(address) {
            return Ok(nonce_and_balance);
        }
        let count =
            self.deferred_pool.count_less(address, &nonce_and_balance.0);
        let timestamp = self
            .garbage_collector
            .get_timestamp(address)
            .unwrap_or(self.get_current_timestamp());
        self.garbage_collector.insert(address, count, timestamp);
        self.ready_nonces_and_balances
            .insert((*address).clone(), nonce_and_balance);

        Ok(nonce_and_balance)
    }

    pub fn get_lowest_nonce(&self, addr: &Address) -> U256 {
        let mut ret = 0.into();
        if let Some((nonce, _)) = self.get_local_nonce_and_balance(addr) {
            ret = nonce;
        }
        if let Some(nonce) = self.deferred_pool.get_lowest_nonce(addr) {
            if *nonce < ret {
                ret = *nonce;
            }
        }
        ret
    }

    pub fn get_next_nonce(&self, address: &Address, state_nonce: U256) -> U256 {
        self.deferred_pool
            .last_succ_nonce(*address, state_nonce)
            .unwrap_or(state_nonce)
    }

    fn recalculate_readiness_with_local_info(&mut self, addr: &Address) {
        let (nonce, balance) = self
            .get_local_nonce_and_balance(addr)
            .unwrap_or((0.into(), 0.into()));
        let ret = self
            .deferred_pool
            .recalculate_readiness_with_local_info(addr, nonce, balance);
        self.ready_account_pool.update(addr, ret);
    }

    fn recalculate_readiness_with_fixed_info(
        &mut self, addr: &Address, nonce: U256, balance: U256,
    ) {
        self.update_nonce_and_balance(addr, nonce, balance);
        let ret = self
            .deferred_pool
            .recalculate_readiness_with_local_info(addr, nonce, balance);
        self.ready_account_pool.update(addr, ret);
    }

    fn recalculate_readiness_with_state(
        &mut self, addr: &Address, account_cache: &AccountCache,
    ) -> StateDbResult<()> {
        let _timer = MeterTimer::time_func(TX_POOL_RECALCULATE.as_ref());
        let (nonce, balance) = self
            .get_and_update_nonce_and_balance_from_storage(
                addr,
                account_cache,
            )?;
        let ret = self
            .deferred_pool
            .recalculate_readiness_with_local_info(addr, nonce, balance);
        self.ready_account_pool.update(addr, ret);

        Ok(())
    }

    pub fn check_tx_packed_in_deferred_pool(&self, tx_hash: &H256) -> bool {
        match self.txs.get(tx_hash) {
            Some(tx) => {
                self.deferred_pool.check_tx_packed(tx.sender(), tx.nonce())
            }
            None => false,
        }
    }

    /// pack at most num_txs transactions randomly
    pub fn pack_transactions<'a>(
        &mut self, num_txs: usize, block_gas_limit: U256,
        block_size_limit: usize, best_epoch_height: u64,
        best_block_number: u64, verification_config: &VerificationConfig,
        machine: &Machine,
    ) -> Vec<Arc<SignedTransaction>>
    {
        let mut packed_transactions: Vec<Arc<SignedTransaction>> = Vec::new();
        if num_txs == 0 {
            return packed_transactions;
        }

        let mut total_tx_gas_limit: U256 = 0.into();
        let mut total_tx_size: usize = 0;

        let mut big_tx_resample_times_limit = 10;
        let mut recycle_txs = Vec::new();

        let spec = machine.spec(best_block_number);
        let transitions = &machine.params().transition_heights;

        'out: while let Some(tx) = self.ready_account_pool.pop() {
            let tx_size = tx.rlp_size();
            if block_gas_limit - total_tx_gas_limit < *tx.gas_limit()
                || block_size_limit - total_tx_size < tx_size
            {
                recycle_txs.push(tx.clone());
                if big_tx_resample_times_limit > 0 {
                    big_tx_resample_times_limit -= 1;
                    continue 'out;
                } else {
                    break 'out;
                }
            }

            // The validity of a transaction may change during the time.
            match verification_config.fast_recheck(
                &tx,
                best_epoch_height,
                transitions,
                &spec,
            ) {
                PackingCheckResult::Pack => {}
                PackingCheckResult::Pending => {
                    recycle_txs.push(tx.clone());
                    continue 'out;
                }
                PackingCheckResult::Drop => {
                    continue 'out;
                }
            }

            total_tx_gas_limit += *tx.gas_limit();
            total_tx_size += tx_size;

            packed_transactions.push(tx.clone());
            self.insert_transaction_without_readiness_check(
                tx.clone(),
                true, /* packed */
                true, /* force */
                None, /* state_nonce_and_balance */
                self.tx_sponsored_gas_map
                    .get(&tx.hash())
                    .map(|x| x.clone())
                    .unwrap_or((U256::from(0), 0)),
            );
            self.recalculate_readiness_with_local_info(&tx.sender());

            if packed_transactions.len() >= num_txs {
                break 'out;
            }
        }

        for tx in recycle_txs {
            self.ready_account_pool.insert(tx);
        }

        // FIXME: to be optimized by only recalculating readiness once for one
        //  sender
        for tx in packed_transactions.iter().rev() {
            self.insert_transaction_without_readiness_check(
                tx.clone(),
                false, /* packed */
                true,  /* force */
                None,  /* state_nonce_and_balance */
                self.tx_sponsored_gas_map
                    .get(&tx.hash())
                    .map(|x| x.clone())
                    .unwrap_or((U256::from(0), 0)),
            );
            self.recalculate_readiness_with_local_info(&tx.sender());
        }

        if log::max_level() >= log::Level::Debug {
            let mut rlp_s = RlpStream::new();
            for tx in &packed_transactions {
                rlp_s.append::<TransactionWithSignature>(&**tx);
            }
            debug!(
                "After packing packed_transactions: {}, rlp size: {}",
                packed_transactions.len(),
                rlp_s.out().len(),
            );
        }

        packed_transactions
    }

    pub fn notify_modified_accounts(
        &mut self, accounts_from_execution: Vec<Account>,
    ) {
        for account in &accounts_from_execution {
            self.recalculate_readiness_with_fixed_info(
                account.address(),
                account.nonce,
                account.balance,
            );
        }
    }

    /// content retrieves the ready and deferred transactions.
    pub fn content(
        &self, address: Option<Address>,
    ) -> (Vec<Arc<SignedTransaction>>, Vec<Arc<SignedTransaction>>) {
        let ready_txs = self
            .ready_account_pool
            .treap
            .iter()
            .filter(|address_tx| {
                address == None || &address.unwrap() == address_tx.0
            })
            .map(|(_, tx)| tx.clone())
            .collect();

        let deferred_txs = self
            .txs
            .values()
            .filter(|tx| address == None || tx.sender == address.unwrap())
            .map(|v| v.clone())
            .collect();

        (ready_txs, deferred_txs)
    }

    // Add transaction into deferred pool and maintain its readiness
    // the packed tag provided
    // if force tag is true, the replacement in nonce pool must be happened
    pub fn insert_transaction_with_readiness_check(
        &mut self, account_cache: &AccountCache,
        transaction: Arc<SignedTransaction>, packed: bool, force: bool,
    ) -> Result<(), String>
    {
        let _timer = MeterTimer::time_func(TX_POOL_INNER_INSERT_TIMER.as_ref());
        let mut sponsored_gas = U256::from(0);
        let mut sponsored_storage = 0;

        // Compute sponsored_gas for `transaction`
        if let Action::Call(callee) = &transaction.action {
            // FIXME: This is a quick fix for performance issue.
            if callee.maybe_contract_address() {
                if let Some(sponsor_info) =
                    account_cache.get_sponsor_info(callee).map_err(|e| {
                        format!(
                            "Failed to read account_cache from storage: {}",
                            e
                        )
                    })?
                {
                    if account_cache
                        .check_commission_privilege(
                            &callee,
                            &transaction.sender(),
                        )
                        .map_err(|e| {
                            format!(
                                "Failed to read account_cache from storage: {}",
                                e
                            )
                        })?
                    {
                        let estimated_gas_u512 =
                            transaction.gas.full_mul(transaction.gas_price);
                        // Normally, it is less than 2^128
                        let estimated_gas = if estimated_gas_u512
                            > U512::from(U128::max_value())
                        {
                            U256::from(U128::max_value())
                        } else {
                            transaction.gas * transaction.gas_price
                        };
                        if estimated_gas <= sponsor_info.sponsor_gas_bound
                            && estimated_gas
                                <= sponsor_info.sponsor_balance_for_gas
                        {
                            sponsored_gas = transaction.gas;
                        }
                        let estimated_collateral =
                            U256::from(transaction.storage_limit)
                                * *DRIPS_PER_STORAGE_COLLATERAL_UNIT;
                        if estimated_collateral
                            <= sponsor_info.sponsor_balance_for_collateral
                        {
                            sponsored_storage = transaction.storage_limit;
                        }
                    }
                }
            }
        }

        let (state_nonce, state_balance) = account_cache
            .get_nonce_and_balance(&transaction.sender)
            .map_err(|e| {
                format!("Failed to read account_cache from storage: {}", e)
            })?;

        if transaction.hash[0] & 254 == 0 {
            trace!(
                "Transaction {:?} sender: {:?} current nonce: {:?}, state nonce:{:?}",
                transaction.hash, transaction.sender, transaction.nonce, state_nonce
            );
        }
        if transaction.nonce
            >= state_nonce
                + U256::from(FURTHEST_FUTURE_TRANSACTION_NONCE_OFFSET)
        {
            trace!(
                "Transaction {:?} is discarded due to in too distant future",
                transaction.hash()
            );
            return Err(format!(
                "Transaction {:?} is discarded due to in too distant future",
                transaction.hash()
            ));
        } else if !packed /* Because we may get slightly out-dated state for transaction pool, we should allow transaction pool to set already past-nonce transactions to packed. */
            && transaction.nonce < state_nonce
        {
            trace!(
                "Transaction {:?} is discarded due to a too stale nonce, self.nonce={}, state_nonce={}",
                transaction.hash(), transaction.nonce, state_nonce,
            );
            return Err(format!(
                "Transaction {:?} is discarded due to a too stale nonce",
                transaction.hash()
            ));
        }

        let result = self.insert_transaction_without_readiness_check(
            transaction.clone(),
            packed,
            force,
            Some((state_nonce, state_balance)),
            (sponsored_gas, sponsored_storage),
        );
        if let InsertResult::Failed(info) = result {
            return Err(format!("Failed imported to deferred pool: {}", info));
        }

        self.recalculate_readiness_with_state(
            &transaction.sender,
            account_cache,
        )
        .map_err(|e| {
            format!("Failed to read account_cache from storage: {}", e)
        })?;

        Ok(())
    }
}

#[cfg(test)]
mod test_transaction_pool_inner {
    use super::{DeferredPool, InsertResult, TxWithReadyInfo};
    use cfx_types::{Address, U256};
    use keylib::{Generator, KeyPair, Random};
    use primitives::{Action, SignedTransaction, Transaction};
    use std::sync::Arc;

    fn new_test_tx(
        sender: &KeyPair, nonce: usize, gas_price: usize, value: usize,
    ) -> Arc<SignedTransaction> {
        Arc::new(
            Transaction {
                nonce: U256::from(nonce),
                gas_price: U256::from(gas_price),
                gas: U256::from(50000),
                action: Action::Call(Address::random()),
                value: U256::from(value),
                storage_limit: 0,
                epoch_height: 0,
                chain_id: 0,
                data: Vec::new(),
            }
            .sign(sender.secret()),
        )
    }

    fn new_test_tx_with_read_info(
        sender: &KeyPair, nonce: usize, gas_price: usize, value: usize,
        packed: bool,
    ) -> TxWithReadyInfo
    {
        let transaction = new_test_tx(sender, nonce, gas_price, value);
        TxWithReadyInfo {
            transaction,
            packed,
            sponsored_gas: U256::from(0),
            sponsored_storage: 0,
        }
    }

    #[test]
    fn test_deferred_pool_insert_and_remove() {
        let mut deferred_pool = DeferredPool::new();

        // insert txs of same sender
        let alice = Random.generate().unwrap();
        let bob = Random.generate().unwrap();
        let eva = Random.generate().unwrap();

        let alice_tx1 = new_test_tx_with_read_info(
            &alice, 5, 10, 100, false, /* packed */
        );
        let alice_tx2 = new_test_tx_with_read_info(
            &alice, 6, 10, 100, false, /* packed */
        );
        let bob_tx1 = new_test_tx_with_read_info(
            &bob, 1, 10, 100, false, /* packed */
        );
        let bob_tx2 = new_test_tx_with_read_info(
            &bob, 2, 10, 100, false, /* packed */
        );
        let bob_tx2_new = new_test_tx_with_read_info(
            &bob, 2, 11, 100, false, /* packed */
        );

        assert_eq!(
            deferred_pool.insert(alice_tx1.clone(), false /* force */),
            InsertResult::NewAdded
        );

        assert_eq!(deferred_pool.contain_address(&alice.address()), true);

        assert_eq!(deferred_pool.contain_address(&eva.address()), false);

        assert_eq!(deferred_pool.remove_lowest_nonce(&eva.address()), None);

        assert_eq!(deferred_pool.contain_address(&bob.address()), false);

        assert_eq!(
            deferred_pool.insert(alice_tx2.clone(), false /* force */),
            InsertResult::NewAdded
        );

        assert_eq!(deferred_pool.remove_lowest_nonce(&bob.address()), None);

        assert_eq!(
            deferred_pool.insert(bob_tx1.clone(), false /* force */),
            InsertResult::NewAdded
        );

        assert_eq!(deferred_pool.contain_address(&bob.address()), true);

        assert_eq!(
            deferred_pool.insert(bob_tx2.clone(), false /* force */),
            InsertResult::NewAdded
        );

        assert_eq!(
            deferred_pool.insert(bob_tx2_new.clone(), false /* force */),
            InsertResult::Updated(bob_tx2.clone())
        );

        assert_eq!(
            deferred_pool.insert(bob_tx2.clone(), false /* force */),
            InsertResult::Failed(format!("Tx with same nonce already inserted. To replace it, you need to specify a gas price > {}", bob_tx2_new.gas_price))
        );

        assert_eq!(
            deferred_pool.get_lowest_nonce(&bob.address()),
            Some(&(1.into()))
        );

        assert_eq!(
            deferred_pool.remove_lowest_nonce(&bob.address()),
            Some(bob_tx1.clone())
        );

        assert_eq!(
            deferred_pool.get_lowest_nonce(&bob.address()),
            Some(&(2.into()))
        );

        assert_eq!(deferred_pool.contain_address(&bob.address()), true);

        assert_eq!(
            deferred_pool.remove_lowest_nonce(&bob.address()),
            Some(bob_tx2_new.clone())
        );

        assert_eq!(deferred_pool.get_lowest_nonce(&bob.address()), None);

        assert_eq!(deferred_pool.contain_address(&bob.address()), false);
    }

    #[test]
    fn test_deferred_pool_recalculate_readiness() {
        let mut deferred_pool = super::DeferredPool::new();

        let alice = Random.generate().unwrap();

        let gas = 50000;
        let tx1 = new_test_tx_with_read_info(
            &alice, 5, 10, 10000, true, /* packed */
        );
        let tx2 = new_test_tx_with_read_info(
            &alice, 6, 10, 10000, true, /* packed */
        );
        let tx3 = new_test_tx_with_read_info(
            &alice, 7, 10, 10000, true, /* packed */
        );
        let tx4 = new_test_tx_with_read_info(
            &alice, 8, 10, 10000, false, /* packed */
        );
        let tx5 = new_test_tx_with_read_info(
            &alice, 9, 10, 10000, false, /* packed */
        );
        let exact_cost = 4 * (gas * 10 + 10000);

        deferred_pool.insert(tx1.clone(), false /* force */);
        deferred_pool.insert(tx2.clone(), false /* force */);
        deferred_pool.insert(tx4.clone(), false /* force */);
        deferred_pool.insert(tx5.clone(), false /* force */);

        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                5.into(),
                exact_cost.into()
            ),
            None
        );

        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                7.into(),
                exact_cost.into()
            ),
            None
        );

        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                8.into(),
                exact_cost.into()
            ),
            Some(tx4.transaction.clone())
        );

        deferred_pool.insert(tx3.clone(), false /* force */);
        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                4.into(),
                exact_cost.into()
            ),
            None
        );

        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                5.into(),
                exact_cost.into()
            ),
            Some(tx4.transaction.clone())
        );

        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                7.into(),
                exact_cost.into()
            ),
            Some(tx4.transaction.clone())
        );

        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                8.into(),
                exact_cost.into()
            ),
            Some(tx4.transaction.clone())
        );

        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                9.into(),
                exact_cost.into()
            ),
            Some(tx5.transaction.clone())
        );

        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                10.into(),
                exact_cost.into()
            ),
            None
        );

        assert_eq!(
            deferred_pool.recalculate_readiness_with_local_info(
                &alice.address(),
                5.into(),
                (exact_cost - 1).into()
            ),
            None
        );
    }
}
