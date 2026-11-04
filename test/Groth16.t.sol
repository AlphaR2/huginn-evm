// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SP1Verifier} from "../src/sp1/SP1VerifierGroth16.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {HuginnOutput} from "../src/HuginnOutput.sol";

/// 4.5 / 7.2: a real Groth16 proof from huginn, checked by SP1's real Solidity verifier
/// (src/sp1, circuit v6.1.0). The fixture is huginn's
/// `fixtures/proofs/real-1-validator-fast.groth16.json`; skipped if it isn't there.
contract Groth16Test is Test {
    string constant FIXTURE = "test/fixtures/real-1-validator-fast.groth16.json";
    uint16 constant SHRED_VERSION = 28090;
    bytes32 constant SET_0 = 0xb1f25236519c10b064498b6507ed0b87b88ad4df728a0a348e9434769d7d3c11;

    SP1Verifier verifier;
    bytes32 vkey;
    bytes pv;
    bytes proof;

    function setUp() public {
        if (!vm.isFile(FIXTURE)) return;
        string memory j = vm.readFile(FIXTURE);
        vkey = vm.parseJsonBytes32(j, ".program_vkey");
        pv = vm.parseBytes(string.concat("0x", vm.parseJsonString(j, ".public_values")));
        proof = vm.parseBytes(string.concat("0x", vm.parseJsonString(j, ".proof")));
        verifier = new SP1Verifier();
    }

    modifier withFixture() {
        vm.skip(!vm.isFile(FIXTURE));
        _;
    }

    function test_realProofVerifies() public withFixture {
        uint256 before = gasleft();
        verifier.verifyProof(vkey, pv, proof);
        emit log_named_uint("SP1 Groth16 verifyProof gas", before - gasleft());
        emit log_named_uint("proof bytes", proof.length);
        // The first 4 bytes name the circuit: this verifier's hash.
        assertEq(bytes4(proof), bytes4(verifier.VERIFIER_HASH()));
    }

    function test_lightClientAcceptsRealProof() public withFixture {
        SolanaLightClient client = new SolanaLightClient(verifier, vkey, SHRED_VERSION, 432000, 0, SET_0);
        uint256 before = gasleft();
        HuginnOutput.Output memory out = client.submit(pv, proof);
        emit log_named_uint("SolanaLightClient.submit gas, real Groth16", before - gasleft());
        assertEq(out.slot, 46718);
        assertEq(string(_tail(out.transaction, 25)), "huginn: hello from solana");
    }

    function test_changedOutputRejected() public withFixture {
        bytes memory forged = bytes.concat(pv);
        forged[forged.length - 1] ^= 0x01; // the memo's last character
        vm.expectRevert();
        verifier.verifyProof(vkey, forged, proof);
    }

    function test_changedProofRejected() public withFixture {
        bytes memory bad = bytes.concat(proof);
        bad[bad.length - 1] ^= 0x01;
        vm.expectRevert();
        verifier.verifyProof(vkey, pv, bad);
    }

    function test_otherProgramRejected() public withFixture {
        vm.expectRevert();
        verifier.verifyProof(bytes32(uint256(vkey) ^ 1), pv, proof);
    }

    function _tail(bytes memory b, uint256 n) internal pure returns (bytes memory out) {
        out = new bytes(n);
        for (uint256 i; i < n; i++) {
            out[i] = b[b.length - n + i];
        }
    }
}
