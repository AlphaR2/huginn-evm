// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {HuginnOutput} from "../src/HuginnOutput.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";

contract Decoder {
    function decode(bytes calldata pv) external pure returns (HuginnOutput.Output memory) {
        return HuginnOutput.decode(pv);
    }
}

contract SolanaLightClientTest is Test {
    // fixtures/local: the one-validator local Alpenglow chain, epoch 0.
    uint16 constant SHRED_VERSION = 28090;
    uint64 constant SLOTS_PER_EPOCH = 432000;
    bytes32 constant SET_0 = 0xb1f25236519c10b064498b6507ed0b87b88ad4df728a0a348e9434769d7d3c11;
    uint64 constant SLOT = 46718;

    BindingVerifier verifier;
    SolanaLightClient client;
    bytes32 vkey;
    bytes pv;
    bytes sig;

    function setUp() public {
        string memory proof = vm.readFile("test/fixtures/real-1-validator-fast.compressed.json");
        pv = vm.parseBytes(string.concat("0x", vm.parseJsonString(proof, ".public_values")));
        vkey = vm.parseJsonBytes32(proof, ".program_vkey");
        string memory tx_ = vm.readFile("test/fixtures/tx_46718.json");
        sig = vm.parseBytes(string.concat("0x", vm.parseJsonString(tx_, ".signature")));

        verifier = new BindingVerifier();
        client = new SolanaLightClient(verifier, vkey, SHRED_VERSION, SLOTS_PER_EPOCH, 0, SET_0, 0);
    }

    function proofOf(bytes memory pv_) internal view returns (bytes memory) {
        return verifier.proofFor(vkey, pv_);
    }

    function test_realOutputDecodes() public {
        HuginnOutput.Output memory o = new Decoder().decode(pv);
        assertEq(o.validatorSetHash, SET_0);
        assertEq(o.shredVersion, SHRED_VERSION);
        assertEq(o.slot, SLOT);
        assertFalse(o.slowPath);
        assertEq(o.signedStake, o.totalStake, "one validator signs everything");
        assertEq(o.transaction.length, 275);
    }

    function test_submitRecordsBlockAndTransaction() public {
        assertFalse(client.isVerified(sig));
        HuginnOutput.Output memory o = client.submit(pv, proofOf(pv));
        assertEq(client.finalizedBlock(SLOT, client.validatorSetHash(client.epochOf(SLOT))), o.blockId);
        assertTrue(client.isVerified(sig));
        assertEq(client.transactionSlot(keccak256(sig)), SLOT);
        // Submitting the same transaction again is harmless.
        client.submit(pv, proofOf(pv));
    }

    function test_gas_submit() public {
        bytes memory proof = proofOf(pv);
        uint256 before = gasleft();
        client.submit(pv, proof);
        // Everything but the SP1 verifier itself (Groth16 adds ~270k).
        emit log_named_uint("submit gas, mock verifier", before - gasleft());
    }

    function test_rejectsChangedPublicValues() public {
        bytes memory proof = proofOf(pv);
        bytes memory forged = bytes.concat(pv);
        forged[forged.length - 1] ^= 0x01; // one byte of the memo
        vm.expectRevert(BindingVerifier.InvalidProof.selector);
        client.verify(forged, proof);
    }

    function test_rejectsProofOfAnotherProgram() public {
        SolanaLightClient other =
            new SolanaLightClient(verifier, keccak256("other program"), SHRED_VERSION, SLOTS_PER_EPOCH, 0, SET_0, 0);
        bytes memory proof = proofOf(pv);
        vm.expectRevert(BindingVerifier.InvalidProof.selector);
        other.verify(pv, proof);
    }

    function test_rejectsOtherCluster() public {
        SolanaLightClient mainnet = new SolanaLightClient(verifier, vkey, 50093, SLOTS_PER_EPOCH, 0, SET_0, 0);
        bytes memory proof = proofOf(pv);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.WrongShredVersion.selector, SHRED_VERSION));
        mainnet.verify(pv, proof);
    }

    function test_rejectsUnknownEpoch() public {
        SolanaLightClient c = new SolanaLightClient(verifier, vkey, SHRED_VERSION, SLOTS_PER_EPOCH, 1, SET_0, 0);
        bytes memory proof = proofOf(pv);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.UnknownEpoch.selector, 0));
        c.verify(pv, proof);
    }

    function test_rejectsWrongValidatorSet() public {
        bytes32 otherSet = keccak256("another set");
        SolanaLightClient c = new SolanaLightClient(verifier, vkey, SHRED_VERSION, SLOTS_PER_EPOCH, 0, otherSet, 0);
        bytes memory proof = proofOf(pv);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.WrongValidatorSet.selector, SET_0, otherSet));
        c.verify(pv, proof);
    }

    function test_rejectsConflictingBlock() public {
        client.submit(pv, proofOf(pv));
        bytes memory other = bytes.concat(pv);
        other[43] ^= 0x01; // block_id
        bytes32 stored = client.finalizedBlock(SLOT, client.validatorSetHash(client.epochOf(SLOT)));
        bytes32 got = bytes32(stored ^ bytes32(uint256(1) << 248));
        bytes memory proof = proofOf(other);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.ConflictingBlock.selector, SLOT, stored, got));
        client.submit(other, proof);
    }

    function test_onlyOwnerProposesSets() public {
        vm.prank(address(0xbad));
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.proposeValidatorSet(1, keccak256("set 1"));
        vm.expectRevert(SolanaLightClient.ZeroSet.selector);
        client.proposeValidatorSet(1, bytes32(0));
    }

    function test_proposedSetCountsOnlyAfterTheDelay() public {
        SolanaLightClient c = new SolanaLightClient(verifier, vkey, SHRED_VERSION, SLOTS_PER_EPOCH, 0, SET_0, 1 days);
        bytes32 set1 = keccak256("set 1");
        c.proposeValidatorSet(1, set1);
        (bytes32 pending, uint64 activeAt) = c.pendingSet(1);
        assertEq(pending, set1);
        assertEq(activeAt, block.timestamp + 1 days);
        assertEq(c.validatorSetHash(1), bytes32(0), "not yet");
        vm.warp(activeAt - 1);
        assertEq(c.validatorSetHash(1), bytes32(0), "one second early");
        vm.warp(activeAt);
        assertEq(c.validatorSetHash(1), set1);
        c.settleValidatorSet(1);
        assertEq(c.validatorSetHash(1), set1);
        (pending,) = c.pendingSet(1);
        assertEq(pending, bytes32(0), "settled into the set in force");
    }

    function test_typoIsReplacedOrCancelledBeforeItCounts() public {
        SolanaLightClient c = new SolanaLightClient(verifier, vkey, SHRED_VERSION, SLOTS_PER_EPOCH, 0, SET_0, 1 days);
        c.proposeValidatorSet(1, keccak256("typo"));
        vm.warp(block.timestamp + 12 hours);
        c.proposeValidatorSet(1, keccak256("right")); // replaces it, the wait restarts
        vm.warp(block.timestamp + 1 days);
        assertEq(c.validatorSetHash(1), keccak256("right"));

        c.proposeValidatorSet(2, keccak256("not needed"));
        c.cancelValidatorSet(2);
        vm.warp(block.timestamp + 2 days);
        assertEq(c.validatorSetHash(2), bytes32(0));
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.NothingPending.selector, uint64(2)));
        c.cancelValidatorSet(2);
    }

    function test_setInForceIsSupersededOnlyAfterTheDelay() public {
        SolanaLightClient c = new SolanaLightClient(verifier, vkey, SHRED_VERSION, SLOTS_PER_EPOCH, 0, SET_0, 1 days);
        c.verify(pv, proofOf(pv));
        bytes32 newSet = keccak256("epoch 0, corrected");
        c.proposeValidatorSet(0, newSet);
        // The old set stays in force while the new one waits.
        assertEq(c.validatorSetHash(0), SET_0);
        c.verify(pv, proofOf(pv));
        vm.warp(block.timestamp + 1 days);
        assertEq(c.validatorSetHash(0), newSet);
        bytes memory proof = proofOf(pv);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.WrongValidatorSet.selector, SET_0, newSet));
        c.verify(pv, proof);
        // A matured proposal can no longer be cancelled: it is in force.
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.NothingPending.selector, uint64(0)));
        c.cancelValidatorSet(0);
    }

    function test_ownershipMovesInTwoSteps() public {
        address next = address(0xbeef);
        client.transferOwnership(address(0xdead)); // a typo: nothing changes yet
        assertEq(client.owner(), address(this));
        client.transferOwnership(next); // corrected
        vm.prank(address(0xdead));
        vm.expectRevert(SolanaLightClient.NotPendingOwner.selector);
        client.acceptOwnership();
        vm.prank(next);
        client.acceptOwnership();
        assertEq(client.owner(), next);
        assertEq(client.pendingOwner(), address(0));
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.proposeValidatorSet(1, keccak256("set 1"));
    }

    function test_constructorRefusesZeroSet() public {
        vm.expectRevert(SolanaLightClient.ZeroSet.selector);
        new SolanaLightClient(verifier, vkey, SHRED_VERSION, SLOTS_PER_EPOCH, 0, bytes32(0), 0);
    }

    function test_decodeRejectsBadFraming() public {
        Decoder d = new Decoder();
        bytes memory v2 = bytes.concat(pv);
        v2[0] = 0x02;
        vm.expectRevert(abi.encodeWithSelector(HuginnOutput.BadVersion.selector, 2));
        d.decode(v2);

        bytes memory extra = bytes.concat(pv, hex"00");
        vm.expectRevert(abi.encodeWithSelector(HuginnOutput.BadLength.selector, extra.length));
        d.decode(extra);

        vm.expectRevert(abi.encodeWithSelector(HuginnOutput.BadLength.selector, 105));
        d.decode(new bytes(105));
    }
}
