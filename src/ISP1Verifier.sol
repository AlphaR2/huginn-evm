// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// Interface of Succinct's deployed SP1 verifiers (the `SP1VerifierGateway` routes to the
/// version a proof was made with). Same as sp1-contracts' `ISP1Verifier`.
interface ISP1Verifier {
    /// Reverts unless `proofBytes` proves that the program with `programVKey` ran and
    /// committed exactly `publicValues`.
    function verifyProof(bytes32 programVKey, bytes calldata publicValues, bytes calldata proofBytes) external view;
}
