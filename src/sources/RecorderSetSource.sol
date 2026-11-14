// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IValidatorSetSource} from "./IValidatorSetSource.sol";

/// Source A: recorder account (frozen stakes via `sol_get_epoch_stake`) plus each member's BLS
/// key from the epoch-boundary block, proven through the footer bank hash and lattice-hash
/// delta; overlap rule and growth cap as in huginn `docs/security.md`. NOT BUILT: waits on
/// quests Q4/Q5 and the delta hardening (W2). Deploying it today makes every install revert.
contract RecorderSetSource is IValidatorSetSource {
    error NotBuilt();

    function verifyNextSet(uint64, bytes32, bytes calldata) external pure returns (bytes32) {
        revert NotBuilt();
    }
}
