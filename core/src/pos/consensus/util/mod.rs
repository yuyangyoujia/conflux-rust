// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

// Copyright 2021 Conflux Foundation. All rights reserved.
// Conflux is free software and distributed under GNU General Public License.
// See http://www.gnu.org/licenses/

use consensus_types::common::Round;
use diem_crypto::HashValue;
use diem_types::transaction::TransactionPayload;

pub mod config_subscription;
#[cfg(any(test, feature = "fuzzing"))]
pub mod mock_time_service;
pub mod time_service;

/// Test command sent by RPCs to construct attack cases.
#[derive(Debug)]
pub enum TestCommand {
    /// Make the node vote for the given proposal regardless of its consensus
    /// state. It will not vote if the proposal was not received.
    ForceVoteProposal(HashValue),
    /// Make the node propose a block with given round, parent, and payload.
    /// It will not propose if the parent does not have a valid QC.
    ForcePropose {
        /// Proposed block round.
        round: Round,
        /// Proposed block parent. A valid QC will be retrieved to match this
        /// parent.
        parent_id: HashValue,
        /// Payload for the proposed block. The PoW internal contract events
        /// will not be appended automatically.
        payload: Vec<TransactionPayload>,
    },
}
