// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IValidatorSetSource} from "./IValidatorSetSource.sol";

/// Source B: SIMD-0676 "On-Chain Epoch Stakes". The account for epoch E-1, written in E-1's
/// first block (certified by E-1's set), holds E's set. Layout: quests/q12-simd-0676; parser:
/// huginn `crates/handover`. NOT BUILT: the proposal is not merged or activated.
contract EpochStakesSetSource is IValidatorSetSource {
    error NotBuilt();

    function verifyNextSet(uint64, bytes32, bytes calldata) external pure returns (bytes32) {
        revert NotBuilt();
    }
}
