// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// Interface of Succinct's deployed SP1 verifiers (the `SP1VerifierGateway` routes to the
/// version a proof was made with). Same as sp1-contracts' `ISP1Verifier`.
interface ISP1Verifier {
    /// Reverts unless `proofBytes` proves that the program with `programVKey` ran and
    /// committed exactly `publicValues`.
    function verifyProof(bytes32 programVKey, bytes calldata publicValues, bytes calldata proofBytes) external view;
}

/// A verifier that names the circuit it checks (its hash's first 4 bytes prefix every proof).
/// Same as sp1-contracts' `ISP1VerifierWithHash`; SP1's generated verifier implements it.
interface ISP1VerifierWithHash is ISP1Verifier {
    /// The hash of the verifier's circuit.
    function VERIFIER_HASH() external pure returns (bytes32);
}
