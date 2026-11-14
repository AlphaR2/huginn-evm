// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {IValidatorSetSource} from "../src/sources/IValidatorSetSource.sol";
import {RecorderSetSource} from "../src/sources/RecorderSetSource.sol";
import {EpochStakesSetSource} from "../src/sources/EpochStakesSetSource.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";

/// Stand-in for a real source: "evidence" is abi.encode(prevSetHash, newSetHash) and is accepted
/// only if it names the set the client trusts for the previous epoch.
contract MockSetSource is IValidatorSetSource {
    error NotCertifiedByPrev();

    function verifyNextSet(uint64, bytes32 trustedPrev, bytes calldata evidence) external pure returns (bytes32) {
        (bytes32 prev, bytes32 next) = abi.decode(evidence, (bytes32, bytes32));
        if (prev != trustedPrev) revert NotCertifiedByPrev();
        return next;
    }
}

/// W8 (one set-source interface) and W4 (recovery when an epoch is missed).
contract HandoverTest is Test {
    uint16 constant SHRED_VERSION = 28090;
    uint64 constant SPE = 432000;
    bytes32 constant SET_0 = keccak256("set 0");
    bytes32 constant SET_1 = keccak256("set 1");
    bytes32 constant SET_2 = keccak256("set 2");
    address constant GUARDIAN = address(0x6a2d);

    SolanaLightClient client;
    MockSetSource source;
    bytes32 vkey = keccak256("program");

    function setUp() public {
        source = new MockSetSource();
        client = new SolanaLightClient(new BindingVerifier(), vkey, SHRED_VERSION, SPE, 0, SET_0, 2 hours, false, source, 7 days);
        client.setGuardian(GUARDIAN);
    }

    function ev(bytes32 prev, bytes32 next) internal pure returns (bytes memory) {
        return abi.encode(prev, next);
    }

    function test_provenSetInstallsAtOnceAndChains() public {
        vm.prank(address(0xa11ce)); // anyone
        client.installProvenSet(1, ev(SET_0, SET_1));
        assertEq(client.validatorSetHash(1), SET_1, "counts at once");
        client.installProvenSet(2, ev(SET_1, SET_2));
        assertEq(client.validatorSetHash(2), SET_2);
    }

    function test_evidenceMustBeCertifiedByTheTrustedPreviousSet() public {
        vm.expectRevert(MockSetSource.NotCertifiedByPrev.selector);
        client.installProvenSet(1, ev(keccak256("forged prev"), SET_1));
    }

    function test_provenSetNeverOverridesADifferentSetInForce() public {
        client.installProvenSet(1, ev(SET_0, SET_1));
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.ConflictingSet.selector, uint64(1), SET_1, SET_2));
        client.installProvenSet(1, ev(SET_0, SET_2));
        client.installProvenSet(1, ev(SET_0, SET_1)); // same set again: harmless
    }

    function test_provenSetReplacesAPendingOwnerProposal() public {
        client.proposeValidatorSet(1, keccak256("owner typo"));
        client.installProvenSet(1, ev(SET_0, SET_1));
        (bytes32 pending,) = client.pendingSet(1);
        assertEq(pending, bytes32(0));
        vm.warp(block.timestamp + 30 days);
        assertEq(client.validatorSetHash(1), SET_1);
    }

    function test_noSourceMeansNoProvenInstall() public {
        SolanaLightClient plain =
            new SolanaLightClient(new BindingVerifier(), vkey, SHRED_VERSION, SPE, 0, SET_0, 2 hours, false, IValidatorSetSource(address(0)), 0);
        vm.expectRevert(SolanaLightClient.NoSetSource.selector);
        plain.installProvenSet(1, ev(SET_0, SET_1));
    }

    function test_reanchorDelayMustExceedSetDelay() public {
        BindingVerifier v = new BindingVerifier();
        vm.expectRevert(SolanaLightClient.ReanchorTooShort.selector);
        new SolanaLightClient(v, vkey, SHRED_VERSION, SPE, 0, SET_0, 2 hours, false, source, 1 hours);
    }

    /// W4: epoch 1 is never installed. The proven chain is broken (epoch 2 needs epoch 1), so
    /// the client refuses epoch 2 until the owner re-anchors, which takes the long delay, is
    /// announced, and can be cancelled by the guardian.
    function test_missedEpochRefusesThenReanchorWithDelayAndCancel() public {
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.UnknownEpoch.selector, uint64(1)));
        client.installProvenSet(2, ev(SET_1, SET_2));
        assertEq(client.validatorSetHash(2), bytes32(0), "nothing for epoch 2: proofs there are refused");

        // A bad re-anchor is caught and cancelled by the guardian.
        vm.expectEmit(true, false, false, true);
        emit SolanaLightClient.ValidatorSetReanchorProposed(2, keccak256("bad"), uint64(block.timestamp + 7 days));
        client.proposeValidatorSet(2, keccak256("bad"));
        vm.prank(GUARDIAN);
        client.cancelValidatorSet(2);

        // The real re-anchor waits the full 7 days, not the 2-hour set delay.
        client.proposeValidatorSet(2, SET_2);
        vm.warp(block.timestamp + 2 hours);
        assertEq(client.validatorSetHash(2), bytes32(0), "not after setDelay");
        vm.warp(block.timestamp + 7 days);
        assertEq(client.validatorSetHash(2), SET_2, "after reanchorDelay");

        // The proven chain continues from the re-anchored set.
        client.installProvenSet(3, ev(SET_2, keccak256("set 3")));
        assertEq(client.validatorSetHash(3), keccak256("set 3"));
    }

    function test_pausedClientInstallsNothing() public {
        vm.prank(GUARDIAN);
        client.pause();
        vm.expectRevert(SolanaLightClient.ClientPaused.selector);
        client.installProvenSet(1, ev(SET_0, SET_1));
    }

    function test_unbuiltSourcesRefuseEverything() public {
        IValidatorSetSource[2] memory s = [IValidatorSetSource(new RecorderSetSource()), IValidatorSetSource(new EpochStakesSetSource())];
        for (uint256 i = 0; i < 2; i++) {
            SolanaLightClient c = new SolanaLightClient(new BindingVerifier(), vkey, SHRED_VERSION, SPE, 0, SET_0, 2 hours, false, s[i], 7 days);
            bytes memory e = ev(SET_0, SET_1);
            vm.expectRevert(bytes4(keccak256("NotBuilt()")));
            c.installProvenSet(1, e);
        }
    }
}
