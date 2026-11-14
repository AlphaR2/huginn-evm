// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {IValidatorSetSource} from "../src/sources/IValidatorSetSource.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";

/// The guardian can only pause or cancel; "verified" no longer depends on the slot number.
contract GuardianAndRecordTest is Test {
    uint16 constant SHRED_VERSION = 28090;
    uint64 constant SLOTS_PER_EPOCH = 432000;
    bytes32 constant SET_0 = 0xb1f25236519c10b064498b6507ed0b87b88ad4df728a0a348e9434769d7d3c11;
    address constant GUARDIAN = address(0x6a2d);
    address constant STRANGER = address(0xbad);

    BindingVerifier verifier;
    SolanaLightClient client;
    bytes32 vkey;
    bytes pv;
    bytes sig;

    function setUp() public {
        string memory proof = vm.readFile("test/fixtures/real-1-validator-fast.compressed.json");
        pv = vm.parseBytes(string.concat("0x", vm.parseJsonString(proof, ".public_values")));
        vkey = vm.parseJsonBytes32(proof, ".program_vkey");
        sig = vm.parseBytes(string.concat("0x", vm.parseJsonString(vm.readFile("test/fixtures/tx_46718.json"), ".signature")));
        verifier = new BindingVerifier();
        client = new SolanaLightClient(verifier, vkey, SHRED_VERSION, SLOTS_PER_EPOCH, 0, SET_0, 1 days, false, IValidatorSetSource(address(0)), 0);
        client.setGuardian(GUARDIAN);
    }

    function proofOf(bytes memory p) internal view returns (bytes memory) {
        return verifier.proofFor(vkey, p);
    }

    function pvAtSlot(uint64 slot) internal view returns (bytes memory p) {
        p = bytes.concat(pv);
        bytes8 s = bytes8(slot);
        for (uint256 i = 0; i < 8; i++) p[35 + i] = s[i];
    }

    // ---- guardian ----

    function test_guardianCanPauseAndOnlyOwnerResumes() public {
        vm.prank(GUARDIAN);
        client.pause();
        assertTrue(client.paused());
        bytes memory proof = proofOf(pv);
        vm.expectRevert(SolanaLightClient.ClientPaused.selector);
        client.verify(pv, proof);
        vm.expectRevert(SolanaLightClient.ClientPaused.selector);
        client.submit(pv, proof);

        vm.prank(GUARDIAN);
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.unpause();

        client.unpause(); // owner
        client.submit(pv, proof);
        assertTrue(client.isVerified(sig));
    }

    function test_guardianCanCancelAPendingSet() public {
        client.proposeValidatorSet(1, keccak256("bad set"));
        vm.prank(GUARDIAN);
        client.cancelValidatorSet(1);
        (bytes32 h,) = client.pendingSet(1);
        assertEq(h, bytes32(0));
    }

    function test_guardianCannotDoAnythingElse() public {
        vm.startPrank(GUARDIAN);
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.proposeValidatorSet(1, keccak256("x"));
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.setGuardian(GUARDIAN);
        vm.expectRevert(SolanaLightClient.NotOwner.selector);
        client.transferOwnership(GUARDIAN);
        vm.stopPrank();
        assertEq(client.owner(), address(this));
    }

    function test_strangerCannotPauseOrCancel() public {
        client.proposeValidatorSet(1, keccak256("x"));
        vm.startPrank(STRANGER);
        vm.expectRevert(SolanaLightClient.NotOwnerOrGuardian.selector);
        client.pause();
        vm.expectRevert(SolanaLightClient.NotOwnerOrGuardian.selector);
        client.cancelValidatorSet(1);
        vm.stopPrank();
    }

    function test_noGuardianMeansZeroAddressIsNotOne() public {
        client.setGuardian(address(0));
        vm.prank(address(0));
        vm.expectRevert(SolanaLightClient.NotOwnerOrGuardian.selector);
        client.pause();
    }

    // ---- verified flag ----

    function test_slotZeroTransactionCountsAsVerified() public {
        client.submit(pvAtSlot(0), proofOf(pvAtSlot(0)));
        assertTrue(client.isVerified(sig), "slot 0 is a valid slot");
        assertEq(client.transactionSlot(keccak256(sig)), 0);
    }

    function test_resubmitKeepsTheFirstRecord() public {
        client.submit(pv, proofOf(pv));
        uint64 first = client.transactionSlot(keccak256(sig));
        client.submit(pv, proofOf(pv)); // same slot: fine, nothing changes
        assertEq(client.transactionSlot(keccak256(sig)), first);
    }

    function test_sameTransactionInAnotherSlotIsRefusedNotOverwritten() public {
        client.submit(pv, proofOf(pv));
        uint64 first = client.transactionSlot(keccak256(sig));
        bytes memory other = pvAtSlot(first + 1);
        bytes memory proof = proofOf(other);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.ConflictingTransaction.selector, keccak256(sig), first, first + 1));
        client.submit(other, proof);
        assertEq(client.transactionSlot(keccak256(sig)), first);
    }

    // ---- W1: a record under a replaced set must not block the real proof ----

    function pvUnder(bytes32 setHash, uint64 slot) internal view returns (bytes memory p) {
        p = pvAtSlot(slot);
        for (uint256 i = 0; i < 32; i++) p[1 + i] = setHash[i];
    }

    function test_recordUnderReplacedSetDoesNotBlockTheRealProof() public {
        // A proof recorded under set A (SET_0) at slot X ...
        client.submit(pv, proofOf(pv));
        uint64 x = client.transactionSlot(keccak256(sig));
        uint64 epoch = client.epochOf(x);
        // ... then A turns out to be wrong and is replaced by B for that epoch.
        bytes32 setB = keccak256("the real set");
        client.proposeValidatorSet(epoch, setB);
        vm.warp(block.timestamp + 1 days);
        assertEq(client.validatorSetHash(epoch), setB);
        // The old record no longer counts ...
        assertFalse(client.isVerified(sig), "record under a replaced set must not count");
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.NotVerified.selector, keccak256(sig)));
        client.transactionSlot(keccak256(sig));
        // ... and the real proof under B, in another slot of the same epoch, goes through.
        uint64 y = x + 1;
        bytes memory real = pvUnder(setB, y);
        client.submit(real, proofOf(real));
        assertTrue(client.isVerified(sig));
        assertEq(client.transactionSlot(keccak256(sig)), y);
        // Under B the same transaction in a third slot is still refused.
        bytes memory third = pvUnder(setB, y + 1);
        bytes memory proof3 = proofOf(third);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.ConflictingTransaction.selector, keccak256(sig), y, y + 1));
        client.submit(third, proof3);
    }

    // ---- F6: epoch math follows Solana's EpochSchedule, warmup included ----

    function test_epochOfWithWarmupMatchesSolana() public {
        // slots_per_epoch 8192 with warmup: epochs of 32, 64, ..., first normal epoch 8 at 8160.
        SolanaLightClient w = new SolanaLightClient(verifier, vkey, SHRED_VERSION, 8192, 0, SET_0, 0, true, IValidatorSetSource(address(0)), 0);
        assertEq(w.firstNormalEpoch(), 8);
        assertEq(w.firstNormalSlot(), 8160);
        assertEq(w.epochOf(0), 0);
        assertEq(w.epochOf(31), 0);
        assertEq(w.epochOf(32), 1);
        assertEq(w.epochOf(95), 1);
        assertEq(w.epochOf(96), 2);
        assertEq(w.epochOf(8159), 7);
        assertEq(w.epochOf(8160), 8);
        assertEq(w.epochOf(8160 + 8192), 9);
        // No warmup (mainnet, devnet): plain division.
        assertEq(client.firstNormalSlot(), 0);
        assertEq(client.epochOf(521_088_299), 1206);
    }
}
