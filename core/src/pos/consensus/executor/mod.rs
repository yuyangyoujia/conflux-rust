// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0
// NOTICE: Code has revised accordingly by Conflux Foundation.

use anyhow::{bail, ensure, format_err, Result};
use diem_config::config::NodeConfig;
use diem_crypto::{
    hash::{GENESIS_BLOCK_ID, PRE_GENESIS_BLOCK_ID},
    HashValue,
};
use diem_types::{
    block_info::{BlockInfo, PivotBlockDecision, Round},
    contract_event::ContractEvent,
    ledger_info::{LedgerInfo, LedgerInfoWithSignatures},
    on_chain_config::{NextValidatorSetProposal, ValidatorSet},
    transaction::{
        Transaction, TransactionOutput, TransactionPayload, TransactionStatus,
        WriteSetPayload,
    },
    validator_verifier::{ValidatorVerifier, VerifyError},
    vm_status::{KeptVMStatus, StatusCode, VMStatus},
    write_set::WriteSet,
};
use diemdb::DiemDB;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use storage_interface::DbReader;

const GENESIS_MEMBERSHIP_ID: u64 = 0;
const GENESIS_ROUND: Round = 0;

/// A structure that summarizes the result of the execution needed for consensus
/// to agree on. The execution is responsible for generating the ID of the new
/// state, which is returned in the result.
///
/// Not every transaction in the payload succeeds: the returned vector keeps the
/// boolean status of success / failure of the transactions.
/// Note that the specific details of compute_status are opaque to
/// StateMachineReplication, which is going to simply pass the results between
/// StateComputer and TxnManager.
#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct StateComputeResult {
    pub executed_state: ExecutedState,
}

impl StateComputeResult {
    pub fn has_reconfiguration(&self) -> bool {
        self.executed_state.validators.is_some()
    }
}

/// Executed state derived from StateComputeResult that is maintained with every
/// proposed block. `state_id`(transaction accumulator root hash) summarized
/// both the information of the version and the validators.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutedState {
    /// Tracks the last pivot selection of a proposed block
    pub pivot: Option<PivotBlockDecision>,
    /// Tracks the execution state of a proposed block
    //pub state_id: HashValue,
    /// Version of after executing a proposed block.  This state must be
    /// persisted to ensure that on restart that the version is calculated
    /// correctly
    //pub version: Version,
    /// If set, this is the validator set that should be changed to if this
    /// block is committed. TODO [Reconfiguration] the validators are
    /// currently ignored, no reconfiguration yet.
    pub validators: Option<ValidatorSet>,
}

/// Generated by processing VM's output.
#[derive(Debug, Clone)]
pub struct ProcessedVMOutput {
    /// The entire set of data associated with each transaction.
    //transaction_data: Vec<TransactionData>,

    /// The in-memory Merkle Accumulator and state Sparse Merkle Tree after
    /// appending all the transactions in this set.
    //executed_trees: ExecutedTrees,

    /// If set, this is the validator set that should be changed to if this
    /// block is committed. TODO [Reconfiguration] the validators are
    /// currently ignored, no reconfiguration yet.
    pub validators: Option<ValidatorSet>,
    /// If set, this is the selected pivot block in current transaction.
    pub pivot_block: Option<PivotBlockDecision>,
    /// Whether the pivot_block is the updated value by executing this block.
    pub pivot_updated: bool,
}

impl ProcessedVMOutput {
    pub fn new(
        //transaction_data: Vec<TransactionData>,
        //executed_trees: ExecutedTrees,
        validators: Option<ValidatorSet>,
        pivot_block: Option<PivotBlockDecision>,
        pivot_updated: bool,
    ) -> Self
    {
        ProcessedVMOutput {
            //transaction_data,
            //executed_trees,
            validators,
            pivot_block,
            pivot_updated,
        }
    }

    pub fn validators(&self) -> &Option<ValidatorSet> { &self.validators }

    pub fn pivot_block(&self) -> &Option<PivotBlockDecision> {
        &self.pivot_block
    }

    pub fn pivot_updated(&self) -> bool { self.pivot_updated }

    // This method should only be called by tests.
    pub fn set_validators(&mut self, validator_set: ValidatorSet) {
        self.validators = Some(validator_set)
    }

    pub fn state_compute_result(&self) -> StateComputeResult {
        //let num_leaves =
        // self.executed_trees().txn_accumulator().num_leaves();
        // let version = if num_leaves == 0 { 0 } else { num_leaves - 1 };
        StateComputeResult {
            // Now that we have the root hash and execution status we can send
            // the response to consensus.
            // TODO: The VM will support a special transaction to set the
            // validators for the next membership_id that is part of a block
            // execution.
            executed_state: ExecutedState {
                pivot: self.pivot_block.clone(),
                validators: self.validators.clone(),
            },
        }
    }
}

