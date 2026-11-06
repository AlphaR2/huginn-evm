// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {SolanaLightClient} from "../src/SolanaLightClient.sol";
import {SolanaControlled} from "../src/SolanaControlled.sol";
import {SolanaGreeter} from "../src/examples/SolanaGreeter.sol";
import {BindingVerifier} from "../src/mocks/BindingVerifier.sol";

/// Adversarial proofs-of-concept from an outside review (docs/adversarial-report.md in huginn).
///
/// `test_POC_*` PASSES while the weakness is present (it asserts the bad outcome).
/// `test_FIXED_*` is a former PoC flipped after the fix: it asserts the attack now fails.
///
/// The stand-in verifier lets the test mint a "proof" for any public values. That is the
/// same power a real prover has over values it can legitimately prove, so each test only
/// builds public values a real prover could obtain (a real Solana transaction in a real
/// finalized block), except `ownerKeyStillForges...`, which uses the owner's trust power.
contract AdversarialTest is Test {
    uint16 constant SHRED_VERSION = 28090;
    bytes32 constant SET_0 = 0xb1f25236519c10b064498b6507ed0b87b88ad4df728a0a348e9434769d7d3c11;
    bytes32 constant MEMO = 0x054a53522854cdd103cc2e4631a3f51792ac727cf1e50abc97828ac5994f3cc2;
    bytes32 constant VKEY = bytes32(uint256(0x000482e7));
    bytes32 constant CONTROLLER = keccak256("the controller's Solana key");

    BindingVerifier verifier;
    SolanaLightClient client;
    SolanaGreeter greeter;

    function setUp() public {
        verifier = new BindingVerifier();
        client = new SolanaLightClient(verifier, VKEY, SHRED_VERSION, 432000, 0, SET_0, 0);
        greeter = new SolanaGreeter(client, CONTROLLER);
    }

    // ---- F1: ordering, expiry, deployment binding (FIXED: nonce + address in the memo) ----

    /// The controller signs "set A" (nonce 1) and then "set B" (nonce 2). Relaying them out
    /// of order no longer lets the stale command win.
    function test_FIXED_commandsCannotBeRelayedOutOfOrder() public {
        bytes memory first = _memoTx(CONTROLLER, 1, _memo(greeter, 1, "A (old, superseded)"));
        bytes memory second = _memoTx(CONTROLLER, 2, _memo(greeter, 2, "B (latest intent)"));
        _expectRevert(
            greeter, second, 50_001, abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(2), uint64(1))
        );
        _execute(greeter, first, 50_000);
        _execute(greeter, second, 50_001);
        _expectRevert(
            greeter, first, 50_000, abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(1), uint64(3))
        );
        assertEq(greeter.greeting(), "B (latest intent)");
    }

    /// A command signed for one deployment does not run on a redeploy: the memo names the
    /// contract's address.
    function test_FIXED_oldCommandDoesNotReplayOnANewDeployment() public {
        bytes memory old = _memoTx(CONTROLLER, 1, _memo(greeter, 1, "sign me once"));
        _execute(greeter, old, 50_000);

        SolanaGreeter v2 = new SolanaGreeter(client, CONTROLLER);
        _expectRevert(v2, old, 50_000, abi.encodeWithSelector(SolanaControlled.NoCommand.selector));
        assertEq(v2.greeting(), "gm");
    }

    /// Still no time-based expiry, but a dormant command can now be cancelled: the
    /// controller relays an empty command with the same nonce first.
    function test_FIXED_unrelayedCommandCanBeCancelled() public {
        bytes memory dormant = _memoTx(CONTROLLER, 1, _memo(greeter, 1, "dormant"));
        _execute(greeter, _memoTx(CONTROLLER, 2, _memo(greeter, 1, "")), 50_001);
        vm.warp(block.timestamp + 5 * 365 days);
        _expectRevert(
            greeter, dormant, 50_000, abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(1), uint64(2))
        );
        assertEq(greeter.greeting(), "gm");
    }

    /// Two signatures over the same message: the second is refused, because the guard is the
    /// nonce inside the signed message, not the signature bytes.
    function test_FIXED_replayGuardIsTheSignedNonce() public {
        bytes memory m = _memo(greeter, 1, "same message");
        _execute(greeter, _memoTx(CONTROLLER, 1, m), 50_000);
        _expectRevert(
            greeter,
            _memoTx(CONTROLLER, 2, m),
            50_000,
            abi.encodeWithSelector(SolanaControlled.WrongNonce.selector, uint64(1), uint64(2))
        );
    }

    // ---- F2: the validator-set owner is the root of trust ----------------------------------

    /// STILL OPEN (real fix: prove the set handover in SP1). The owner can propose a set whose
    /// keys it controls and then prove anything. What changed: the fake set waits a public
    /// delay (`ValidatorSetProposed`, `pendingSet`) before it counts, so watchers and
    /// consumers have that long to see it coming.
    function test_POC_ownerKeyStillForges_butOnlyAfterAPublicDelay() public {
        SolanaLightClient slow = new SolanaLightClient(verifier, VKEY, SHRED_VERSION, 432000, 0, SET_0, 1 days);
        SolanaGreeter g = new SolanaGreeter(slow, CONTROLLER);
        bytes32 evilSet = keccak256("attacker's own validator set");
        slow.proposeValidatorSet(1, evilSet); // this test contract is the owner
        (bytes32 visible,) = slow.pendingSet(1);
        assertEq(visible, evilSet, "anyone can see the pending set");

        bytes memory forged = _memoTx(CONTROLLER, 9, _memo(g, 1, "owned by whoever holds the owner key"));
        bytes memory pv = _pv(forged, 432_001, evilSet, keccak256("a block that never existed"));
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(abi.encodeWithSelector(SolanaLightClient.UnknownEpoch.selector, uint64(1)));
        g.execute(pv, proof);

        vm.warp(block.timestamp + 1 days);
        g.execute(pv, proof);
        assertEq(g.greeting(), "owned by whoever holds the owner key");
    }

    /// FIXED. A wrong set no longer bricks an epoch: replaced before it counts, or superseded
    /// (after the same delay) once it does.
    function test_FIXED_wrongSetForAnEpochCanBeCorrected() public {
        bytes32 typo = keccak256("typo");
        bytes32 realSet1 = keccak256("the correct epoch 1 set");
        client.proposeValidatorSet(1, typo);
        client.proposeValidatorSet(1, realSet1); // this client's delay is 0: supersedes at once

        bytes memory tx_ = _memoTx(CONTROLLER, 1, _memo(greeter, 1, "x"));
        bytes memory pv = _pv(tx_, 432_001, realSet1, keccak256("real block"));
        client.verify(pv, verifier.proofFor(VKEY, pv));
    }

    /// FIXED. Ownership moves in two steps, so a mistyped address strands nothing.
    function test_FIXED_ownershipTransferNeedsTheNewOwnerToAccept() public {
        client.transferOwnership(address(0xdead)); // the typo
        client.proposeValidatorSet(1, keccak256("next epoch")); // still the owner
        assertEq(client.owner(), address(this));
    }

    // ---- F7: `verify` ignored the conflicting-block record (FIXED) ----------------------

    /// After block A is submitted for slot 50,000, a proof about a different block B at the
    /// same slot is refused by `verify` too, so applications get the same protection.
    function test_FIXED_applicationsGetTheConflictingBlockCheck() public {
        bytes memory t1 = _memoTx(CONTROLLER, 1, _memo(greeter, 1, "on block A"));
        bytes memory pv1 = _pv(t1, 50_000, SET_0, keccak256("block A"));
        client.submit(pv1, verifier.proofFor(VKEY, pv1));

        bytes memory t2 = _memoTx(CONTROLLER, 2, _memo(greeter, 1, "on block B"));
        bytes memory pv2 = _pv(t2, 50_000, SET_0, keccak256("block B"));
        bytes memory proof2 = verifier.proofFor(VKEY, pv2);
        bytes memory conflict = abi.encodeWithSelector(
            SolanaLightClient.ConflictingBlock.selector, uint64(50_000), keccak256("block A"), keccak256("block B")
        );
        vm.expectRevert(conflict);
        client.submit(pv2, proof2);
        vm.expectRevert(conflict);
        greeter.execute(pv2, proof2);
        assertEq(greeter.greeting(), "gm");
    }

    // ---- building blocks (same layout as SolanaGreeter.t.sol) ------------------------------

    function _execute(SolanaGreeter g, bytes memory tx_, uint64 slot) internal {
        bytes memory pv = _pv(tx_, slot, SET_0, keccak256(abi.encodePacked("block", slot)));
        g.execute(pv, verifier.proofFor(VKEY, pv));
    }

    function _expectRevert(SolanaGreeter g, bytes memory tx_, uint64 slot, bytes memory err) internal {
        bytes memory pv = _pv(tx_, slot, SET_0, keccak256(abi.encodePacked("block", slot)));
        bytes memory proof = verifier.proofFor(VKEY, pv);
        vm.expectRevert(err);
        g.execute(pv, proof);
    }

    function _memo(SolanaGreeter g, uint256 nonce, string memory command) internal view returns (bytes memory) {
        return abi.encodePacked(g.memoPrefix(), vm.toString(nonce), ":", command);
    }

    /// Legacy transaction: 1 signature, keys [signer, Memo program], one Memo instruction.
    /// `sigSeed` only changes the signature bytes, never the message.
    function _memoTx(bytes32 signer, uint8 sigSeed, bytes memory memo) internal pure returns (bytes memory) {
        bytes memory sig = abi.encodePacked(bytes32(uint256(sigSeed)), bytes32(0));
        return bytes.concat(
            hex"01",
            sig,
            hex"010001",
            hex"02",
            signer,
            MEMO,
            keccak256("blockhash"),
            hex"01",
            hex"01",
            hex"0100",
            _compactU16(memo.length),
            memo
        );
    }

    function _compactU16(uint256 n) internal pure returns (bytes memory) {
        if (n < 0x80) return abi.encodePacked(uint8(n));
        if (n < 0x4000) return abi.encodePacked(uint8(n & 0x7f) | 0x80, uint8(n >> 7));
        return abi.encodePacked(uint8(n & 0x7f) | 0x80, uint8((n >> 7) & 0x7f) | 0x80, uint8(n >> 14));
    }

    function _pv(bytes memory tx_, uint64 slot, bytes32 setHash, bytes32 blockId) internal pure returns (bytes memory) {
        return bytes.concat(
            hex"01",
            setHash,
            bytes2(SHRED_VERSION),
            bytes8(slot),
            blockId,
            hex"00",
            bytes8(uint64(1000)),
            bytes8(uint64(1000)),
            bytes4(0),
            bytes4(0),
            bytes4(0),
            bytes2(uint16(tx_.length)),
            tx_
        );
    }
}
