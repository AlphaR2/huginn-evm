// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {ISP1Verifier} from "../ISP1Verifier.sol";

/// NOT A VERIFIER. Stands in for SP1's on a local chain and in tests, until a Groth16 proof
/// exists. It binds a "proof" to (program key, public values) the way a real one does, so
/// changing either without "re-proving" fails, but anyone can make such a proof:
/// `proofFor(vkey, publicValues)`. Deploy.s.sol only deploys it on a local chain.
contract BindingVerifier is ISP1Verifier {
    error InvalidProof();

    function proofFor(bytes32 vkey, bytes memory pv) public pure returns (bytes memory) {
        return abi.encode(keccak256(abi.encode(vkey, pv)));
    }

    function verifyProof(bytes32 vkey, bytes calldata pv, bytes calldata proof) external pure {
        if (keccak256(proof) != keccak256(proofFor(vkey, pv))) revert InvalidProof();
    }
}