/// `Executor` implements all functionalities the execution module needs to
/// provide.
pub struct Executor {
    db: Arc<DiemDB>,
    validators: RwLock<Option<ValidatorVerifier>>,
}

impl Executor {
    /// Constructs an `Executor`.
    pub fn new(config: &NodeConfig, db: Arc<DiemDB>) -> Self {
        let mut executor = Executor {
            db,
            //By default all initial validators are admin.
            /*validators: RwLock::new(Some(
                (&config.consensus.consensus_peers.get_validator_set()).into(),
            )),*/
            validators: RwLock::new(None),
        };

        if executor
            .db
            .get_startup_info()
            .expect("Shouldn't fail")
            .is_none()
        {
            let genesis_txn = config
                .execution
                .genesis
                .as_ref()
                .expect("failed to load genesis transaction!")
                .clone();
            executor.init_genesis(genesis_txn);
        }
        executor
    }

    /// This is used when we start for the first time and the DB is completely
    /// empty. It will write necessary information to DB by committing the
    /// genesis transaction.
    fn init_genesis(&mut self, genesis_txn: Transaction) {
        let genesis_txns = vec![genesis_txn];

        info!("PRE_GENESIS_BLOCK_ID: {}", *PRE_GENESIS_BLOCK_ID);

        // Create a block with genesis_txn being the only transaction. Execute
        // it then commit it immediately.
        // We create `PRE_GENESIS_BLOCK_ID` as the parent of the genesis block.
        let output = self
            .execute_block(
                genesis_txns.clone(),
                None, /* last_pivot */
                *PRE_GENESIS_BLOCK_ID,
                *GENESIS_BLOCK_ID,
                GENESIS_MEMBERSHIP_ID,
                false, /* verify_admin_transaction */
            )
            .expect("Failed to execute genesis block.");

        let ledger_info = LedgerInfo::new(
            BlockInfo::new(
                GENESIS_MEMBERSHIP_ID,
                GENESIS_ROUND,
                *PRE_GENESIS_BLOCK_ID,
                HashValue::zero(),
                0,
                0,
                None,
            ),
            HashValue::zero(),
        );
        let ledger_info_with_sigs = LedgerInfoWithSignatures::new(
            ledger_info,
            /* signatures = */ BTreeMap::new(),
        );
        self.commit_blocks(
            vec![(genesis_txns, Arc::new(output))],
            ledger_info_with_sigs,
        )
        .expect("Failed to commit genesis block.");
        info!("GENESIS transaction is committed.")
    }

    pub fn get_diem_db(&self) -> Arc<DiemDB> { self.db.clone() }

    pub fn set_validators(&self, validators: ValidatorVerifier) {
        let mut v = self.validators.write();
        *v = Some(validators);
    }

    fn gen_output(events: Vec<ContractEvent>) -> TransactionOutput {
        let vm_status = KeptVMStatus::Executed;

        let status = TransactionStatus::Keep(vm_status);

        TransactionOutput::new(WriteSet::default(), events, 0, status)
    }

    /// Executes a block.
    pub fn execute_block(
        &self, transactions: Vec<Transaction>,
        last_pivot: Option<PivotBlockDecision>, parent_id: HashValue,
        id: HashValue, current_membership_id: u64,
        verify_admin_transaction: bool,
    ) -> Result<ProcessedVMOutput>
    {
        debug!(
            "Received request to execute block. Parent id: {:x}. Id: {:x}.",
            parent_id, id
        );

        ensure!(
            transactions.len() <= 2,
            "One block can at most contain 1 user transaction for proposal."
        );
        let mut vm_outputs = Vec::new();
        for transaction in transactions {
            // Execute the transaction
            match transaction {
                Transaction::BlockMetadata(_data) => {}
                Transaction::UserTransaction(trans) => {
                    /*
                    let trans = trans.check_signature()?;
                    if verify_admin_transaction && trans.is_admin_type() {
                        info!("executing admin trans");
                        // Check the voting power of signers in administrators.
                        let admins = self.validators.read();
                        if admins.is_none() {
                            bail!("Administrators are not set.");
                        }
                        let admins = admins.as_ref().unwrap();
                        let signers = trans.pubkey_account_addresses();
                        match admins.check_voting_power(signers.iter()) {
                            Ok(_) => {}
                            Err(VerifyError::TooLittleVotingPower {
                                    ..
                                }) => {
                                bail!("Not enough voting power in administrators.");
                            }
                            Err(_) => {
                                bail!(
                                    "There are signers not in administrators."
                                );
                            }
                        }
                    }
                    let payload = trans.payload();
                    let events = match payload {
                        TransactionPayload::WriteSet(write_set_payload) => {
                            match write_set_payload {
                                WriteSetPayload::Direct(change_set) => change_set.events().to_vec(),
                                _ => vec![]
                            }
                        }
                        _ => bail!("Wrong transaction payload"),
                    };

                    ensure!(
                        events.len() == 1,
                        "One transaction can contain exactly 1 event."
                    );

                    let output = Self::gen_output(events);
                    vm_outputs.push(output);
                     */
                }
                _ => {} /*
                        Transaction::WriteSet(change_set) => {
                            let events = change_set.events().to_vec();
                            ensure!(
                                events.len() == 1,
                                "One transaction can contain exactly 1 event."
                            );

                            let output = Self::gen_output(events);
                            vm_outputs.push(output);
                        }*/
            }
        }

        let status: Vec<_> = vm_outputs
            .iter()
            .map(TransactionOutput::status)
            .cloned()
            .collect();
        if !status.is_empty() {
            debug!("Execution status: {:?}", status);
        }

        let output = Self::process_vm_outputs(
            vm_outputs,
            last_pivot,
            current_membership_id,
        )
        .map_err(|err| format_err!("Failed to execute block: {}", err))?;

        Ok(output)
    }

    /// Saves eligible blocks to persistent storage.
    /// If we have multiple blocks and not all of them have signatures, we may
    /// send them to storage in a few batches. For example, if we have
    /// ```text
    /// A <- B <- C <- D <- E
    /// ```
    /// and only `C` and `E` have signatures, we will send `A`, `B` and `C` in
    /// the first batch, then `D` and `E` later in the another batch.
    /// Commits a block and all its ancestors in a batch manner. Returns
    /// `Ok(())` if successful.
    pub fn commit_blocks(
        &self, _blocks: Vec<(Vec<Transaction>, Arc<ProcessedVMOutput>)>,
        ledger_info_with_sigs: LedgerInfoWithSignatures,
    ) -> Result<()>
    {
        info!(
            "Received request to commit block {:x}, round {}.",
            ledger_info_with_sigs.ledger_info().consensus_block_id(),
            ledger_info_with_sigs.ledger_info().round(),
        );

        //self.db
        //    .save_ledger_info(&Some(ledger_info_with_sigs.clone()))?;
        Ok(())
    }

    pub fn ledger_info_committed(
        &self, ledger_info_with_sigs: &LedgerInfoWithSignatures,
    ) -> bool {
        false
        //self.db.ledger_info_exists(ledger_info_with_sigs)
    }

    pub fn get_membership_change_ledger_infos(
        &self, start_membership_id: u64, end_membership_id: u64,
    ) -> Result<(Vec<LedgerInfoWithSignatures>, bool)> {
        /*self.db.get_membership_change_ledger_infos(
            start_membership_id,
            end_membership_id,
        )*/
        Ok((vec![], false))
    }

    /// Post-processing of what the VM outputs. Returns the entire block's
    /// output.
    fn process_vm_outputs(
        vm_outputs: Vec<TransactionOutput>,
        last_pivot: Option<PivotBlockDecision>, current_membership_id: u64,
    ) -> Result<ProcessedVMOutput>
    {
        ensure!(
            vm_outputs.len() <= 1,
            "One block can have at most one transaction output!"
        );

        let mut next_validator_set = None;
        let mut next_pivot_block = last_pivot;
        let mut pivot_updated = false;

        for vm_output in vm_outputs.into_iter() {
            let validator_set_change_event_key =
                ValidatorSet::change_event_key();
            let pivot_select_event_key =
                PivotBlockDecision::pivot_select_event_key();
            for event in vm_output.events() {
                // check for change in validator set
                if *event.key() == validator_set_change_event_key {
                    let next_validator_set_proposal =
                        NextValidatorSetProposal::from_bytes(
                            event.event_data(),
                        )?;
                    ensure!(
                        current_membership_id
                            == next_validator_set_proposal.this_membership_id,
                        "Wrong membership_id proposal."
                    );
                    next_validator_set =
                        Some(next_validator_set_proposal.next_validator_set);
                    debug!(
                        "validator set change event: next_validator_set {:?}",
                        next_validator_set
                    );
                    break;
                }
                // check for pivot block selection.
                if *event.key() == pivot_select_event_key {
                    next_pivot_block = Some(PivotBlockDecision::from_bytes(
                        event.event_data(),
                    )?);
                    pivot_updated = true;
                    break;
                }
            }
        }

        Ok(ProcessedVMOutput::new(
            next_validator_set,
            next_pivot_block,
            pivot_updated,
        ))
    }
}